"""Direct typed adapter for Herdr pane and native harness commands.

Herdr owns its server and terminal lifecycle. This adapter invokes the public
CLI and does not start a server, select a broker, or execute shell jobs.
"""

from __future__ import annotations

import errno
import json
import os
import pwd
import re
import select
import shlex
import signal
import stat
import subprocess
import time
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from typing import Literal, Protocol

from agentctl.errors import HerdrUnavailable, InputExpectationFailed
from agentctl.jsonx import as_mapping, as_sequence, get_int, get_str, opt_str
from agentctl.procstat import ProcessStat, parse_process_stat
from agentctl.submission import (
    SCREEN_LINES as SUBMISSION_SCREEN_LINES,
    VERIFIED_HARNESSES,
    PromptNotStaged,
    SubmissionReceipt,
    codex_idle_composer,
    composer_view,
    submit_verified,
)

__all__ = [
    "HerdrClient",
    "Pane",
    "ProcessInfo",
    "CustomProcessIdentity",
    "PaneShellProof",
    "Runner",
    "CONTROL_TIMEOUT_SECONDS",
    "AgentPaneInfo",
]

#: A ``subprocess.run``-shaped callable, injected so tests never spawn a real process.
Runner = Callable[[Sequence[str]], "subprocess.CompletedProcess[str]"]

#: Maximum wait for one Herdr control command, in seconds.
CONTROL_TIMEOUT_SECONDS = 30.0
_CONTROL_STDOUT_BYTES = 8 << 20
_CONTROL_STDERR_BYTES = 64 << 10
_CONTROL_READ_BURST = 256 << 10

# Linux exposes process IDs through the positive range of its signed ``pid_t``.  Keep this bound
# explicit so Python's arbitrary-precision integers cannot accept protocol values that the Rust
# implementation (or a Linux process API) cannot represent.
_MAX_PROCESS_ID = 2_147_483_647
_MAX_U64 = (1 << 64) - 1
_SUPPORTED_PANE_SHELLS = frozenset(("bash", "zsh", "sh", "dash", "fish", "ksh"))
_MAX_SHELL_TASKS = 4096
_MAX_CHILDREN_BYTES = 64 << 10
_MAX_PROC_ENTRIES = 1 << 20
_MUSE_EFFORT = re.compile(r"[a-z0-9_-]{1,32}\Z")
_BOOT_ID = re.compile(
    r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\Z"
)


_HERDR_ERROR_CODE = re.compile(r'"code"\s*:\s*"([a-z_]+)"')


def herdr_error_code(detail: str) -> str | None:
    """The ``code`` of a Herdr error object quoted in ``detail``, such as ``pane_not_found``."""
    match = _HERDR_ERROR_CODE.search(detail)
    return match.group(1) if match else None


def _unique_response_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate Herdr response field")
        result[key] = value
    return result


def _is_codex_startup_timeout(completed: subprocess.CompletedProcess[str]) -> bool:
    """Recognize only Herdr's documented startup-timeout error envelope."""
    if completed.returncode != 1 or completed.stdout.strip():
        return False
    try:
        document = json.loads(completed.stderr, object_pairs_hook=_unique_response_object)
    except (TypeError, ValueError):
        return False
    return bool(document == {
        "id": "cli:agent:start",
        "error": {"code": "timeout", "message": "timed out waiting for agent startup"},
    })


def muse_startup_metadata(screen: str) -> tuple[str | None, str | None]:
    """Return a bounded Muse downgrade warning and its explicit effective effort.

    Launch arguments describe requested policy, not necessarily the policy the
    provider accepted.  Only Muse's narrow, human-visible downgrade sentence is
    persisted; arbitrary terminal text never becomes agent metadata.
    """
    for raw_line in screen.splitlines():
        line = raw_line.strip()
        if not line.isascii() or len(line) > 256:
            continue
        prefix = "reasoning effort "
        if not line.startswith(prefix) or "; using " not in line:
            continue
        unavailable, effective = line[len(prefix):].rsplit("; using ", 1)
        requested, marker, detail = unavailable.partition(" is not available")
        if (not marker or _MUSE_EFFORT.fullmatch(requested) is None
                or _MUSE_EFFORT.fullmatch(effective) is None):
            continue
        if detail and not (detail.startswith(" (") and detail.endswith(")")):
            continue
        return line, effective
    return None, None


def muse_trust_prompt(screen: str) -> bool:
    """Recognize only explicit workspace-trust questions."""
    lowered = screen.lower()
    return (
        "trust this workspace" in lowered
        or ("do you trust" in lowered
            and ("workspace" in lowered or "folder" in lowered))
        or "workspace trust" in lowered
    )


def muse_idle_composer(screen: str) -> bool:
    """Recognize Muse's idle composer without mistaking a choice prompt for it."""
    regions = _muse_composer_regions(screen, require_header=True)
    return regions is not None and regions[1].strip() in ("❯", "›")


def muse_auto_review_idle_composer(screen: str) -> bool:
    """Require the Auto-review footer on the active empty composer."""
    regions = _muse_composer_regions(screen, require_header=True)
    if regions is None or regions[1].strip() not in ("❯", "›"):
        return False
    lines = screen.splitlines()
    dividers = [
        index for index, line in enumerate(lines)
        if len(line.strip()) >= 3 and set(line.strip()) <= {"─", "━", "═"}
    ]
    modes = _muse_footer_modes(lines[dividers[-1] + 1:])
    return modes == ("Auto-review",)


def muse_verified_process_idle_composer(screen: str) -> bool:
    """Recognize an idle editor after exact live-Muse process verification."""
    regions = _muse_composer_regions(screen, require_header=False)
    return regions is not None and regions[1].strip() in ("❯", "›")


def muse_verified_process_composer(screen: str) -> bool:
    """Recognize a possibly nonempty editor after live-Muse process verification."""
    regions = _muse_composer_regions(screen, require_header=False)
    return regions is not None and _muse_editor_segments(regions[1]) is not None


def muse_verified_process_goal_paused(screen: str) -> bool:
    """Recognize Muse's explicit paused-goal state in a verified UI frame."""
    if _muse_composer_regions(screen, require_header=False) is None:
        return False
    lines = screen.splitlines()
    bottom = max(
        index for index, line in enumerate(lines)
        if len(line.strip()) >= 3 and set(line.strip()) <= {"─", "━", "═"}
    )
    return any(line.strip() == "Goal (paused)" for line in lines[bottom + 1:])


def _muse_composer_regions(
    screen: str, *, require_header: bool = True,
) -> tuple[str, str] | None:
    """Split transcript/composer using Muse's ruled editor and status footer."""
    lines = screen.splitlines()
    dividers = [
        index for index, line in enumerate(lines)
        if len(line.strip()) >= 3 and set(line.strip()) <= {"─", "━", "═"}
    ]
    if len(dividers) < 2:
        return None
    top, bottom = dividers[-2:]
    footer = lines[bottom + 1:]
    versioned_header = any(
        re.fullmatch(r"Muse Code [0-9]+\.[0-9]+\.[0-9]+", line.strip())
        for line in lines[:top]
    )

    if not ((versioned_header or not require_header)
            and len(_muse_footer_modes(footer)) == 1):
        return None
    return "\n".join(lines[:top]), "\n".join(lines[top + 1:bottom])


def _muse_status_mode(line: str) -> str | None:
    """Return the explicit mode from one structurally valid Muse footer."""
    fields = [field.strip() for field in line.split("·")]
    if (len(fields) not in (3, 4) or not all(fields)
            or _MUSE_EFFORT.fullmatch(fields[1]) is None):
        return None
    if len(fields) == 3:
        return "standard"
    return fields[3] if fields[3] in ("YOLO", "Auto-review") else None


def _muse_footer_modes(lines: list[str]) -> tuple[str, ...]:
    """Return all structurally valid footer modes; callers require one authority."""
    return tuple(
        mode for line in lines if line.strip()
        for mode in (_muse_status_mode(line.strip()),) if mode is not None
    )


def _muse_editor_segments(editor: str) -> tuple[str, ...] | None:
    """Return normalized editor lines after one leading Muse prompt marker."""
    lines = [" ".join(line.split()) for line in editor.splitlines()]
    lines = [line for line in lines if line]
    if not lines:
        return None
    first = lines[0]
    marker = next(
        (candidate for candidate in ("❯", "›")
         if first == candidate or first.startswith(candidate + " ")),
        None,
    )
    if marker is None:
        return None
    initial = first[len(marker):].strip()
    return tuple(([initial] if initial else []) + lines[1:])


def _muse_editor_matches(editor: str, text: str, *, exact: bool) -> bool:
    """Match a logical prompt against one conservatively rendered Muse editor."""
    wanted = " ".join(text.split())
    segments = _muse_editor_segments(editor)
    if not wanted or segments is None:
        return False
    prefixes = {""}
    previous = ""
    for index, segment in enumerate(segments):
        separators = ("",) if index == 0 else ((" ", "") if previous.endswith("-") else (" ",))
        next_prefixes: set[str] = set()
        for prefix in prefixes:
            for separator in separators:
                rendered = prefix + separator + segment
                if not exact and (
                    rendered == wanted or rendered.startswith(wanted + " ")
                ):
                    return True
                if wanted.startswith(rendered):
                    next_prefixes.add(rendered)
        if not next_prefixes:
            return False
        prefixes = next_prefixes
        previous = segment
    return wanted in prefixes


def _muse_prompt_in_composer(
    screen: str, text: str, *, require_header: bool, exact: bool,
) -> bool:
    regions = _muse_composer_regions(screen, require_header=require_header)
    return regions is not None and _muse_editor_matches(
        regions[1], text, exact=exact,
    )


def muse_prompt_in_composer(screen: str, text: str) -> bool:
    """Conservatively detect a prompt retained in Muse's bottom editor."""
    return _muse_prompt_in_composer(
        screen, text, require_header=True, exact=False,
    )


def muse_verified_process_prompt_in_composer(screen: str, text: str) -> bool:
    """Detect retained input after exact live-Muse process verification."""
    return _muse_prompt_in_composer(
        screen, text, require_header=False, exact=False,
    )


def muse_prompt_is_exact_composer(screen: str, text: str) -> bool:
    """Require the entire active Muse editor to be this literal prompt."""
    return _muse_prompt_in_composer(
        screen, text, require_header=True, exact=True,
    )


def muse_verified_process_prompt_is_exact_composer(screen: str, text: str) -> bool:
    """Require exact editor text after pinning the live Muse process."""
    return _muse_prompt_in_composer(
        screen, text, require_header=False, exact=True,
    )


def _muse_prompt_transcript_count(transcript: str, text: str) -> int:
    """Count complete marker-delimited user turns, never prompt prefixes."""
    if not " ".join(text.split()):
        return 0
    lines = transcript.splitlines()
    count = 0
    index = 0
    while index < len(lines):
        stripped = lines[index].strip()
        if not any(
            stripped == marker or stripped.startswith(marker + " ")
            for marker in ("❯", "›")
        ):
            index += 1
            continue
        end = index + 1
        while end < len(lines):
            next_line = lines[end].strip()
            if any(
                next_line == marker or next_line.startswith(marker + " ")
                for marker in ("◆", "❯", "›")
            ) or re.fullmatch(r"(?:Working|Thinking)(?:\.\.\.|\u2026)", next_line):
                break
            end += 1
        if _muse_editor_matches("\n".join(lines[index:end]), text, exact=True):
            count += 1
        index = end
    return count


def muse_prompt_in_transcript(screen: str, text: str) -> bool:
    """Require the exact prompt as a user turn above the active Muse editor."""
    return muse_prompt_transcript_count(screen, text) > 0


def muse_prompt_transcript_count(screen: str, text: str) -> int:
    """Count bounded prompt renderings above the composer for transition proofs."""
    regions = _muse_composer_regions(screen, require_header=True)
    return 0 if regions is None else _muse_prompt_transcript_count(regions[0], text)


def muse_verified_process_prompt_transcript_count(screen: str, text: str) -> int:
    """Count exact user turns after pinning the live Muse process."""
    regions = _muse_composer_regions(screen, require_header=False)
    return 0 if regions is None else _muse_prompt_transcript_count(regions[0], text)


def _get_process_id(mapping: dict[str, object], key: str, what: str) -> int:
    """Require one positive Linux ``pid_t``-compatible protocol value."""
    value = get_int(mapping, key, what)
    if not 1 <= value <= _MAX_PROCESS_ID:
        raise TypeError(
            f"{what}: field {key!r} is outside the positive Linux pid_t range"
        )
    return value


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
    #: ``(pid, name, cmdline, argv0, reported_executable)``; unknown argv0 is empty.
    foreground: tuple[tuple[int, str, str, str, str | None], ...]


@dataclass(frozen=True)
class CustomProcessIdentity:
    """Kernel identity of one pane-owned process generation."""

    version: int
    boot_id: str
    pid: int
    starttime_ticks: int
    executable_device: int
    executable_inode: int


@dataclass(frozen=True)
class PaneShellProof:
    """Exact supported idle-shell process generation and kernel executable path."""

    identity: CustomProcessIdentity
    executable_path: str


@dataclass(frozen=True)
class AgentPaneInfo:
    """Identity and readiness fields for one interactive-agent pane."""

    pane_id: str
    workspace_id: str
    cwd: str
    agent: str | None
    status: str
    session_agent: str | None
    session_value: str | None
    #: Herdr's terminal and tab for the pane, when ``pane get`` reported them.
    terminal_id: str | None = None
    tab_id: str | None = None


class GuardedInput(Protocol):
    """A prompt terminal that runs each input effect between recipient checks."""

    def read_screen(self, pane_id: str) -> str:
        """Read the pane's visible rows; reading is not an input effect."""
        ...

    def send_text(self, pane_id: str, text: str) -> None:
        """Insert literal text between recipient checks."""
        ...

    def send_keys(self, pane_id: str, keys: str) -> None:
        """Send named keys between recipient checks."""
        ...

    def native_prompt(self, pane_id: str, text: str) -> None:
        """Submit through Herdr's native prompt primitive between recipient checks."""
        ...


@dataclass(frozen=True)
class AgentIdentity:
    """What ``agent get NAME`` reports about one named Herdr agent."""

    name: str
    pane_id: str
    tab_id: str | None
    terminal_id: str | None


def _bounded_control_command(
    command: Sequence[str],
    *,
    environ: Mapping[str, str] | None = None,
    timeout: float = CONTROL_TIMEOUT_SECONDS,
) -> "subprocess.CompletedProcess[str]":
    """Capture one control command and kill its whole process group on timeout.

    A Herdr control command may itself start helpers which inherit the captured pipes.
    Giving every control call a fresh session lets the timeout path terminate those descendants as
    well as the immediate child, then reap the child before returning.  Otherwise a timed-out call
    could leave an unbounded helper behind or block forever waiting for its inherited pipe ends.
    """
    argv = list(command)
    process = subprocess.Popen(
        argv,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=None if environ is None else dict(environ),
        start_new_session=True,
    )
    assert process.stdout is not None and process.stderr is not None
    stdout_content = bytearray()
    stderr_content = bytearray()
    streams = {
        process.stdout.fileno(): (
            process.stdout, stdout_content, _CONTROL_STDOUT_BYTES, "stdout",
        ),
        process.stderr.fileno(): (
            process.stderr, stderr_content, _CONTROL_STDERR_BYTES, "stderr",
        ),
    }
    poller = select.poll()
    for descriptor in streams:
        os.set_blocking(descriptor, False)
        poller.register(descriptor, select.POLLIN | select.POLLHUP | select.POLLERR)
    deadline = time.monotonic() + timeout
    try:
        while streams:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise subprocess.TimeoutExpired(argv, timeout)
            events = poller.poll(max(1, min(10, int(remaining * 1000))))
            for descriptor, _event in events:
                stream = streams.get(descriptor)
                if stream is None:
                    continue
                pipe, content, limit, label = stream
                consumed = 0
                while consumed < _CONTROL_READ_BURST:
                    try:
                        block = os.read(
                            descriptor,
                            min(64 << 10, limit + 1 - len(content)),
                        )
                    except BlockingIOError:
                        break
                    if not block:
                        poller.unregister(descriptor)
                        pipe.close()
                        del streams[descriptor]
                        break
                    content.extend(block)
                    consumed += len(block)
                    if len(content) > limit:
                        raise OSError(
                            errno.EFBIG,
                            f"Herdr control {label} exceeds {limit} bytes",
                        )
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise subprocess.TimeoutExpired(argv, timeout)
        returncode = process.wait(timeout=remaining)
    except BaseException:
        # start_new_session=True makes the child's PID its process-group ID.  Kill the group even
        # when the immediate child happened to exit at the deadline: descendants may still own the
        # captured pipe ends and are precisely what this cleanup is intended to catch.
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        finally:
            process.kill()
            for pipe in (process.stdout, process.stderr):
                pipe.close()
            process.wait()
        raise
    return subprocess.CompletedProcess(
        argv,
        returncode,
        stdout_content.decode("utf-8", errors="replace"),
        stderr_content.decode("utf-8", errors="replace"),
    )


def default_runner(command: Sequence[str]) -> "subprocess.CompletedProcess[str]":
    """Run a command and capture its output. The real runner, replaced by tests."""
    return _bounded_control_command(command)


_PRODUCTION_RUNNER = default_runner


def relay_trust_prompt(screen: str) -> bool:
    """Whether a Claude or Codex workspace trust dialog is on screen."""
    return (
        ("Quick safety check: Is this a project you created or one you trust?" in screen
         and "Yes, I trust this folder" in screen)
        or ("Trust this folder?" in screen and "Trust and continue" in screen)
    )


def _validated_executable(candidate: str, name: str) -> str:
    """Canonicalize and validate one fixed executable candidate."""
    resolved = os.path.realpath(os.path.abspath(candidate))
    try:
        metadata = os.stat(resolved)
    except OSError as exc:
        raise HerdrUnavailable(
            f"cannot inspect {name} executable {resolved}: {exc}"
        ) from exc
    if not stat.S_ISREG(metadata.st_mode) or not os.access(resolved, os.X_OK):
        raise HerdrUnavailable(
            f"{name} executable is not an executable regular file: {resolved}"
        )
    if metadata.st_mode & (stat.S_IWGRP | stat.S_IWOTH):
        raise HerdrUnavailable(
            f"refusing group/world-writable {name} executable for agent control: {resolved}"
        )
    return resolved


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
        # command.  Production calls pin the executable to one canonical absolute path before
        # invoking the public control interface.
        self._resolve_executable = run is None
        selected_runner = default_runner if run is None else run
        # Keeping the production runner as an identity sentinel lets tests replace the module's
        # default runner while still exercising absolute-path resolution.  The real default is
        # the only path that may invoke subprocess directly with the scrubbed environment below.
        self._production_runner = selected_runner is _PRODUCTION_RUNNER
        self._resolved_bin: str | None = None
        self._input_expect: bool | None = None
        if broker != "direct":
            raise ValueError("agentctl uses direct Herdr control; server setup is external")
        self._run: Runner = selected_runner
        self._environ = dict(os.environ if environ is None else environ)
        self._sleep = sleep

    # ---- plumbing ---------------------------------------------------------------------------

    def _executable(self) -> str:
        """Return one canonical executable without trusting the caller's PATH ordering.

        This removes the easy ``PATH=$PWD:$PATH`` escalation into ``systemd-run``. It is a safety
        rail, not a same-UID trust boundary: normal per-user installations are owner-writable, as
        documented in the user guide.
        """
        if not self._resolve_executable:
            return self._bin
        if self._resolved_bin is not None:
            return self._resolved_bin
        candidate = self._bin
        if os.path.sep not in candidate:
            home = self._account_home()
            fixed_candidates = (
                os.path.join("/usr/local/bin", candidate),
                os.path.join("/usr/bin", candidate),
                os.path.join(home, ".local", "bin", candidate),
                os.path.join(home, "bin", candidate),
                os.path.join(home, ".cargo", "bin", candidate),
            )
            candidate = next(
                (
                    path
                    for path in fixed_candidates
                    if os.path.isfile(path) and os.access(path, os.X_OK)
                ),
                "",
            )
            if not candidate:
                searched = ", ".join(fixed_candidates)
                raise HerdrUnavailable(
                    f"Herdr executable not found in fixed install locations: {searched}"
                )
        resolved = _validated_executable(candidate, "Herdr")
        self._resolved_bin = resolved
        return resolved

    @staticmethod
    def _account_home() -> str:
        """Read HOME from the account database, not caller-controlled environment text."""
        try:
            home = pwd.getpwuid(os.getuid()).pw_dir
        except (KeyError, OSError) as exc:
            raise HerdrUnavailable(
                f"cannot resolve the current account's home directory: {exc}"
            ) from exc
        if not home:
            raise HerdrUnavailable("the current account has no home directory")
        return home


    def _execute(
        self,
        command: Sequence[str],
        *,
        timeout: float = CONTROL_TIMEOUT_SECONDS,
    ) -> "subprocess.CompletedProcess[str]":
        """Invoke one control command; production calls discard caller HOME/PATH."""
        try:
            if not self._production_runner:
                return self._run(command)
            environ = dict(self._environ)
            environ.pop("PATH", None)
            environ["HOME"] = self._account_home()
            return _bounded_control_command(command, environ=environ, timeout=timeout)
        except subprocess.TimeoutExpired as exc:
            raise HerdrUnavailable(
                f"Herdr control command timed out after {timeout:g} seconds"
            ) from exc
        except (OSError, ValueError) as exc:
            raise HerdrUnavailable(f"cannot invoke Herdr: {exc}") from exc


    def _invoke(
        self,
        args: Sequence[str],
        *,
        timeout: float = CONTROL_TIMEOUT_SECONDS,
    ) -> "subprocess.CompletedProcess[str]":
        command = [self._executable(), *args]
        try:
            return self._execute(command, timeout=timeout)
        except (
            OSError
        ) as exc:  # pragma: no cover - _execute already narrows production failures
            raise HerdrUnavailable(f"cannot invoke Herdr: {exc}") from exc

    def _call(self, args: Sequence[str], purpose: str) -> dict[str, object]:
        """Invoke a socket-API subcommand and return its ``result`` object."""
        completed = self._invoke(args)
        if completed.returncode != 0:
            detail = (
                completed.stderr or completed.stdout or ""
            ).strip() or f"exit {completed.returncode}"
            raise HerdrUnavailable(f"{purpose}: {detail}")
        try:
            document: object = json.loads(completed.stdout)
        except json.JSONDecodeError as exc:
            preview = completed.stdout.strip()[:200]
            raise HerdrUnavailable(
                f"{purpose}: herdr returned non-JSON output: {preview!r}"
            ) from exc
        try:
            envelope = as_mapping(document, purpose)
            result = envelope.get("result")
            if not isinstance(result, dict):
                raise HerdrUnavailable(
                    f"{purpose}: herdr response has no result object"
                )
            return as_mapping(result, purpose)
        except TypeError as exc:
            raise HerdrUnavailable(f"{purpose}: invalid Herdr response: {exc}") from exc

    # ---- server -----------------------------------------------------------------------------




    # ---- workspaces / tabs / panes ------------------------------------------------------------

    def workspace_id_for_label(self, label: str) -> str | None:
        """Resolve a workspace LABEL to its id, or ``None`` when no workspace carries that label."""
        result = self._call(["workspace", "list"], "workspace list")
        try:
            matches: list[str] = []
            for entry in as_sequence(result.get("workspaces"), "workspace list"):
                workspace = as_mapping(entry, "workspace list entry")
                if opt_str(workspace, "label") == label:
                    matches.append(
                        get_str(workspace, "workspace_id", "workspace list entry")
                    )
        except TypeError as exc:
            raise HerdrUnavailable(
                f"workspace list: invalid Herdr response: {exc}"
            ) from exc
        if len(matches) > 1:
            raise HerdrUnavailable(
                f"workspace label {label!r} is ambiguous across ids: {', '.join(matches)}"
            )
        return matches[0] if matches else None

    def create_workspace(
        self, *, label: str, cwd: str, environment: Sequence[str] = (),
    ) -> tuple[str, str, str]:
        """Create a workspace. Returns ``(workspace_id, root_tab_id, root_pane_id)``.

        Herdr gives a new workspace one default tab (labelled ``"1"``); the caller renames it rather
        than creating a second tab, so a freshly created workspace has exactly one tab.
        """
        arguments = ["workspace", "create", "--label", label, "--cwd", cwd]
        for entry in environment:
            arguments.extend(("--env", entry))
        arguments.append("--no-focus")
        result = self._call(
            arguments,
            f"workspace create {label!r}",
        )
        try:
            workspace = as_mapping(result.get("workspace"), "workspace create")
            tab = as_mapping(result.get("tab"), "workspace create")
            pane = as_mapping(result.get("root_pane"), "workspace create")
            return (
                get_str(workspace, "workspace_id", "workspace create"),
                get_str(tab, "tab_id", "workspace create"),
                get_str(pane, "pane_id", "workspace create"),
            )
        except TypeError as exc:
            raise HerdrUnavailable(
                f"workspace create: invalid Herdr response: {exc}"
            ) from exc

    def tab_id_for_label(self, workspace_id: str, label: str) -> str | None:
        """Resolve a tab LABEL within one workspace to its id, or ``None`` when absent."""
        result = self._call(["tab", "list", "--workspace", workspace_id], "tab list")
        try:
            matches: list[str] = []
            for entry in as_sequence(result.get("tabs"), "tab list"):
                tab = as_mapping(entry, "tab list entry")
                if opt_str(tab, "label") == label:
                    matches.append(get_str(tab, "tab_id", "tab list entry"))
        except TypeError as exc:
            raise HerdrUnavailable(f"tab list: invalid Herdr response: {exc}") from exc
        if len(matches) > 1:
            raise HerdrUnavailable(
                f"tab label {label!r} is ambiguous in workspace {workspace_id}: "
                f"{', '.join(matches)}"
            )
        return matches[0] if matches else None

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
            f"tab create {label!r}",
        )
        try:
            tab = as_mapping(result.get("tab", result), "tab create")
            return get_str(tab, "tab_id", "tab create")
        except TypeError as exc:
            raise HerdrUnavailable(
                f"tab create: invalid Herdr response: {exc}"
            ) from exc

    def rename_tab(self, tab_id: str, label: str) -> None:
        """Relabel an existing tab."""
        self._call(["tab", "rename", tab_id, label], f"tab rename {tab_id}")

    def create_tab_with_pane(
        self, *, workspace_id: str, label: str, cwd: str,
        environment: Sequence[str] = (),
    ) -> tuple[str, str]:
        """Return the tab and original pane from the same allocation response."""
        arguments = ["tab", "create", "--workspace", workspace_id,
            "--label", label, "--cwd", cwd]
        for entry in environment:
            arguments.extend(("--env", entry))
        arguments.append("--no-focus")
        result = self._call(arguments, f"tab create {label!r}")
        try:
            tab = as_mapping(result.get("tab"), "created tab")
            pane = as_mapping(result.get("root_pane"), "created root pane")
            tab_id = get_str(tab, "tab_id", "created tab")
            if get_str(pane, "tab_id", "created pane") != tab_id or get_str(pane, "workspace_id", "created pane") != workspace_id:
                raise HerdrUnavailable("created pane does not belong to the allocated tab/workspace")
            return tab_id, get_str(pane, "pane_id", "created pane")
        except TypeError as exc:
            raise HerdrUnavailable(f"tab create: invalid allocation identity: {exc}") from exc

    def close_tab(self, tab_id: str) -> None:
        """Close one explicitly owned tab, without closing its shared workspace."""
        self._call_ok(["tab", "close", tab_id], f"tab close {tab_id}")

    def enter_slot_sandbox(self, pane_id: str, command_line: str, *, timeout: float = 30.0) -> int:
        """Replace a fresh pane's shell with a slot-boxed shell; return its PID.

        ``command_line`` comes from the slot manager's ``shell-command`` and execs
        at every step, so the boxed interactive shell keeps the pane's shell PID.
        Readiness is proven from kernel state, not screen text: the pane's own
        shell PID is the sole foreground process, it is a shell again, and its
        cgroup is a slot slice.
        """
        if not 0 < timeout <= 300:
            raise ValueError("slot sandbox timeout must be between 0 and 300 seconds")
        self._call_ok(["pane", "run", pane_id, command_line], f"pane run wrkslots sandbox {pane_id}")
        deadline = time.monotonic() + timeout
        shells = {"bash", "zsh", "sh", "fish", "dash", "ksh"}
        while time.monotonic() < deadline:
            try:
                info = self.process_info(pane_id)
            except HerdrUnavailable:
                # A process mid-exec can briefly report no argv; retry.
                self._sleep(0.05)
                continue
            pids = {entry[0] for entry in info.foreground}
            names = {os.path.basename(entry[3]).lstrip("-") for entry in info.foreground}
            if pids == {info.shell_pid} and names & shells:
                try:
                    with open(f"/proc/{info.shell_pid}/cgroup", encoding="utf-8") as stream:
                        cgroup = stream.read()
                except OSError:
                    cgroup = ""
                if "wrkslots" in cgroup:
                    self._sleep(0.3)  # let the new shell draw its prompt
                    return info.shell_pid
            self._sleep(min(0.1, max(0.0, deadline - time.monotonic())))
        raise HerdrUnavailable(
            f"pane {pane_id} did not return to a wrkslots-boxed shell within {timeout:g}s; "
            "inspect the pane for the wrkslots error"
        )

    def start_agent(
        self, name: str, kind: str, pane_id: str, arguments: Sequence[str] = (),
        *, timeout: float = 30.0,
    ) -> None:
        """Start a visible harness in an existing shell pane (Herdr 0.8 or newer).

        Herdr selects the canonical executable for ``kind``. Arguments are passed
        literally; no shell command or permission-bypass option is synthesized.
        A failed readiness barrier leaves the pane available for inspection.
        """
        if not 0 < timeout <= 300:
            raise ValueError("agent startup timeout must be between 0 and 300 seconds")
        initial: AgentPaneInfo | None = None
        if kind == "codex":
            try:
                initial = self.pane_info(pane_id)
            except HerdrUnavailable:
                # Ordinary native success remains independent of the extra fallback proof.
                pass
        completed = self._invoke(
            ["agent", "start", name, "--kind", kind, "--pane", pane_id,
             "--timeout", str(max(1, int(timeout * 1000))), "--", *arguments],
            timeout=timeout + CONTROL_TIMEOUT_SECONDS,
        )
        if completed.returncode != 0:
            if kind == "codex" and initial is not None and _is_codex_startup_timeout(completed):
                try:
                    if self._codex_ready_after_startup_timeout(name, initial):
                        return
                except HerdrUnavailable:
                    # A failed or unavailable proof does not erase the launch failure.
                    pass
            detail = (completed.stderr or completed.stdout).strip() or f"exit {completed.returncode}"
            raise HerdrUnavailable(f"agent start {name!r}: {detail}")

    def _codex_ready_after_startup_timeout(self, name: str, initial: AgentPaneInfo) -> bool:
        if initial.agent is not None or not initial.terminal_id or "\0" in initial.terminal_id:
            return False
        info = self.pane_info(initial.pane_id)
        if ((info.pane_id, info.workspace_id, info.cwd, info.tab_id, info.terminal_id)
                != (initial.pane_id, initial.workspace_id, initial.cwd, initial.tab_id, initial.terminal_id)):
            return False
        named = self.agent_identity(name)
        if ((named.pane_id, named.tab_id, named.terminal_id)
                != (info.pane_id, info.tab_id, info.terminal_id)):
            return False
        identity = self.harness_identity(info.pane_id, "codex")
        if identity is None or not self.codex_idle_ready(info):
            return False
        return (self.agent_identity(name) == named and self.pane_info(info.pane_id) == info
                and self.verify_harness_identity(info.pane_id, identity))

    def _harness_executable(self, kind: str) -> str:
        if not kind or any(
            not (character.isascii() and (character.islower() or character.isdigit() or character == "-"))
            for character in kind
        ):
            raise HerdrUnavailable(
                "custom harness name must contain lowercase ASCII letters, digits, or hyphens"
            )
        configured = self._environ.get("AGENTCTL_MUSE_BIN") if kind == "muse" else None
        if configured is not None:
            if not os.path.isabs(configured):
                raise HerdrUnavailable("AGENTCTL_MUSE_BIN must be an absolute path")
            return _validated_executable(configured, kind)
        home = self._account_home()
        candidates = (
            os.path.join("/usr/local/bin", kind),
            os.path.join("/usr/bin", kind),
            os.path.join(home, ".local", "bin", kind),
            os.path.join(home, "bin", kind),
            os.path.join(home, ".cargo", "bin", kind),
        )
        candidate = next(
            (path for path in candidates if os.path.isfile(path) and os.access(path, os.X_OK)),
            "",
        )
        if not candidate:
            raise HerdrUnavailable(
                f"custom harness executable {kind!r} was not found in fixed install locations"
            )
        return _validated_executable(candidate, kind)

    def report_pane_agent(self, pane_id: str, kind: str, state: str) -> None:
        """Label one exact custom-harness pane without registering an agent name."""
        self._call_ok(
            [
                "pane", "report-agent", pane_id, "--source", "agentctl",
                "--agent", kind, "--state", state,
                "--message", "agentctl custom harness",
            ],
            f"report custom agent {pane_id}",
        )

    @staticmethod
    def _process_executable(pid: int) -> str | None:
        """Resolve the kernel-owned executable identity, not spoofable argv text."""
        try:
            return os.path.realpath(os.readlink(f"/proc/{pid}/exe"))
        except OSError:
            return None

    def process_liveness(
        self, expected: CustomProcessIdentity,
    ) -> Literal["alive", "dead", "unknown"]:
        """Inspect a recorded local process without confusing probe failure with death.

        Only a changed boot or process generation, or a positive kernel exit
        indication, establishes death. A live process with a different image
        remains unknown. This proof authorizes no input or signal on its own.
        """
        numbers = (expected.version, expected.pid, expected.starttime_ticks,
                   expected.executable_device, expected.executable_inode)
        if (not hasattr(os, "pidfd_open")
                or any(type(value) is not int for value in numbers)
                or expected.version != 1
                or not isinstance(expected.boot_id, str)
                or _BOOT_ID.fullmatch(expected.boot_id) is None
                or not 1 <= expected.pid <= _MAX_PROCESS_ID
                or any(not 1 <= value <= _MAX_U64 for value in numbers[2:])):
            return "unknown"
        try:
            boot = self._liveness_boot_identity()
            if boot != expected.boot_id:
                return "dead" if self._liveness_boot_identity() == boot else "unknown"
            descriptor: int | None = None
            for _attempt in range(4):
                try:
                    descriptor = os.pidfd_open(expected.pid)
                    break
                except InterruptedError:
                    continue
                except OSError as exc:
                    if exc.errno == errno.ESRCH:
                        return "dead" if self._liveness_boot_identity() == boot else "unknown"
                    return "unknown"
            if descriptor is None:
                return "unknown"
            try:
                return self._pinned_process_liveness(expected, descriptor, boot)
            finally:
                os.close(descriptor)
        except (OSError, UnicodeError, ValueError, TypeError):
            return "unknown"

    @staticmethod
    def _liveness_boot_identity() -> str:
        with open("/proc/sys/kernel/random/boot_id", encoding="ascii") as stream:
            value = stream.read(129)
        boot = value.strip()
        if not value or len(value) > 128 or _BOOT_ID.fullmatch(boot) is None:
            raise ValueError("invalid Linux boot identity")
        return boot

    @staticmethod
    def _liveness_process_stat(pid: int) -> ProcessStat:
        with open(f"/proc/{pid}/stat", "rb") as stream:
            value = stream.read(8193)
        parsed = parse_process_stat(value)
        if not value or len(value) > 8192 or parsed is None or parsed.pid != pid or parsed.starttime < 1:
            raise ValueError("invalid Linux process generation")
        return parsed

    @staticmethod
    def _liveness_executable_identity(pid: int) -> tuple[int, int]:
        metadata = os.stat(f"/proc/{pid}/exe")
        if (not stat.S_ISREG(metadata.st_mode)
                or not 1 <= metadata.st_dev <= _MAX_U64
                or not 1 <= metadata.st_ino <= _MAX_U64):
            raise ValueError("invalid Linux executable identity")
        return metadata.st_dev, metadata.st_ino

    @staticmethod
    def _pidfd_exited(descriptor: int) -> bool:
        poller = select.poll()
        poller.register(descriptor, select.POLLIN)
        for _attempt in range(4):
            try:
                events = poller.poll(0)
                break
            except InterruptedError:
                continue
        else:
            raise InterruptedError("pidfd inspection interrupted repeatedly")
        if not events:
            return False
        if (len(events) != 1 or events[0][0] != descriptor
                or not events[0][1] & select.POLLIN
                or events[0][1] & ~(select.POLLIN | select.POLLHUP)):
            raise ValueError("invalid pidfd exit indication")
        return True

    def _pinned_process_liveness(
        self, expected: CustomProcessIdentity, descriptor: int, boot: str,
    ) -> Literal["alive", "dead", "unknown"]:
        if self._pidfd_exited(descriptor):
            return "dead" if self._liveness_boot_identity() == boot else "unknown"
        first = self._liveness_process_stat(expected.pid)
        dead_states = ("Z", "X", "x")
        if first.starttime != expected.starttime_ticks or first.state in dead_states:
            second = self._liveness_process_stat(expected.pid)
            stable_boot = self._liveness_boot_identity() == boot
            self._pidfd_exited(descriptor)
            if (stable_boot and first.starttime == second.starttime
                    and (first.starttime != expected.starttime_ticks
                         or second.state in dead_states)):
                return "dead"
            return "unknown"
        image_before = self._liveness_executable_identity(expected.pid)
        second = self._liveness_process_stat(expected.pid)
        image_after = self._liveness_executable_identity(expected.pid)
        stable_boot = self._liveness_boot_identity() == boot
        exited = self._pidfd_exited(descriptor)
        if not stable_boot or first.starttime != second.starttime or image_before != image_after:
            return "unknown"
        if exited:
            return "dead"
        if second.state in dead_states:
            return "unknown"
        return "alive" if image_before == (expected.executable_device, expected.executable_inode) else "unknown"

    @staticmethod
    def _process_identity(
        pid: int,
    ) -> tuple[CustomProcessIdentity, int, str] | None:
        """Read a coherent Linux process/image identity and kernel executable link."""
        if not hasattr(os, "pidfd_open"):
            return None
        descriptor: int | None = None
        for _attempt in range(4):
            try:
                descriptor = os.pidfd_open(pid)
                break
            except InterruptedError:
                continue
            except OSError:
                return None
        if descriptor is None:
            return None
        try:
            poller = select.poll()
            poller.register(descriptor, select.POLLIN)
            if poller.poll(0):
                return None

            def boot_identity() -> str | None:
                with open("/proc/sys/kernel/random/boot_id", encoding="ascii") as stream:
                    boot_data = stream.read(129)
                if not boot_data or len(boot_data) > 128:
                    return None
                boot_id = boot_data.strip()
                return boot_id if _BOOT_ID.fullmatch(boot_id) is not None else None

            def process_stat() -> tuple[int, int] | None:
                with open(f"/proc/{pid}/stat", "rb") as stream:
                    raw = stream.read(8193)
                if not raw or len(raw) > 8192:
                    return None
                parsed = parse_process_stat(raw)
                if (
                    parsed is None
                    or parsed.pid != pid
                    or parsed.state == "Z"
                    or parsed.pgrp < 1
                    or parsed.starttime < 1
                ):
                    return None
                return parsed.pgrp, parsed.starttime

            boot_before = boot_identity()
            first = process_stat()
            executable_link_before = os.readlink(f"/proc/{pid}/exe")
            executable_before = os.stat(f"/proc/{pid}/exe")
            second = process_stat()
            executable_after = os.stat(f"/proc/{pid}/exe")
            executable_link_after = os.readlink(f"/proc/{pid}/exe")
            boot_after = boot_identity()
            if poller.poll(0):
                return None
        except (OSError, UnicodeError, ValueError):
            return None
        finally:
            try:
                os.close(descriptor)
            except OSError as exc:
                raise HerdrUnavailable(
                    f"cannot close pidfd after process identity proof: {exc}"
                ) from exc
        if (boot_before is None or boot_before != boot_after
                or first is None or second is None or first != second
                or executable_link_before != executable_link_after
                or not stat.S_ISREG(executable_before.st_mode)
                or not stat.S_ISREG(executable_after.st_mode)
                or executable_before.st_dev <= 0 or executable_before.st_ino <= 0
                or executable_before.st_dev > _MAX_U64
                or executable_before.st_ino > _MAX_U64
                or (executable_before.st_dev, executable_before.st_ino)
                != (executable_after.st_dev, executable_after.st_ino)):
            return None
        return (
            CustomProcessIdentity(
                version=1, boot_id=boot_before, pid=pid,
                starttime_ticks=first[1], executable_device=executable_before.st_dev,
                executable_inode=executable_before.st_ino,
            ),
            first[0],
            executable_link_before,
        )

    @staticmethod
    def _supported_shell_name(executable: str) -> bool:
        """Recognize the bounded shell roles supported by Herdr's default policy."""
        name = os.path.basename(executable)
        if name.endswith(" (deleted)"):
            name = name.removesuffix(" (deleted)")
        return name in _SUPPORTED_PANE_SHELLS

    def _supported_shell_identity(
        self, pid: int,
    ) -> tuple[CustomProcessIdentity, int, str] | None:
        """Return one coherent generation only when the kernel image is a known shell."""
        observed = self._process_identity(pid)
        if observed is None or not self._supported_shell_name(observed[2]):
            return None
        return observed

    @staticmethod
    def _process_has_no_descendants(pid: int) -> bool:
        """Boundedly prove that no task in this shell currently has a child."""
        task_root = f"/proc/{pid}/task"

        def proc_parents() -> dict[int, int] | None:
            try:
                entries = tuple(os.listdir("/proc"))
            except OSError:
                return None
            numeric = tuple(
                entry for entry in entries
                if entry.isascii() and entry.isdigit()
            )
            if len(numeric) > _MAX_PROC_ENTRIES:
                return None
            parents: dict[int, int] = {}
            for entry in numeric:
                try:
                    process = int(entry)
                    with open(f"/proc/{entry}/stat", "rb") as stream:
                        raw = stream.read(8193)
                except (FileNotFoundError, ProcessLookupError):
                    continue
                except (OSError, UnicodeError, ValueError):
                    return None
                parsed = parse_process_stat(raw)
                if parsed is None or parsed.pid != process or len(raw) > 8192:
                    return None
                parents[process] = parsed.ppid
            return parents

        def has_descendant(parents: dict[int, int]) -> bool:
            children: dict[int, list[int]] = {}
            for process, parent in parents.items():
                children.setdefault(parent, []).append(process)
            pending = list(children.get(pid, ()))
            seen: set[int] = set()
            while pending:
                process = pending.pop()
                if process in seen:
                    continue
                seen.add(process)
                pending.extend(children.get(process, ()))
            return bool(seen)

        def task_ids() -> tuple[str, ...] | None:
            try:
                names = tuple(sorted(os.listdir(task_root)))
            except OSError:
                return None
            if (not names or len(names) > _MAX_SHELL_TASKS
                    or any(not name.isascii() or not name.isdigit() for name in names)):
                return None
            return names

        before = task_ids()
        if before is None:
            return False
        children_supported = True
        try:
            for task in before:
                with open(
                    os.path.join(task_root, task, "children"),
                    encoding="ascii",
                ) as stream:
                    children = stream.read(_MAX_CHILDREN_BYTES + 1)
                if len(children) > _MAX_CHILDREN_BYTES or children.split():
                    return False
        except FileNotFoundError:
            children_supported = False
        except (OSError, UnicodeError):
            return False
        if children_supported:
            return task_ids() == before
        first = proc_parents()
        second = proc_parents()
        return (first is not None and second is not None
                and not has_descendant(first) and not has_descendant(second)
                and task_ids() == before)

    def _idle_shell_proof(self, info: ProcessInfo) -> PaneShellProof | None:
        if (info.foreground_pgid != info.shell_pid or len(info.foreground) != 1
                or info.foreground[0][0] != info.shell_pid):
            return None
        _pid, _name, _command, argv0, _reported = info.foreground[0]
        if not argv0:
            return None
        observed = self._supported_shell_identity(info.shell_pid)
        if observed is None or observed[1] != info.foreground_pgid:
            return None
        executable_path = os.path.realpath(observed[2])
        if executable_path != os.path.realpath(argv0):
            return None
        if not self._process_has_no_descendants(info.shell_pid):
            return None
        confirmed = self._supported_shell_identity(info.shell_pid)
        if confirmed != observed:
            return None
        return PaneShellProof(observed[0], executable_path)

    def pane_idle_shell_identity(self, pane_id: str) -> PaneShellProof | None:
        """Return one stable idle-shell proof, including absence of descendants."""
        before = self.process_info(pane_id)
        proof = self._idle_shell_proof(before)
        if proof is None:
            return None
        after = self.process_info(pane_id)
        if after != before or self._idle_shell_proof(after) != proof:
            return None
        return proof

    def _pane_process_identity(
        self, info: ProcessInfo, executable: str | None,
        expected: CustomProcessIdentity | None = None,
        launch_image: tuple[int, int] | None = None,
    ) -> CustomProcessIdentity | None:
        matches: list[CustomProcessIdentity] = []
        for pid, _process_name, _command, argv0, reported_executable in info.foreground:
            if expected is not None and pid != expected.pid:
                continue
            if expected is None and (
                executable is None or (argv0 and os.path.realpath(argv0) != executable)
                or (not argv0 and launch_image is None)
            ):
                continue
            observed_path = self._process_executable(pid)
            observed = self._process_identity(pid)
            if (observed is None and observed_path is None
                    and not self._production_runner and argv0
                    and reported_executable is not None):
                try:
                    metadata = os.stat(reported_executable)
                except OSError:
                    continue
                observed = (
                    CustomProcessIdentity(
                        version=1, boot_id="00000000-0000-0000-0000-000000000000",
                        pid=pid, starttime_ticks=pid,
                        executable_device=metadata.st_dev, executable_inode=metadata.st_ino,
                    ),
                    info.foreground_pgid,
                    reported_executable,
                )
            if observed is None:
                continue
            identity, process_group_id, _observed_executable = observed
            if process_group_id != info.foreground_pgid:
                continue
            if expected is not None:
                if identity == expected:
                    matches.append(identity)
                continue
            if launch_image is not None and (
                identity.executable_device, identity.executable_inode
            ) != launch_image:
                continue
            if launch_image is not None or observed_path == executable or (
                not self._production_runner and reported_executable is not None
                and os.path.realpath(reported_executable) == executable
            ):
                matches.append(identity)
        return matches[0] if len(matches) == 1 else None

    def verify_custom_harness(
        self, pane_id: str, kind: str,
        expected_identity: CustomProcessIdentity | None = None,
    ) -> None:
        """Require the recorded custom process or strict current-path identity."""
        executable = None if expected_identity is not None else self._harness_executable(kind)
        if self._pane_process_identity(
            self.process_info(pane_id), executable, expected_identity
        ) is None:
            raise HerdrUnavailable(
                f"custom harness {kind!r} is not the foreground process in pane {pane_id}"
            )

    def pane_is_idle_shell(self, pane_id: str) -> bool:
        """Prove the pane has returned to Herdr's original shell process group."""
        return self.pane_idle_shell_identity(pane_id) is not None

    def pane_shell_identity(self, pane_id: str) -> CustomProcessIdentity:
        """Capture the kernel identity of the shell process Herdr owns for one pane."""
        info = self.process_info(pane_id)
        observed = self._supported_shell_identity(info.shell_pid)
        if observed is None or observed[1] != info.shell_pid:
            raise HerdrUnavailable(
                f"cannot capture a supported identity-bound shell process for pane {pane_id}"
            )
        return observed[0]

    def pane_is_same_idle_shell(
        self, pane_id: str, expected: CustomProcessIdentity,
    ) -> bool:
        """Prove both idle-shell state and the exact shell generation captured earlier."""
        proof = self.pane_idle_shell_identity(pane_id)
        return proof is not None and proof.identity == expected

    @staticmethod
    def _process_cgroup(pid: int) -> str | None:
        try:
            with open(f"/proc/{pid}/cgroup", encoding="utf-8") as stream:
                return stream.read(4097)
        except OSError:
            return None

    @staticmethod
    def _process_children_map() -> dict[int, list[int]]:
        """Parent -> children for every visible process, from world-readable stat files.

        A root-owned process's own task directory is not readable by the user,
        so descendants of sudo are found through their parent links instead.
        """
        children: dict[int, list[int]] = {}
        try:
            entries = [entry for entry in os.listdir("/proc") if entry.isascii() and entry.isdigit()]
        except OSError:
            return children
        for entry in entries[:_MAX_PROC_ENTRIES]:
            try:
                with open(f"/proc/{entry}/stat", "rb") as stream:
                    raw = stream.read(8193)
            except OSError:
                continue
            parsed = parse_process_stat(raw)
            if parsed is not None and parsed.pid == int(entry):
                children.setdefault(parsed.ppid, []).append(parsed.pid)
        return children

    def relay_process(self, pane_id: str) -> CustomProcessIdentity | None:
        """Identify the program a slot's root relay runs in one pane, or None.

        Under root slot isolation the pane's shell PID execs into sudo, which
        keeps the terminal and runs the boxed program on a private pty behind
        a monitor process. The program is the first process below the pane's
        shell PID that belongs to this user and shares its wrkslots slot scope.
        The pane lives exactly as long as that chain, so a later generation in
        the same pane cannot be mistaken for this one.
        """
        info = self.process_info(pane_id)
        if (len(info.foreground) != 1 or info.foreground[0][0] != info.shell_pid
                or info.foreground_pgid != info.shell_pid
                or os.path.basename(info.foreground[0][3]) != "sudo"):
            return None
        scope = self._process_cgroup(info.shell_pid)
        if scope is None or "wrkslots" not in scope:
            return None
        tree = self._process_children_map()
        frontier, seen = [info.shell_pid], {info.shell_pid}
        for _depth in range(4):
            following: list[int] = []
            for parent in frontier:
                for child in sorted(tree.get(parent, ())):
                    if child in seen:
                        continue
                    seen.add(child)
                    try:
                        owner = os.stat(f"/proc/{child}").st_uid
                    except OSError:
                        continue
                    if owner == os.getuid() and self._process_cgroup(child) == scope:
                        observed = self._process_identity(child)
                        return None if observed is None else observed[0]
                    following.append(child)
            frontier = following
        return None

    def verify_relay_harness(self, pane_id: str, expected: CustomProcessIdentity | None) -> None:
        """Require the recorded program to still run behind this pane's root relay."""
        observed = self.relay_process(pane_id)
        if observed is None or expected is None or observed != expected:
            raise HerdrUnavailable(
                f"the recorded program is not running behind the slot relay in pane {pane_id}"
            )

    def explain_agent(self, pane_id: str) -> tuple[str | None, str]:
        """Herdr's screen-rule verdict for one pane: (agent kind, state).

        Herdr keeps a reported pane state as reported; its bundled screen rules
        are evaluated live only by ``agent explain``.
        """
        completed = self._invoke(["agent", "explain", pane_id])
        if completed.returncode != 0:
            detail = (completed.stderr or completed.stdout or "").strip() or f"exit {completed.returncode}"
            raise HerdrUnavailable(f"agent explain {pane_id}: {detail}")
        fields: dict[str, str] = {}
        for line in completed.stdout.splitlines():
            key, separator, value = line.partition(":")
            if (separator and key.strip() in ("agent", "state", "rule", "fallback_reason")
                    and key.strip() not in fields):
                fields[key.strip()] = value.strip()
        state = fields.get("state", "unknown")
        # A default for a known agent with no matching rule is not screen evidence.
        if (state not in ("idle", "working", "blocked", "done", "unknown")
                or fields.get("rule", "none") == "none" or "fallback_reason" in fields):
            state = "unknown"
        return fields.get("agent") or None, state

    def start_relay_agent(
        self, kind: str, pane_id: str, command_line: str, *, timeout: float = 30.0,
        on_observed: Callable[[CustomProcessIdentity], None] | None = None,
    ) -> CustomProcessIdentity:
        """Run a slot-boxed harness behind a root relay in a fresh pane and prove it ready.

        Herdr cannot start an agent there (sudo, not a shell, is the pane's
        foreground process), so the boxed harness line is run directly, the
        relayed program is pinned, and readiness comes from Herdr's live screen
        rules, which Herdr applies once the pane is reported as ``kind``.
        """
        if not 0 < timeout <= 300:
            raise ValueError("agent startup timeout must be between 0 and 300 seconds")
        self._call_ok(["pane", "run", pane_id, command_line], f"pane run slot relay {kind!r}")
        deadline = time.monotonic() + timeout
        observed: CustomProcessIdentity | None = None
        while time.monotonic() < deadline and observed is None:
            try:
                observed = self.relay_process(pane_id)
            except HerdrUnavailable:
                observed = None
            if observed is None:
                self._sleep(min(0.1, max(0.0, deadline - time.monotonic())))
        if observed is None:
            raise HerdrUnavailable(
                f"no slot relay program appeared in pane {pane_id}; inspect the pane for the wrkslots error"
            )
        if on_observed is not None:
            on_observed(observed)
        # Herdr evaluates screen rules only for a pane it knows as an agent.
        self.report_pane_agent(pane_id, kind, "unknown")
        state = "unknown"
        while time.monotonic() < deadline:
            # The program may exec while it starts; any relay program still running counts.
            if self.relay_process(pane_id) is None:
                raise HerdrUnavailable(
                    f"the slot relay program in pane {pane_id} exited during startup"
                )
            if relay_trust_prompt(self.read(pane_id, source="visible", lines=200)):
                raise HerdrUnavailable(
                    f"{kind} workspace trust prompt requires human attention; no input was submitted"
                )
            agent_kind, state = self.explain_agent(pane_id)
            # Herdr's title rule can read idle before the harness has drawn
            # anything; the verified composer on screen is the ready signal.
            if (agent_kind == kind and state == "idle"
                    and composer_view(kind, self.read_screen(pane_id)) is not None):
                break
            if agent_kind == kind and state == "blocked":
                raise HerdrUnavailable(
                    f"{kind} is waiting for human attention (for example a workspace trust prompt); "
                    "no input was submitted"
                )
            self._sleep(min(0.2, max(0.0, deadline - time.monotonic())))
        else:
            raise HerdrUnavailable(
                f"{kind} behind the slot relay did not reach an idle screen in pane {pane_id} "
                f"within {timeout:g}s (last state {state})"
            )
        # A relayed program may exec more than once while it starts; pin the
        # generation that reached the idle screen.
        settled = self.relay_process(pane_id)
        if settled is None:
            raise HerdrUnavailable(f"the slot relay program in pane {pane_id} exited during startup")
        if on_observed is not None and settled != observed:
            on_observed(settled)
        self.report_pane_agent(pane_id, kind, "idle")
        return settled

    def start_pane_agent(
        self, name: str, kind: str, pane_id: str, arguments: Sequence[str] = (),
        *, timeout: float = 30.0,
        on_observed: Callable[[CustomProcessIdentity], None] | None = None,
    ) -> CustomProcessIdentity:
        """Launch and verify a custom TUI through one exact Herdr pane.

        Unknown harness kinds cannot use Herdr's native ``agent start`` path.
        No trust-prompt input is synthesized by this adapter.
        """
        del name
        if not 0 < timeout <= 300:
            raise ValueError("agent startup timeout must be between 0 and 300 seconds")
        executable = self._harness_executable(kind)
        try:
            descriptor = os.open(executable, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
        except OSError as exc:
            raise HerdrUnavailable(f"cannot pin {kind} executable {executable}: {exc}") from exc
        try:
            metadata = os.fstat(descriptor)
            if (not stat.S_ISREG(metadata.st_mode) or metadata.st_mode & 0o111 == 0
                    or metadata.st_mode & (stat.S_IWGRP | stat.S_IWOTH)
                    or metadata.st_dev <= 0 or metadata.st_ino <= 0
                    or metadata.st_dev > _MAX_U64 or metadata.st_ino > _MAX_U64):
                raise HerdrUnavailable(
                    f"refusing unsafe {kind} executable after opening: {executable}"
                )
            if os.pread(descriptor, 4, 0) != b"\x7fELF":
                raise HerdrUnavailable(
                    f"custom harness {kind!r} must be a native ELF executable"
                )
            launch_image = (metadata.st_dev, metadata.st_ino)
            self._call_ok(
                ["pane", "run", pane_id, shlex.join([executable, *arguments])],
                f"pane run {kind!r}",
            )
            deadline = time.monotonic() + timeout
            observed: CustomProcessIdentity | None = None
            while time.monotonic() < deadline:
                observed = self._pane_process_identity(
                    self.process_info(pane_id), executable, launch_image=launch_image
                )
                if observed is not None:
                    break
                self._sleep(min(0.05, max(0.0, deadline - time.monotonic())))
            if observed is None:
                raise HerdrUnavailable(
                    f"custom harness {kind!r} was not the foreground process in pane {pane_id}"
                )
            if on_observed is not None:
                on_observed(observed)
        finally:
            os.close(descriptor)
        ready = False
        while time.monotonic() < deadline:
            self.verify_custom_harness(pane_id, kind, observed)
            screen = self.read(pane_id, source="visible", lines=200)
            if muse_trust_prompt(screen) and "--trust-workspace" not in arguments:
                raise HerdrUnavailable(
                    f"{kind} workspace trust prompt requires human attention; no input was submitted"
                )
            # An arrow alone is also used by choice dialogs. Muse's idle screen
            # couples its composer marker with the Auto-review status label.
            ready = muse_idle_composer(screen)
            if ready:
                break
            self._sleep(min(0.05, max(0.0, deadline - time.monotonic())))
        if not ready:
            raise HerdrUnavailable(
                f"custom harness {kind!r} did not reach a verified idle composer in pane {pane_id}"
            )
        self.verify_custom_harness(pane_id, kind, observed)
        self.report_pane_agent(pane_id, kind, "idle")
        return observed

    def agent_prompt(
        self, pane_id: str, text: str, *, expect_terminal: str | None = None,
    ) -> None:
        """Submit text plus Enter through Herdr's native prompt primitive."""
        self._input_ok(
            ["agent", "prompt", pane_id, text], f"agent prompt {pane_id}", expect_terminal,
        )

    def agent_identity(self, name: str) -> AgentIdentity:
        """Resolve an exact live Herdr agent name to its pane, tab and terminal."""
        result = self._call(["agent", "get", name], f"agent get {name!r}")
        try:
            info = as_mapping(result.get("agent"), "agent get")
            if get_str(info, "name", "agent get") != name:
                raise HerdrUnavailable(f"agent get: returned a different agent name for {name!r}")
            return AgentIdentity(
                name=name,
                pane_id=get_str(info, "pane_id", "agent get"),
                tab_id=opt_str(info, "tab_id"),
                terminal_id=opt_str(info, "terminal_id"),
            )
        except TypeError as exc:
            raise HerdrUnavailable(f"agent get: invalid Herdr response: {exc}") from exc

    def rename_agent(self, pane_id: str, name: str) -> None:
        """Give the agent in one exact pane a new Herdr agent name."""
        self._call_ok(["agent", "rename", pane_id, name], f"agent rename {pane_id}")

    def agent_names(self) -> dict[str, str]:
        """Every named Herdr agent, mapped to its pane."""
        result = self._call(["agent", "list"], "agent list")
        names: dict[str, str] = {}
        try:
            for entry in as_sequence(result.get("agents"), "agent list"):
                item = as_mapping(entry, "agent list entry")
                name = opt_str(item, "name")
                if name is not None:
                    names[name] = get_str(item, "pane_id", "agent list entry")
        except TypeError as exc:
            raise HerdrUnavailable(f"agent list: invalid Herdr response: {exc}") from exc
        return names

    def tab_labels(self, workspace_id: str) -> dict[str, str]:
        """Every tab of one workspace, mapped to its label."""
        result = self._call(["tab", "list", "--workspace", workspace_id], "tab list")
        labels: dict[str, str] = {}
        try:
            for entry in as_sequence(result.get("tabs"), "tab list"):
                tab = as_mapping(entry, "tab list entry")
                labels[get_str(tab, "tab_id", "tab list entry")] = get_str(
                    tab, "label", "tab list entry"
                )
        except TypeError as exc:
            raise HerdrUnavailable(f"tab list: invalid Herdr response: {exc}") from exc
        return labels

    def tab_label(self, tab_id: str) -> str:
        """Return the label of one exact tab."""
        result = self._call(["tab", "get", tab_id], f"tab get {tab_id}")
        try:
            tab = as_mapping(result.get("tab"), "tab get")
            if get_str(tab, "tab_id", "tab get") != tab_id:
                raise HerdrUnavailable(f"tab get: returned a different tab for {tab_id!r}")
            return get_str(tab, "label", "tab get")
        except TypeError as exc:
            raise HerdrUnavailable(f"tab get: invalid Herdr response: {exc}") from exc

    def harness_identity(self, pane_id: str, kind: str) -> CustomProcessIdentity | None:
        """Pin the pane's harness process when it is the only foreground process.

        A foreground group with several processes (a launcher and the program it
        started, or the harness running a foreground child) is ambiguous: the pinned
        process could be a launcher that outlives the harness. ``None`` then, and when
        the kernel identity cannot be read coherently. ``kind`` is the expected harness.
        """
        del kind
        info = self.process_info(pane_id)
        if info.foreground_pgid == info.shell_pid or len(info.foreground) != 1:
            return None
        pid = info.foreground[0][0]
        observed = self._process_identity(pid)
        if observed is None or observed[1] != info.foreground_pgid:
            return None
        return observed[0]

    def verify_harness_identity(
        self, pane_id: str, expected: CustomProcessIdentity,
    ) -> bool:
        """Is the pinned harness process, the same kernel lifetime, still the pane's only
        foreground process? (An anchor pinned to a launcher therefore fails.)"""
        info = self.process_info(pane_id)
        if len(info.foreground) != 1:
            return False
        return self._pane_process_identity(info, None, expected) is not None

    def codex_idle_ready(self, info: AgentPaneInfo) -> bool:
        """Prove unknown Codex readiness with one stable foreground generation.

        Reported idle/done states keep their existing readiness behavior. This
        additional proof never changes the reported status or authorizes input.
        """
        if (info.agent != "codex" or info.status != "unknown"
                or not info.terminal_id or "\0" in info.terminal_id
                or (info.session_agent is None) != (info.session_value is None)
                or (info.session_agent is not None and (
                    info.session_agent != "codex" or not info.session_value
                    or "\0" in info.session_value
                ))):
            return False
        identity = self.harness_identity(info.pane_id, "codex")
        if identity is None or not codex_idle_composer(self.read_screen(info.pane_id)):
            return False
        return (self.pane_info(info.pane_id) == info
                and self.verify_harness_identity(info.pane_id, identity))

    def agent_pane(self, name: str) -> str:
        """Resolve an exact live Herdr agent name, rejecting a stale pane occupant."""
        result = self._call(["agent", "get", name], f"agent get {name!r}")
        try:
            info = as_mapping(result.get("agent"), "agent get")
            if get_str(info, "name", "agent get") != name:
                raise HerdrUnavailable(f"agent get: returned a different agent name for {name!r}")
            return get_str(info, "pane_id", "agent get")
        except TypeError as exc:
            raise HerdrUnavailable(f"agent get: invalid Herdr response: {exc}") from exc

    def report_agent_session(self, name: str, pane_id: str, kind: str, session_id: str) -> None:
        """Publish an explicitly supplied native session identity on its named pane."""
        if self.agent_pane(name) != pane_id:
            raise HerdrUnavailable("cannot bind a session to a different named agent pane")
        self._call_ok(
            ["pane", "report-agent-session", pane_id, "--source", "herdr-agent",
             "--agent", kind, "--agent-session-id", session_id],
            "report managed agent session",
        )

    def panes(self, workspace_id: str | None = None) -> tuple[Pane, ...]:
        """Every pane, optionally restricted to one workspace."""
        args = ["pane", "list"]
        if workspace_id is not None:
            args += ["--workspace", workspace_id]
        result = self._call(args, "pane list")
        out: list[Pane] = []
        try:
            for entry in as_sequence(result.get("panes"), "pane list"):
                pane = as_mapping(entry, "pane list entry")
                parsed = Pane(
                    pane_id=get_str(pane, "pane_id", "pane list entry"),
                    tab_id=get_str(pane, "tab_id", "pane list entry"),
                    workspace_id=get_str(pane, "workspace_id", "pane list entry"),
                )
                if workspace_id is not None and parsed.workspace_id != workspace_id:
                    raise HerdrUnavailable(
                        f"pane list: returned pane {parsed.pane_id!r} from workspace "
                        f"{parsed.workspace_id!r}, expected {workspace_id!r}"
                    )
                out.append(parsed)
        except TypeError as exc:
            raise HerdrUnavailable(f"pane list: invalid Herdr response: {exc}") from exc
        return tuple(out)

    def move_pane_to_new_tab(
        self, pane_id: str, *, workspace_id: str, tab_label: str,
    ) -> Pane:
        """Move one pane into a fresh labelled tab without stealing focus."""
        result = self._call(
            [
                "pane", "move", pane_id, "--new-tab", "--workspace",
                workspace_id, "--label", tab_label, "--no-focus",
            ],
            f"pane move {pane_id}",
        )
        try:
            moved = as_mapping(result.get("move_result"), "pane move")
            if moved.get("changed") is not True:
                raise HerdrUnavailable(
                    "pane move: Herdr reported that no move occurred"
                )
            if get_str(moved, "previous_pane_id", "pane move") != pane_id:
                raise HerdrUnavailable(
                    "pane move: previous pane identity did not match the request"
                )
            raw = as_mapping(moved.get("pane"), "pane move")
            pane = Pane(
                pane_id=get_str(raw, "pane_id", "pane move"),
                tab_id=get_str(raw, "tab_id", "pane move"),
                workspace_id=get_str(raw, "workspace_id", "pane move"),
            )
        except TypeError as exc:
            raise HerdrUnavailable(
                f"pane move: invalid Herdr response: {exc}"
            ) from exc
        if pane.workspace_id != workspace_id:
            raise HerdrUnavailable(
                f"pane move: returned workspace {pane.workspace_id!r}, "
                f"expected {workspace_id!r}"
            )
        return pane

    def pane_exists(self, pane_id: str) -> bool:
        """Is this pane id still live? Used to invalidate a cached id rather than trust it."""
        try:
            self._call(["pane", "get", pane_id], f"pane get {pane_id}")
        except HerdrUnavailable:
            return False
        return True

    def pane_info(self, pane_id: str) -> AgentPaneInfo:
        """Return validated identity/readiness data for an interactive pane."""
        result = self._call(["pane", "get", pane_id], f"pane get {pane_id}")
        try:
            pane = as_mapping(result.get("pane"), "pane get")
            returned = get_str(pane, "pane_id", "pane get")
            if returned != pane_id:
                raise HerdrUnavailable(
                    f"pane get: returned pane {returned!r}, expected {pane_id!r}"
                )
            session_raw = pane.get("agent_session")
            session_agent: str | None = None
            session_value: str | None = None
            if session_raw is not None:
                session = as_mapping(session_raw, "pane agent_session")
                session_agent = opt_str(session, "agent")
                session_value = opt_str(session, "value")
            return AgentPaneInfo(
                pane_id=returned,
                workspace_id=get_str(pane, "workspace_id", "pane get"),
                cwd=get_str(pane, "cwd", "pane get"),
                agent=opt_str(pane, "agent"),
                status=opt_str(pane, "agent_status") or "unknown",
                session_agent=session_agent,
                session_value=session_value,
                terminal_id=opt_str(pane, "terminal_id"),
                tab_id=opt_str(pane, "tab_id"),
            )
        except TypeError as exc:
            raise HerdrUnavailable(f"pane get: invalid Herdr response: {exc}") from exc

    def workspace_label(self, workspace_id: str) -> str:
        """Return the label for one exact workspace id."""
        result = self._call(["workspace", "get", workspace_id], "workspace get")
        try:
            workspace = as_mapping(result.get("workspace"), "workspace get")
            returned = get_str(workspace, "workspace_id", "workspace get")
            if returned != workspace_id:
                raise HerdrUnavailable(
                    f"workspace get: returned workspace {returned!r}, expected {workspace_id!r}"
                )
            return get_str(workspace, "label", "workspace get")
        except TypeError as exc:
            raise HerdrUnavailable(f"workspace get: invalid Herdr response: {exc}") from exc

    def wait_agent_status(self, pane_id: str, status: str, timeout_ms: int) -> None:
        """Wait for a native Herdr agent-state transition."""
        purpose = f"wait for pane {pane_id} status {status}"
        completed = self._invoke(
            ["agent", "wait", pane_id, "--until", status, "--timeout", str(timeout_ms)],
            timeout=max(CONTROL_TIMEOUT_SECONDS, timeout_ms / 1000.0 + 5.0),
        )
        if completed.returncode != 0:
            detail = (completed.stderr or completed.stdout or "").strip() or f"exit {completed.returncode}"
            raise HerdrUnavailable(f"{purpose}: {detail}")
        try:
            envelope = as_mapping(json.loads(completed.stdout), purpose)
            result = as_mapping(envelope.get("result"), purpose)
            data = as_mapping(result.get("agent"), purpose)
            returned_pane = get_str(data, "pane_id", purpose)
            returned_status = get_str(data, "agent_status", purpose)
        except (json.JSONDecodeError, TypeError) as exc:
            raise HerdrUnavailable(f"{purpose}: invalid Herdr event response: {exc}") from exc
        if returned_pane != pane_id or returned_status != status:
            raise HerdrUnavailable(
                f"{purpose}: event reported pane={returned_pane!r} status={returned_status!r}"
            )

    def event_socket(self) -> str:
        """Resolve the running server's event socket through Herdr's own discovery."""
        completed = self._invoke(["status", "server", "--json"])
        if completed.returncode:
            raise HerdrUnavailable("cannot discover the running Herdr server for output subscriptions")
        try:
            document = as_mapping(json.loads(completed.stdout), "Herdr server status")
            path = get_str(document, "socket", "Herdr server status")
            if document.get("running") is not True or document.get("compatible") is not True:
                raise HerdrUnavailable("output subscriptions require a running, compatible Herdr server")
            if not os.path.isabs(path) or "\0" in path:
                raise ValueError("event socket must be an absolute path")
            return path
        except (TypeError, ValueError) as exc:
            raise HerdrUnavailable(f"invalid Herdr server status: {exc}") from exc

    def process_info(self, pane_id: str) -> ProcessInfo:
        """The pane's live shell pid and foreground process group — the readiness signal."""
        result = self._call(
            ["pane", "process-info", "--pane", pane_id], f"pane process-info {pane_id}"
        )
        try:
            info = as_mapping(result.get("process_info"), "pane process-info")
            foreground: list[tuple[int, str, str, str, str | None]] = []
            for entry in as_sequence(
                info.get("foreground_processes"), "foreground_processes"
            ):
                process = as_mapping(entry, "foreground process")
                raw_argv = process.get("argv")
                argv = () if raw_argv is None else as_sequence(raw_argv, "foreground process argv")
                if argv and (
                    any(not isinstance(argument, str) for argument in argv)
                    or not argv[0] or "\0" in str(argv[0])
                ):
                    raise TypeError("foreground process argv must have string entries and a nonempty NUL-free argv[0]")
                foreground.append(
                    (
                        _get_process_id(process, "pid", "foreground process"),
                        opt_str(process, "name") or "",
                        opt_str(process, "cmdline") or "",
                        str(argv[0]) if argv else "",
                        opt_str(process, "executable"),
                    )
                )
            parsed = ProcessInfo(
                pane_id=get_str(info, "pane_id", "pane process-info"),
                shell_pid=_get_process_id(info, "shell_pid", "pane process-info"),
                foreground_pgid=_get_process_id(
                    info, "foreground_process_group_id", "pane process-info"
                ),
                foreground=tuple(foreground),
            )
            if parsed.pane_id != pane_id:
                raise HerdrUnavailable(
                    f"pane process-info: returned pane {parsed.pane_id!r}, expected {pane_id!r}"
                )
            return parsed
        except TypeError as exc:
            raise HerdrUnavailable(
                f"pane process-info: invalid Herdr response: {exc}"
            ) from exc

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
            detail = (completed.stderr or "").strip() or f"exit {completed.returncode}"
            raise HerdrUnavailable(f"pane read {pane_id}: {detail}")
        return completed.stdout

    def _call_ok(self, args: Sequence[str], purpose: str) -> None:
        """Invoke a subcommand that reports success only through its exit status.

        ``pane run`` and ``pane send-keys`` write NOTHING on success — they are input-injection
        calls, not queries — so requiring a JSON envelope here would fail every successful call.
        """
        completed = self._invoke(args)
        if completed.returncode != 0:
            detail = (
                completed.stderr or completed.stdout or ""
            ).strip() or f"exit {completed.returncode}"
            raise HerdrUnavailable(f"{purpose}: {detail}")


    def prompt_agent(
        self, pane_id: str, text: str, *, terminal: GuardedInput | None = None,
    ) -> SubmissionReceipt | None:
        """Submit one prompt, proving the submission from the screen when the harness allows it.

        For harnesses with a known composer layout the text is pasted into an
        empty composer, the harness's submission key is sent and repeated with
        bounded backoff while the text stays staged, and a receipt is returned
        only once the text has left the composer with corroborating screen
        evidence. ``PromptNotStaged`` means nothing was typed.

        Slash commands open harness menus and confirmation dialogs rather than
        submitting through the composer, and other harnesses have no composer
        model, so both use the native ``agent prompt`` primitive and return
        ``None``: the caller must then confirm submission some other way.

        ``terminal``, when given, performs every input effect, so a caller can
        verify the recipient immediately around each one.
        """
        try:
            harness = self.pane_info(pane_id).agent or ""
        except HerdrUnavailable as exc:
            raise PromptNotStaged(
                f"pane {pane_id}: harness lookup failed before typing: {exc}"
            ) from exc
        if harness in VERIFIED_HARNESSES and not text.lstrip().startswith("/"):
            return submit_verified(terminal or self, pane_id, harness, text)
        if terminal is None:
            self._call_ok(["agent", "prompt", pane_id, text], f"agent prompt {pane_id}")
        else:
            terminal.native_prompt(pane_id, text)
        return None

    def read_scrollback(self, pane_id: str) -> str:
        """Read recent scrollback without wrapping, to find text a prompt left there."""
        return self.read(pane_id, source="recent-unwrapped", lines=400)

    def read_screen(self, pane_id: str) -> str:
        """Read the visible rows with SGR styling retained for composer inspection."""
        args = [
            "pane", "read", pane_id, "--source", "visible",
            "--lines", str(SUBMISSION_SCREEN_LINES), "--format", "ansi",
        ]
        completed = self._invoke(args)
        if completed.returncode != 0:
            detail = (completed.stderr or "").strip() or f"exit {completed.returncode}"
            raise HerdrUnavailable(f"pane read {pane_id}: {detail}")
        return completed.stdout

    def input_expect_supported(self) -> bool:
        """Does the running server refuse input whose expected terminal does not match?

        Read once from the ``capabilities`` line of ``herdr status server``. The protocol
        number cannot answer this: a server without the capability ignores the field.
        """
        if self._input_expect is None:
            completed = self._invoke(["status", "server"])
            supported = False
            if completed.returncode == 0:
                for line in completed.stdout.splitlines():
                    key, _, value = line.partition(":")
                    if key.strip() == "capabilities":
                        supported = "input-expect" in value.replace(",", " ").split()
            self._input_expect = supported
        return self._input_expect

    def _input_ok(
        self, args: list[str], purpose: str, expect_terminal: str | None,
    ) -> None:
        if expect_terminal is not None:
            args[2:2] = ["--expect-terminal", expect_terminal]
        completed = self._invoke(args)
        if completed.returncode != 0:
            detail = (
                completed.stderr or completed.stdout or ""
            ).strip() or f"exit {completed.returncode}"
            if expect_terminal is not None and '"code":"expectation_failed"' in detail.replace(" ", ""):
                raise InputExpectationFailed(f"{purpose}: {detail}")
            raise HerdrUnavailable(f"{purpose}: {detail}")

    def send_text(
        self, pane_id: str, text: str, *, expect_terminal: str | None = None,
    ) -> None:
        """Insert literal text without synthesizing a submission keystroke.

        With ``expect_terminal`` the server writes nothing unless the pane still holds
        that terminal; refusal raises ``InputExpectationFailed``.
        """
        self._input_ok(
            ["pane", "send-text", pane_id, text], f"pane send-text {pane_id}", expect_terminal,
        )

    def send_keys(
        self, pane_id: str, keys: str, *, expect_terminal: str | None = None,
    ) -> None:
        """Send named key presses (for example ``ctrl+u``) to a pane."""
        self._input_ok(
            ["pane", "send-keys", pane_id, keys], f"pane send-keys {pane_id}", expect_terminal,
        )

    def close_pane(self, pane_id: str) -> None:
        """Close one exact pane, preserving any other panes added to its tab."""
        self._call_ok(["pane", "close", pane_id], f"pane close {pane_id}")

    def focus_tab(self, tab_id: str) -> None:
        """Focus an exact tab, including headless presentations without a detected agent."""
        self._call_ok(["tab", "focus", tab_id], f"tab focus {tab_id}")

    def focus_pane(self, pane_id: str) -> None:
        """Focus an existing verified pane for direct human interaction."""
        self._call_ok(["agent", "focus", pane_id], f"agent focus {pane_id}")
