"""Thin typed wrapper over the ``herdr`` CLI's socket-API subcommands.

Why the CLI and not the socket directly: ``herdr`` already speaks its own protocol version
(``protocol: 17`` at time of writing) and ships compatibility checks. Re-implementing the wire
format here would create a second, silently-drifting client.

**The one thing that must not be got wrong**: the Herdr SERVER is what determines whether panes are
inside or outside the agent sandbox, because panes are its children. If a confined agent starts the
server itself, every pane it creates inherits that confinement and the whole tool becomes an
elaborate way to get the same 403 — silently, because nothing about the pane would look different.
So :meth:`HerdrClient.ensure_server` always launches through ``systemd-run --user``, which reparents
the server onto the user manager outside the jail. Control CALLS do not need that (the server's unix
socket is reachable from inside; measured on devbig014 2026-08-06), and default to going direct.

Every executable and the account home are resolved lazily, at the first call that needs them, in
one fixed order (Herdr, then ``systemd-run``, then the account home), so a subcommand that never
calls Herdr never depends on its installation, and both editions report the same missing piece
first. Every message here is worded exactly as the native edition words it.
"""

from __future__ import annotations

import os
import pwd
import signal
import stat
import subprocess
import time
from collections.abc import Callable, Iterable, Mapping, Sequence
from dataclasses import dataclass

from herdr_run.errors import HerdrUnavailable
from herdr_run.textfmt import display, os_error_text, quote, strict_json_loads, strip_space

__all__ = [
    "CONTROL_TIMEOUT_SECONDS",
    "HerdrClient",
    "Pane",
    "ProcessInfo",
    "Runner",
    "SERVER_UNIT",
    "SERVER_WORKER_THREADS",
    "SYSTEMD_RUN_CANDIDATES",
    "account_home",
    "bounded_control_command",
    "child_environment",
    "environment_entries",
]

#: A ``subprocess.run``-shaped callable, injected so tests never spawn a real process.
Runner = Callable[[Sequence[str]], "subprocess.CompletedProcess[str]"]

#: Transient user unit that owns an auto-started server. Named so a stray one is identifiable, and
#: so repeated starts collide on the unit name instead of racing up several servers.
SERVER_UNIT = "herdr-run-server"

#: Maximum/avoidance bound for one Herdr CLI or systemd control call. Command execution itself has
#: its separate configurable timeout in the pane runner.
CONTROL_TIMEOUT_SECONDS = 30.0

#: Tokio runtime workers for the server. Herdr is a static musl binary, and musl's mallocng has one
#: global allocator lock; tokio's default of one worker per core turns that lock into a convoy on a
#: many-core host. Measured on devbig014 (316 cores, 260 panes) 2026-08-10: uncapped the server sat
#: at >1000% CPU with ~98% of cycles in the kernel's futex spinlock and every control call timing
#: out; capped at 16 the same session served ``pane list`` in 0.03s. This bounds only the runtime
#: workers -- the per-pane blocking threads are separate and still scale with pane count.
SERVER_WORKER_THREADS = 16

#: Fixed locations searched for ``systemd-run``, in order.
SYSTEMD_RUN_CANDIDATES: tuple[str, ...] = ("/usr/bin/systemd-run", "/bin/systemd-run")

# Linux exposes process IDs through the positive range of its signed ``pid_t``.
_MAX_PROCESS_ID = 2_147_483_647
_I64_MIN = -(2**63)
_I64_MAX = 2**63 - 1


@dataclass(frozen=True)
class Pane:
    """One terminal pane and the tab/workspace it belongs to."""

    pane_id: str
    tab_id: str
    workspace_id: str


@dataclass(frozen=True)
class ProcessInfo:
    """A pane's live foreground-process state, as reported by ``herdr pane process-info``."""

    pane_id: str
    shell_pid: int
    foreground_pgid: int
    #: ``(pid, name, cmdline)`` for each process in the pane's foreground process group.
    foreground: tuple[tuple[int, str, str], ...]


def environment_entries(
    environ: Mapping[str, str] | Mapping[bytes, bytes] | None = None,
) -> list[bytes]:
    """``environ`` as ``NAME=value`` entries, in order; this process's environment by default.

    This process's environment is read as ``os.environb`` holds it: the interpreter splits each
    entry at its first ``=``, skips one without ``=``, and keeps the first of two entries with one
    name, in the order they came. Put through :func:`child_environment`, that gives exactly what
    the shared rule makes of the entries themselves, because the rule drops what the interpreter
    skips and keeps the entries it keeps.
    """
    source = os.environb if environ is None else environ
    return [os.fsencode(name) + b"=" + os.fsencode(value) for name, value in source.items()]


def child_environment(
    entries: Iterable[bytes], changes: Iterable[tuple[bytes, bytes | None]] = ()
) -> dict[bytes, bytes]:
    """The environment a process herdr-run starts receives, by the rule both editions share.

    * Each entry is split at its first ``=``: the name is what precedes it and the value what
      follows, so a value may hold ``=`` and a name never does.
    * An entry with no ``=``, or with nothing before its first ``=``, is dropped. That includes
      ``=A=1``, which the C library's ``getenv`` and the native runtime's environment reader take
      for a variable named ``=A``: its name here is empty, so no child receives it.
    * Of two entries with one name, the first is kept, where it stood.
    * The order is otherwise the order of ``entries``.
    * Then each change is applied in turn: a value replaces the variable's value where it stands
      (``HOME`` for a Herdr call keeps its place), or is added at the end when the variable is
      absent; ``None`` removes the variable (``PATH`` for a Herdr call).

    Applying the rule to its own result changes nothing, so a process that was itself started by
    it (the restart in UTF-8 mode) passes the same environment on.
    """
    environment: dict[bytes, bytes] = {}
    for entry in entries:
        name, separator, value = entry.partition(b"=")
        if separator and name:
            environment.setdefault(name, value)
    for name, replacement in changes:
        if replacement is None:
            environment.pop(name, None)
        else:
            environment[name] = replacement
    return environment


def bounded_control_command(
    command: Sequence[str],
    *,
    environ: Mapping[str, str] | Mapping[bytes, bytes] | None = None,
    timeout: float = CONTROL_TIMEOUT_SECONDS,
) -> "subprocess.CompletedProcess[str]":
    """Capture one control command and kill its whole process group on timeout.

    The child starts a new session: it has no controlling terminal, so a helper that tries to
    prompt on ``/dev/tty`` fails at once, and killing the session's process group on timeout also
    reaches descendants that inherited the captured pipes. Output is captured as bytes and decoded
    as UTF-8 with replacement, with no newline translation, so a carriage return reads as one. A
    signal-killed child reports ``-N``. An argument containing a NUL byte is refused before
    anything is spawned.

    The child's environment is ``environ`` (this process's environment by default) put through
    :func:`child_environment`, always passed explicitly: a child never inherits the C library's
    environment array as it is, with its repeated names and entries without ``=``.

    Signal dispositions are handed on as the interpreter holds them. It starts no thread, so the C
    library never takes over a signal for its own use, and a signal herdr-run was started with
    ignored is still ignored in the child -- except SIGPIPE and SIGXFSZ, which ``subprocess``
    gives back their default actions, and SIGCHLD, which herdr-run itself resets at start-up.
    """
    argv = list(command)
    if any("\x00" in argument for argument in argv):
        raise OSError("argument contains a NUL byte")
    environment = child_environment(environment_entries(environ))
    process = subprocess.Popen(
        argv,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=environment,
        start_new_session=True,
    )
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        # start_new_session=True makes the child's PID its process-group ID.  Kill the group even
        # when the immediate child happened to exit at the deadline: descendants may still own the
        # captured pipe ends and are precisely what this cleanup is intended to catch.
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        finally:
            process.kill()
            process.communicate()
        raise
    return subprocess.CompletedProcess(
        argv,
        process.returncode,
        stdout.decode("utf-8", errors="replace"),
        stderr.decode("utf-8", errors="replace"),
    )


def _timeout_text(timeout: float) -> str:
    return f"control command timed out after {int(timeout)}s"


def _invoke_error_text(exc: BaseException, timeout: float) -> str:
    if isinstance(exc, subprocess.TimeoutExpired):
        return _timeout_text(timeout)
    return os_error_text(exc)


def account_home() -> str:
    """The current account's home directory from the account database, never from ``HOME``.

    Returned as text; a home whose bytes are not UTF-8 comes back with surrogate escapes, for
    callers that only need a filesystem path. Herdr calls additionally require UTF-8 (see
    :meth:`HerdrClient._account_home`).
    """
    try:
        home = pwd.getpwuid(os.getuid()).pw_dir
    except KeyError as exc:
        raise HerdrUnavailable("cannot resolve current account home: no account record") from exc
    except OSError as exc:
        raise HerdrUnavailable(
            f"cannot resolve current account home: {os_error_text(exc)}"
        ) from exc
    if not home:
        raise HerdrUnavailable("the current account has no home directory")
    return home


def _has_surrogates(value: str) -> bool:
    return any(0xD800 <= ord(character) <= 0xDFFF for character in value)


def _utf8_absolute(path: str, what: str) -> str:
    if not os.path.isabs(path):
        raise HerdrUnavailable(f"{what} must be absolute: {path}")
    if _has_surrogates(path):
        raise HerdrUnavailable(f"{what} is not valid UTF-8")
    return path


def _resolve_fixed_executable(name: str, candidates: Sequence[str]) -> str:
    """The first candidate that is an executable regular file, canonicalized and checked."""
    for candidate in candidates:
        try:
            metadata = os.stat(candidate)
        except OSError:
            continue
        if not stat.S_ISREG(metadata.st_mode) or not metadata.st_mode & 0o111:
            continue
        try:
            canonical = os.path.realpath(candidate, strict=True)
        except OSError as exc:
            raise HerdrUnavailable(
                f"cannot canonicalize {name} executable {candidate}: {os_error_text(exc)}"
            ) from exc
        try:
            metadata = os.stat(canonical)
        except OSError as exc:
            raise HerdrUnavailable(
                f"cannot inspect {name} executable {canonical}: {os_error_text(exc)}"
            ) from exc
        if not stat.S_ISREG(metadata.st_mode) or not metadata.st_mode & 0o111:
            continue
        if metadata.st_mode & 0o022:
            raise HerdrUnavailable(
                f"refusing group/world-writable {name} executable outside the jail: {canonical}"
            )
        return canonical
    searched = ", ".join(candidates)
    raise HerdrUnavailable(f"{name} executable not found in fixed install locations: {searched}")


def _resolve_configured_herdr(configured: str, home: str) -> str:
    if os.path.sep in configured:
        return _resolve_fixed_executable("Herdr", (configured,))
    return _resolve_fixed_executable(
        "Herdr",
        (
            os.path.join("/usr/local/bin", configured),
            os.path.join("/usr/bin", configured),
            os.path.join(home, ".local", "bin", configured),
            os.path.join(home, "bin", configured),
            os.path.join(home, ".cargo", "bin", configured),
        ),
    )


def _detail(completed: "subprocess.CompletedProcess[str]") -> str:
    """A failed call's detail: trimmed stderr, else trimmed stdout, else ``exit N``.

    Shown with :func:`herdr_run.textfmt.display`, so what Herdr wrote is quoted when it holds a
    line break, a terminal control, or an invisible character: it ends up in a herdr-run message,
    and nothing reaches a terminal raw from one of those.
    """
    detail = strip_space(completed.stderr or "") or strip_space(completed.stdout or "")
    return display(detail) if detail else f"exit {completed.returncode}"


def _stderr_detail(completed: "subprocess.CompletedProcess[str]") -> str:
    """A failed ``pane read``'s detail: trimmed stderr, else ``exit N``, shown as :func:`_detail`."""
    detail = strip_space(completed.stderr or "")
    return display(detail) if detail else f"exit {completed.returncode}"


# ---- protocol narrowing, worded exactly as the native edition words it ---------------------------


def _value_array(value: object, what: str) -> list[object]:
    if not isinstance(value, list):
        raise HerdrUnavailable(f"{what}: expected an array")
    return value


def _object_entries(values: list[object], what: str) -> list[dict[str, object]]:
    entries: list[dict[str, object]] = []
    for value in values:
        if not isinstance(value, dict):
            raise HerdrUnavailable(f"{what}: expected an object")
        entries.append(value)
    return entries


def _required_object(mapping: dict[str, object], key: str, what: str) -> dict[str, object]:
    value = mapping.get(key)
    if not isinstance(value, dict):
        raise HerdrUnavailable(f"{what}: {quote(key)} is not an object")
    return value


def _required_string(mapping: dict[str, object], key: str, what: str) -> str:
    value = mapping.get(key)
    if not isinstance(value, str):
        raise HerdrUnavailable(f"{what}: {quote(key)} is not a string")
    return value


def _optional_string(mapping: dict[str, object], key: str, what: str) -> str | None:
    value = mapping.get(key)
    if value is None or isinstance(value, str):
        return value
    raise HerdrUnavailable(f"{what}: {quote(key)} is not a string")


def _required_integer(mapping: dict[str, object], key: str, what: str) -> int:
    value = mapping.get(key)
    if isinstance(value, bool) or not isinstance(value, int) or not _I64_MIN <= value <= _I64_MAX:
        raise HerdrUnavailable(f"{what}: {quote(key)} is not an integer")
    return value


def _required_process_id(mapping: dict[str, object], key: str, what: str) -> int:
    value = _required_integer(mapping, key, what)
    if not 1 <= value <= _MAX_PROCESS_ID:
        raise HerdrUnavailable(f"{what}: {quote(key)} is outside the positive Linux pid_t range")
    return value


def _unique_label_id(
    entries: list[dict[str, object]], id_key: str, label: str, kind: str
) -> str | None:
    what = f"{kind} list entry"
    matches: list[str] = []
    for entry in entries:
        if _optional_string(entry, "label", what) == label:
            matches.append(_required_string(entry, id_key, what))
    if len(matches) > 1:
        raise HerdrUnavailable(
            f"{kind} label {quote(label)} is ambiguous: {len(matches)} matching IDs"
        )
    return matches[0] if matches else None


class HerdrClient:
    """Command-level access to a Herdr session, with everything narrowed to concrete types."""

    def __init__(
        self,
        *,
        herdr_bin: str = "herdr",
        broker: str = "direct",
        run: Runner | None = None,
        environ: Mapping[str, str] | None = None,
        sleep: Callable[[float], None] = time.sleep,
    ) -> None:
        self._bin = herdr_bin
        # Injected runners are protocol fakes and intentionally receive the literal configured
        # command. Production calls pin every executable to one canonical absolute path.
        self._production = run is None
        self._run: Runner | None = run
        self._resolved_bin: str | None = None
        self._resolved_systemd_run: str | None = None
        self._broker = broker
        self._environ = dict(os.environ if environ is None else environ)
        self._sleep = sleep

    # ---- plumbing ---------------------------------------------------------------------------

    @staticmethod
    def _account_home() -> str:
        """The account database's home, which every Herdr call runs with, as UTF-8 text."""
        return _utf8_absolute(account_home(), "account home")

    def _executable(self) -> str:
        """Return one canonical executable without trusting the caller's PATH ordering.

        This removes the easy ``PATH=$PWD:$PATH`` escalation into ``systemd-run``. It is a safety
        rail, not a same-UID trust boundary: normal per-user installations are owner-writable, as
        documented in the user guide.
        """
        if not self._production:
            return self._bin
        if self._resolved_bin is not None:
            return self._resolved_bin
        home = "" if os.path.sep in self._bin else self._account_home()
        resolved = _utf8_absolute(_resolve_configured_herdr(self._bin, home), "Herdr executable")
        self._resolved_bin = resolved
        return resolved

    def _systemd_executable(self) -> str:
        if not self._production:
            return "systemd-run"
        if self._resolved_systemd_run is not None:
            return self._resolved_systemd_run
        resolved = _utf8_absolute(
            _resolve_fixed_executable("systemd-run", SYSTEMD_RUN_CANDIDATES),
            "systemd-run executable",
        )
        self._resolved_systemd_run = resolved
        return resolved

    def _execute(self, command: Sequence[str]) -> "subprocess.CompletedProcess[str]":
        """Run one control command; production calls discard caller PATH and pin HOME."""
        if self._run is not None:
            return self._run(command)
        environ = child_environment(
            environment_entries(self._environ),
            ((b"PATH", None), (b"HOME", os.fsencode(self._account_home()))),
        )
        return bounded_control_command(
            command, environ=environ, timeout=CONTROL_TIMEOUT_SECONDS
        )

    def _invoke(self, args: Sequence[str]) -> "subprocess.CompletedProcess[str]":
        command = [self._executable(), *args]
        if self._broker == "systemd-run":
            command = [
                self._systemd_executable(),
                "--user",
                "--wait",
                "--pipe",
                "--collect",
                "--quiet",
                "--setenv",
                f"HOME={self._account_home()}",
                *command,
            ]
        try:
            return self._execute(command)
        except (OSError, ValueError, subprocess.TimeoutExpired) as exc:
            raise HerdrUnavailable(
                f"cannot invoke Herdr: {_invoke_error_text(exc, CONTROL_TIMEOUT_SECONDS)}"
            ) from exc

    def _call(self, args: Sequence[str], purpose: str) -> dict[str, object]:
        """Invoke a socket-API subcommand and return its ``result`` object."""
        completed = self._invoke(args)
        if completed.returncode != 0:
            raise HerdrUnavailable(f"{purpose}: {_detail(completed)}")
        try:
            document = strict_json_loads(completed.stdout)
        except ValueError as exc:
            preview = strip_space(completed.stdout)[:200]
            raise HerdrUnavailable(
                f"{purpose}: herdr returned non-JSON output: {quote(preview)}"
            ) from exc
        if not isinstance(document, dict):
            raise HerdrUnavailable(f"{purpose}: Herdr response is not an object")
        result = document.get("result")
        if not isinstance(result, dict):
            raise HerdrUnavailable(f"{purpose}: Herdr response has no result object")
        return result

    def _call_ok(self, args: Sequence[str], purpose: str) -> None:
        """Invoke a subcommand that reports success only through its exit status.

        ``pane run`` writes NOTHING on success -- it is an input-injection call, not a query -- so
        requiring a JSON envelope here would fail every successful call.
        """
        completed = self._invoke(args)
        if completed.returncode != 0:
            raise HerdrUnavailable(f"{purpose}: {_detail(completed)}")

    # ---- server -----------------------------------------------------------------------------

    def preflight(self) -> None:
        """Resolve every executable this client would need, raising if one is missing.

        ``status`` must be able to distinguish "Herdr is not installed" from "Herdr is installed and
        no server is running", and :meth:`server_running` deliberately answers ``False`` to both.
        Herdr is checked first, then ``systemd-run``.
        """
        self._executable()
        self._systemd_executable()

    def server_running(self) -> bool:
        """Is a compatible Herdr server currently up? Never raises; absence is a normal answer."""
        try:
            completed = self._invoke(["status", "--json"])
        except HerdrUnavailable:
            return False
        if completed.returncode != 0:
            return False
        try:
            document = strict_json_loads(completed.stdout)
        except ValueError:
            return False
        if not isinstance(document, dict):
            return False
        server = document.get("server")
        return isinstance(server, dict) and server.get("running") is True

    def ensure_server(self, *, attempts: int = 30, delay: float = 0.2) -> bool:
        """Start the Herdr server if it is not already running. Returns True if it had to start one.

        Always launched via ``systemd-run --user`` regardless of the configured broker — see this
        module's docstring for why that is a correctness requirement, not a convenience.
        """
        if self.server_running():
            return False

        herdr = self._executable()
        systemd_run = self._systemd_executable()
        home = self._account_home()
        launch = [
            systemd_run,
            "--user",
            "--collect",
            "--unit",
            SERVER_UNIT,
            "--description",
            "herdr-run Herdr server (outside the agent sandbox)",
            "--setenv",
            f"HOME={home}",
            "--setenv",
            f"TOKIO_WORKER_THREADS={SERVER_WORKER_THREADS}",
            herdr,
            "server",
        ]
        try:
            completed = self._execute(launch)
        except (OSError, ValueError, subprocess.TimeoutExpired) as exc:
            raise HerdrUnavailable(
                "cannot start the Herdr server: "
                f"{_invoke_error_text(exc, CONTROL_TIMEOUT_SECONDS)}"
            ) from exc
        combined = (completed.stdout or "") + (completed.stderr or "")
        if completed.returncode != 0 and "already exists" not in combined:
            raise HerdrUnavailable(f"cannot start the Herdr server: {_detail(completed)}")

        for _ in range(attempts):
            self._sleep(delay)
            if self.server_running():
                return True
        raise HerdrUnavailable(
            f"the Herdr server did not become ready after {attempts} attempts; "
            f"check 'systemctl --user status {SERVER_UNIT}'"
        )

    # ---- workspaces / tabs / panes ------------------------------------------------------------

    def workspace_id_for_label(self, label: str) -> str | None:
        """Resolve a workspace LABEL to its id, or ``None`` when no workspace carries that label."""
        result = self._call(["workspace", "list"], "workspace list")
        values = _value_array(result.get("workspaces"), "workspace list.workspaces")
        entries = _object_entries(values, "workspace list entry")
        return _unique_label_id(entries, "workspace_id", label, "workspace")

    def create_workspace(self, *, label: str, cwd: str) -> tuple[str, str, str]:
        """Create a workspace. Returns ``(workspace_id, root_tab_id, root_pane_id)``.

        Herdr gives a new workspace one default tab (labelled ``"1"``); the caller renames it rather
        than creating a second tab, so a freshly created workspace has exactly one tab.
        """
        result = self._call(
            ["workspace", "create", "--label", label, "--cwd", cwd, "--no-focus"],
            f"workspace create {quote(label)}",
        )
        workspace = _required_object(result, "workspace", "workspace create")
        tab = _required_object(result, "tab", "workspace create")
        pane = _required_object(result, "root_pane", "workspace create")
        return (
            _required_string(workspace, "workspace_id", "workspace create"),
            _required_string(tab, "tab_id", "workspace create"),
            _required_string(pane, "pane_id", "workspace create"),
        )

    def tab_id_for_label(self, workspace_id: str, label: str) -> str | None:
        """Resolve a tab LABEL within one workspace to its id, or ``None`` when absent."""
        result = self._call(["tab", "list", "--workspace", workspace_id], "tab list")
        values = _value_array(result.get("tabs"), "tab list.tabs")
        entries = _object_entries(values, "tab list entry")
        return _unique_label_id(entries, "tab_id", label, "tab")

    def create_tab(self, *, workspace_id: str, label: str, cwd: str) -> str:
        """Create a labelled tab in an existing workspace and return its id. Never steals focus."""
        result = self._call(
            [
                "tab",
                "create",
                "--workspace",
                workspace_id,
                "--label",
                label,
                "--cwd",
                cwd,
                "--no-focus",
            ],
            f"tab create {quote(label)}",
        )
        tab: object = result["tab"] if "tab" in result else result
        if not isinstance(tab, dict):
            raise HerdrUnavailable('tab create: "tab" is not an object')
        return _required_string(tab, "tab_id", "tab create")

    def rename_tab(self, tab_id: str, label: str) -> None:
        """Relabel an existing tab."""
        self._call(["tab", "rename", tab_id, label], f"tab rename {tab_id}")

    def close_tab(self, tab_id: str) -> None:
        """Close one explicitly owned tab, without closing its shared workspace."""
        self._call_ok(["tab", "close", tab_id], f"tab close {tab_id}")

    def panes(self, workspace_id: str | None = None) -> tuple[Pane, ...]:
        """Every pane, optionally restricted to one workspace.

        Every entry is validated before any is checked against the requested workspace.
        """
        args = ["pane", "list"]
        if workspace_id is not None:
            args += ["--workspace", workspace_id]
        result = self._call(args, "pane list")
        values = _value_array(result.get("panes"), "pane list.panes")
        entries = _object_entries(values, "pane list entry")
        panes = tuple(
            Pane(
                pane_id=_required_string(entry, "pane_id", "pane list entry"),
                tab_id=_required_string(entry, "tab_id", "pane list entry"),
                workspace_id=_required_string(entry, "workspace_id", "pane list entry"),
            )
            for entry in entries
        )
        if workspace_id is not None:
            for pane in panes:
                if pane.workspace_id != workspace_id:
                    raise HerdrUnavailable(
                        f"pane list: returned pane {quote(pane.pane_id)} from workspace "
                        f"{quote(pane.workspace_id)}, expected {quote(workspace_id)}"
                    )
        return panes

    def pane_exists(self, pane_id: str) -> bool:
        """Is this pane id still live? Used to invalidate a cached id rather than trust it."""
        try:
            self._call(["pane", "get", pane_id], f"pane get {pane_id}")
        except HerdrUnavailable:
            return False
        return True

    def process_info(self, pane_id: str) -> ProcessInfo:
        """The pane's live shell pid and foreground process group — the readiness signal."""
        result = self._call(
            ["pane", "process-info", "--pane", pane_id], f"pane process-info {pane_id}"
        )
        info = _required_object(result, "process_info", "pane process-info")
        values = _value_array(info.get("foreground_processes"), "foreground_processes")
        foreground = tuple(
            (
                _required_process_id(entry, "pid", "foreground process"),
                _optional_string(entry, "name", "foreground process") or "",
                _optional_string(entry, "cmdline", "foreground process") or "",
            )
            for entry in _object_entries(values, "foreground process")
        )
        parsed = ProcessInfo(
            pane_id=_required_string(info, "pane_id", "pane process-info"),
            shell_pid=_required_process_id(info, "shell_pid", "pane process-info"),
            foreground_pgid=_required_process_id(
                info, "foreground_process_group_id", "pane process-info"
            ),
            foreground=foreground,
        )
        if parsed.pane_id != pane_id:
            raise HerdrUnavailable(
                f"pane process-info: returned pane {quote(parsed.pane_id)}, "
                f"expected {quote(pane_id)}"
            )
        return parsed

    def read(
        self,
        pane_id: str,
        *,
        source: str = "recent-unwrapped",
        lines: int | None = None,
    ) -> str:
        """Read the pane's rendered text. ANSI is stripped (no ``--raw``), trailing spaces included.

        Note this returns PLAIN TEXT, not JSON: ``herdr pane read`` writes the terminal contents to
        stdout directly, which is why it does not go through :meth:`_call`.
        """
        args = ["pane", "read", pane_id, "--source", source]
        if lines is not None:
            args += ["--lines", str(lines)]
        completed = self._invoke(args)
        if completed.returncode != 0:
            raise HerdrUnavailable(f"pane read {pane_id}: {_stderr_detail(completed)}")
        return completed.stdout

    def run(self, pane_id: str, command: str) -> None:
        """Type ``command`` into the pane's shell and submit it.

        Fire-and-forget by design: ``herdr pane run`` returns as soon as the line is submitted, so
        the caller collects the result from the spool files rather than from this call.
        """
        self._call_ok(["pane", "run", pane_id, command], f"pane run {pane_id}")
