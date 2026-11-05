#!/usr/bin/env python3
"""Black-box Python/Rust differential for :command:`herdr-run`.

The commands under test are supplied by the caller as complete argv vectors.  This keeps the
harness independent of both packaging layouts and, importantly, never invokes a shell to locate or
compose either implementation.  The comparison covers bootstrap behavior, every help screen and
document, the structured option surface, the command-policy boundary, strict configuration
loading/discovery with every configuration message, argument grammar, agent inference, bytes that
are not UTF-8, locales, output failures and signals, OS error text, the audited dry-run path, the
spool, reap over planted records, paths and names a terminal must not see raw, and what the
editions hand to the processes they start: environment, descriptors, and signal dispositions.

The editions are invoked directly -- the Python package under the real interpreter, the Rust
binary itself -- because what is compared is the TOOL. ``common/bin/herdr-run`` is exercised only
as a smoke test whose launcher-written lines are stripped: the launcher is shared plumbing for every
tool, and its own behaviour (see ``cross/README.md``) is not herdr-run's.

Output is compared as BYTES: both streams are decoded with ``surrogateescape``, so a byte that is
not UTF-8 survives the comparison instead of being replaced by U+FFFD in both editions alike.

Both implementations resolve ``herdr`` only from trusted installation locations under the account
home the account database reports, and ignore caller-controlled ``PATH`` and ``HOME``. The
end-to-end group keeps that contract and changes the database's answer instead: a shim compiled
here at test time and preloaded into both editions reports a temporary directory as the account
home, a fake Herdr (``herdr_e2e/fake_herdr.py``) is installed under it, and both production
clients run against it. The shim also refuses to start ``systemd-run`` or any other Herdr. Every
other invocation that may reach a Herdr (``status``, ``reap``, the unsafe-path cases) runs behind
the same shim against its own fake, so nothing here talks to a Herdr installed on the host. The
fake records, for every call, the environment it was handed (entry by entry, as the kernel holds
it) and its ignored signals, and both are compared. The golden corpora under
``rs/herdr-run/testdata/``, which both editions' unit tests load, still pin each protocol call,
readiness rule, and record format on their own.

Only values which are intrinsically different between two processes are normalized: temporary
roots, run IDs, timestamps, process IDs, and measured durations.  Policy text, quoting, exit codes,
JSON formatting, and diagnostics otherwise remain observable.
"""

from __future__ import annotations

import argparse
import errno
import fcntl
import json
import os
import re
import shlex
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import termios
import time
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import cast

REPO_ROOT = Path(__file__).resolve().parent.parent
TIMEOUT_SECONDS = 30

#: The documented launcher every engine is reached through.
LAUNCHER = REPO_ROOT / "common" / "bin" / "herdr-run"

#: Placeholder for each edition's own project root inside arguments and environment values.
ROOT_TOKEN = "<ROOT>"

_RUN_ID_VALUE = re.compile(r'(?P<prefix>"run_id"\s*:\s*)"[^"]*"')
_RUN_ID_PATH = re.compile(r"\b\d{8}T\d{6}-[A-Za-z0-9_.-]+-\d+(?:-\d+)?\b")
_TIME_VALUE = re.compile(
    r'(?P<prefix>"(?:time|timestamp|started_at|finished_at)"\s*:\s*)"[^"]*"'
)
_PID_VALUE = re.compile(r'(?P<prefix>"(?:pid|shell_pid)"\s*:\s*)-?\d+')
_DURATION_VALUE = re.compile(
    r'(?P<prefix>"(?:duration|duration_seconds)"\s*:\s*)-?(?:\d+(?:\.\d*)?|\.\d+)'
)
#: A line the documented launcher writes itself (``[herdr-run] engine=...``, the Rust cache verdict),
#: as opposed to the tool's own ``herdr-run: ...`` diagnostics.
_LAUNCHER_LINE = re.compile(r"^\[herdr-run\] [^\n]*\n", re.MULTILINE)

#: Agent variables the editions read. Removed from the inherited environment so a developer's own
#: session name cannot leak into a case that is about something else.
_AGENT_VARIABLES = ("HERDR_RUN_AGENT", "DG_AGENT_NAME", "ORC_AGENT_NAME")

#: Interpreter settings a developer's shell may carry that change how the Python edition behaves,
#: not what it does: ``PYTHONUNBUFFERED`` once hid a failed-write divergence that only showed with
#: it unset. Removed so every comparison runs the interpreter as an ordinary caller starts it; a
#: case that is about one of them sets it explicitly.
_INTERPRETER_VARIABLES = (
    "PYTHONUNBUFFERED",
    "PYTHONIOENCODING",
    "PYTHONUTF8",
    "PYTHONWARNINGS",
    "PYTHONDEVMODE",
    "PYTHONFAULTHANDLER",
    "PYTHONINSPECT",
    "PYTHONVERBOSE",
    "PYTHONDEBUG",
    "PYTHONOPTIMIZE",
    "PYTHONMALLOC",
    "PYTHONHASHSEED",
    "PYTHONSAFEPATH",
    "PYTHONSTARTUP",
    "PYTHONBREAKPOINT",
)


#: Why a herdr-run check can be skipped instead of executed. ``cross/differential.py`` registers
#: the same kinds, so a lane can allow one with ``--allow-skip``; without that a skip fails the run.
SKIP_KINDS: dict[str, str] = {
    "c-compiler": (
        "needs a C compiler to build the account-home shim that every herdr-run check reaching a "
        "Herdr preloads: the end-to-end group, status, reap, and the unsafe-path cases"
    ),
    "system-herdr": (
        "needs no Herdr installed in /usr/local/bin or /usr/bin, which herdr-run would find before "
        "the fake one in the account home"
    ),
}


@dataclass(frozen=True)
class Outcome:
    """One observable CLI result; both streams decoded losslessly."""

    returncode: int
    stdout: str
    stderr: str


@dataclass
class Report:
    """Accumulated paired checks and actionable divergence descriptions."""

    checks: int = 0
    failures: list[str] = field(default_factory=list)
    notes: list[str] = field(default_factory=list)
    #: Checks that did NOT run, as ``(label, kind, reason)``. Never a pass; see ``SKIP_KINDS``.
    skipped: list[tuple[str, str, str]] = field(default_factory=list)

    def skip(self, label: str, kind: str, reason: str) -> None:
        """Record a check that did not run. It is not counted in ``checks``."""
        if kind not in SKIP_KINDS:
            raise ValueError(f"unknown skip kind {kind!r}; expected one of {sorted(SKIP_KINDS)}")
        self.skipped.append((label, kind, reason))

    def require(self, label: str, condition: bool, detail: str) -> None:
        self.checks += 1
        if not condition:
            self.failures.append(f"{label}: {detail}")

    def exact(
        self, label: str, python: Outcome, rust: Outcome, *, expected_rc: int
    ) -> None:
        self.checks += 1
        if (
            python.returncode != expected_rc
            or rust.returncode != expected_rc
            or python != rust
        ):
            self.failures.append(
                f"{label}: expected rc {expected_rc}; "
                f"python={_describe(python)} rust={_describe(rust)}"
            )

    def same(
        self, label: str, python: Outcome, rust: Outcome, *, allowed: frozenset[int]
    ) -> None:
        """Byte-identical outcomes whose exit status is one of ``allowed``, and no crash text."""
        self.checks += 1
        crashed = _crashed(python) or _crashed(rust)
        if python != rust or python.returncode not in allowed or crashed:
            self.failures.append(
                f"{label}: expected identical outcomes with rc in {sorted(allowed)} and no "
                f"traceback/panic; python={_describe(python)} rust={_describe(rust)}"
            )


@dataclass(frozen=True)
class PairCase:
    """Equivalent isolated projects, one for each implementation."""

    python_root: Path
    rust_root: Path


Setup = Callable[[Path], None]


class Harness:
    """Create paired fixtures and invoke both supplied argv vectors hermetically."""

    def __init__(
        self,
        root: Path,
        python_command: Sequence[str],
        rust_command: Sequence[str],
    ) -> None:
        self.root = root
        self.python_command = _prepare_command(python_command)
        self.rust_command = _prepare_command(rust_command)
        self._serial = 0
        #: The account-home shim, once :func:`_prepare_account_shim` has built it and proved it
        #: takes effect; ``None`` before that, or when it cannot be.
        self.shim: Path | None = None
        #: Why there is no shim, as ``(skip kind, reason)``, when that is a reason to skip.
        self.shim_skip: tuple[str, str] | None = None

    def account_unavailable(self, report: Report, label: str) -> bool:
        """True, with ``label`` recorded as skipped when that is why, if no shim can be preloaded.

        Every invocation that may reach a Herdr runs against the fake one behind the shim, never
        against whatever Herdr the host has installed; without the shim it does not run at all.
        """
        if self.shim is not None:
            return False
        if self.shim_skip is not None:
            report.skip(label, *self.shim_skip)
        return True

    def _account_environment(self, root: Path) -> dict[str, str | None]:
        """Install the fake Herdr in ``root``'s own account home; preload the shim pointing there.

        The account home sits inside the project root, so every path through it is normalized to
        ``<ROOT>`` like the project's own.
        """
        assert self.shim is not None
        account = root / _ACCOUNT_DIRECTORY
        if not account.exists():
            _install_fake_herdr(account)
        return {
            _E2E_HOME_VARIABLE: str(account),
            _E2E_REFUSALS_VARIABLE: str(root / _ACCOUNT_REFUSALS),
            "LD_PRELOAD": str(self.shim),
        }

    def account_calls(self, report: Report, label: str, case: PairCase) -> None:
        """Compare what each edition asked the fake Herdr in ``case``; require that it only read.

        Also requires that the shim refused nothing, and ends anything the fake started.
        """
        observed: list[tuple[str, str, list[str], str]] = []
        for root in (case.python_root, case.rust_root):
            account = root / _ACCOUNT_DIRECTORY
            calls = _fake_calls(account)
            _end_fake_processes(account)
            argv = "".join(_normalize_text(json.dumps(call["argv"]), root) + "\n" for call in calls)
            handed = "".join(
                _normalize_text(_handed_line(index, call), root) for index, call in enumerate(calls)
            )
            written = [
                " ".join(cast(list[str], call["argv"])[:2])
                for call in calls
                if not _reads_only(cast(list[str], call["argv"]))
            ]
            refusals = root / _ACCOUNT_REFUSALS
            refused = refusals.read_text(encoding="utf-8") if refusals.exists() else ""
            observed.append((argv, handed, written, refused))
        python, rust = observed
        report.exact(
            f"{label}/herdr-calls",
            Outcome(0, python[0], ""),
            Outcome(0, rust[0], ""),
            expected_rc=0,
        )
        report.require(
            f"{label}/handed-to-herdr", python[1] == rust[1], _first_difference(python[1], rust[1])
        )
        report.require(
            f"{label}/herdr-only-read",
            not python[2] and not rust[2],
            f"a call that changes Herdr state was made: python={python[2]!r} rust={rust[2]!r}",
        )
        report.require(
            f"{label}/nothing-refused",
            python[3] == "" and rust[3] == "",
            "an edition tried to start systemd-run or a Herdr outside the fake account home: "
            f"python={python[3]!r} rust={rust[3]!r}",
        )

    def case(
        self,
        label: str,
        files: Mapping[str, str | bytes] | None = None,
        *,
        setup: Setup | None = None,
    ) -> PairCase:
        self._serial += 1
        safe_label = re.sub(r"[^A-Za-z0-9_.-]+", "-", label).strip("-") or "case"
        case_root = self.root / f"{self._serial:03d}-{safe_label}"
        # Keep the project basename identical: ``{project}`` is intentionally observable in tab
        # labels, so naming the two roots "python" and "rust" would manufacture a divergence.
        python_root = case_root / "python" / "project"
        rust_root = case_root / "rust" / "project"
        for project in (python_root, rust_root):
            project.mkdir(parents=True)
            (project / ".home").mkdir()
            if files is not None:
                for relative, content in files.items():
                    path = project / relative
                    path.parent.mkdir(parents=True, exist_ok=True)
                    if isinstance(content, bytes):
                        path.write_bytes(content)
                    else:
                        path.write_text(content, encoding="utf-8")
            if setup is not None:
                setup(project)
        return PairCase(python_root, rust_root)

    def invoke(
        self,
        case: PairCase,
        args: Sequence[str],
        *,
        cwd: str = ".",
        extra_env: Mapping[str, str | None] | None = None,
        mode: str = "capture",
        account: bool = False,
    ) -> tuple[Outcome, Outcome]:
        """Run ``args`` under both editions; ``account`` puts the fake Herdr behind the shim."""
        return self.invoke_commands(
            case,
            self.python_command,
            self.rust_command,
            args,
            cwd=cwd,
            extra_env=extra_env,
            mode=mode,
            account=account,
        )

    def environment_for(
        self, root: Path, extra_env: Mapping[str, str | None] | None = None
    ) -> dict[str, str]:
        """The environment :meth:`invoke` gives the edition whose project is ``root``."""
        return _environment(
            root / ".home",
            {
                key: None if value is None else _expand(value, root)
                for key, value in (extra_env or {}).items()
            },
        )

    def invoke_commands(
        self,
        case: PairCase,
        python_command: Sequence[str],
        rust_command: Sequence[str],
        args: Sequence[str],
        *,
        cwd: str = ".",
        extra_env: Mapping[str, str | None] | None = None,
        rust_env: Mapping[str, str | None] | None = None,
        mode: str = "capture",
        account: bool = False,
    ) -> tuple[Outcome, Outcome]:
        outcomes: list[Outcome] = []
        for command, root, own_env in (
            (python_command, case.python_root, None),
            (rust_command, case.rust_root, rust_env),
        ):
            working = root / cwd
            if mode != "deleted-cwd":
                working.mkdir(parents=True, exist_ok=True)
            environment: dict[str, str | None] = dict(extra_env or {})
            environment.update(own_env or {})
            if account:
                environment.update(self._account_environment(root))
            outcome = _run(
                command,
                [_expand(argument, root) for argument in args],
                cwd=working,
                home=root / ".home",
                extra_env={
                    key: None if value is None else _expand(value, root)
                    for key, value in environment.items()
                },
                mode=mode,
            )
            outcomes.append(_normalize_outcome(outcome, root))
        return outcomes[0], outcomes[1]


def _expand(value: str, root: Path) -> str:
    return value.replace(ROOT_TOKEN, str(root))


def _prepare_command(command: Sequence[str]) -> tuple[str, ...]:
    if not command:
        raise ValueError("implementation command argv must not be empty")
    prepared = list(command)
    executable = prepared[0]
    if os.sep in executable and not os.path.isabs(executable):
        prepared[0] = str(Path(executable).resolve())
    return tuple(prepared)


def _environment(home: Path, extra: Mapping[str, str | None] | None) -> dict[str, str]:
    environment = dict(os.environ)
    for key in (*_AGENT_VARIABLES, *_INTERPRETER_VARIABLES):
        environment.pop(key, None)
    original_home = environment.get("HOME")
    existing_pythonpath = environment.get("PYTHONPATH", "")
    local_pythonpath = str(REPO_ROOT / "py")
    environment["PYTHONPATH"] = (
        local_pythonpath
        if not existing_pythonpath
        else local_pythonpath + os.pathsep + existing_pythonpath
    )
    environment.update(
        {
            "HOME": str(home),
            "LANG": "C.UTF-8",
            "LC_ALL": "C.UTF-8",
            "NO_COLOR": "1",
            "TZ": "UTC",
        }
    )
    environment.pop("DAGRUN_ENGINE", None)
    # A development interpreter may have declared dependencies in its normal user site.  Changing
    # HOME for deterministic tilde expansion must not make that already-installed site disappear,
    # and the Rust launcher's toolchain must stay where the developer installed it.
    if original_home:
        environment.setdefault("PYTHONUSERBASE", str(Path(original_home) / ".local"))
        environment.setdefault("CARGO_HOME", str(Path(original_home) / ".cargo"))
        environment.setdefault("RUSTUP_HOME", str(Path(original_home) / ".rustup"))
    for key, value in (extra or {}).items():
        if value is None:
            environment.pop(key, None)
        else:
            environment[key] = value
    return environment


def _decode_output(value: bytes | str | None) -> str:
    if value is None:
        return ""
    if isinstance(value, bytes):
        return value.decode("utf-8", errors="surrogateescape")
    return value


def _crashed(outcome: Outcome) -> bool:
    lowered = outcome.stderr.lower()
    return "traceback" in lowered or "panicked at" in lowered


def _run(
    command: Sequence[str],
    args: Sequence[str],
    *,
    cwd: Path,
    home: Path,
    extra_env: Mapping[str, str | None] | None,
    mode: str = "capture",
) -> Outcome:
    environment = _environment(home, extra_env)
    argv = [*command, *args]
    if mode == "capture":
        return _run_captured(argv, cwd, environment)
    if mode == "inherited-descriptors":
        # Standard input closed, and descriptor 7 open on a file: what a caller hands over that the
        # editions' children must not see (7) or must see as /dev/null (0).
        inherited = os.open(os.devnull, os.O_RDONLY)
        try:

            def hand_over() -> None:
                os.dup2(inherited, 7)
                os.close(0)

            return _run_captured(argv, cwd, environment, preexec=hand_over, close_fds=False)
        finally:
            os.close(inherited)
    if mode == "sigint-ignored":
        return _run_captured(argv, cwd, environment, preexec=_ignore_sigint)
    if mode == "sigint-ignored-while-writing":
        return _run_interrupted(argv, cwd, environment, ignore_sigint=True)
    if mode == "file-size-limit":
        return _run_size_limited(argv, cwd, environment)
    if mode == "closed-stdout":
        return _run_captured(argv, cwd, environment, preexec=lambda: os.close(1))
    if mode == "deleted-cwd":
        doomed = str(cwd)

        def enter_and_delete() -> None:
            os.mkdir(doomed)
            os.chdir(doomed)
            os.rmdir(doomed)

        return _run_captured(argv, cwd.parent, environment, preexec=enter_and_delete)
    if mode == "dev-full":
        with open("/dev/full", "wb") as full:
            return _run_redirected(argv, cwd, environment, full.fileno())
    if mode == "broken-pipe":
        reader, writer = os.pipe()
        os.close(reader)
        try:
            return _run_redirected(argv, cwd, environment, writer)
        finally:
            os.close(writer)
    if mode == "sigint-while-writing":
        return _run_interrupted(argv, cwd, environment)
    if mode == "broken-stderr":
        return _run_broken_stderr(argv, cwd, environment)
    if mode == "sigchld-ignored":
        return _run_captured(argv, cwd, environment, preexec=_ignore_sigchld)
    if mode == "raw-environment":
        return _run_with_raw_environment(argv, cwd, _raw_entries(environment))
    if mode == "signals-ignored":
        return _run_captured(argv, cwd, environment, preexec=_ignoring_raw(_HANDED_IGNORED))
    raise ValueError(f"unknown invocation mode {mode}")


#: What ``raw-environment`` appends to a case's environment: a name given twice, an entry with no
#: ``=``, an empty name, an entry the C library reads as a variable named ``=HERDR_E2E_LEADING``, a
#: value holding ``=``, and second ``HOME`` and ``PATH`` entries. ``subprocess`` cannot express any
#: of that; see :func:`_run_with_raw_environment`.
_RAW_EXTRAS: tuple[bytes, ...] = (
    b"HERDR_E2E_DUPLICATE=first",
    b"HERDR_E2E_NO_EQUALS",
    b"=empty-name",
    b"=HERDR_E2E_LEADING=x",
    b"HERDR_E2E_VALUE=a=b",
    b"HERDR_E2E_DUPLICATE=second",
    b"HOME=/second/home",
    b"PATH=/second/path",
)


def _raw_entries(environment: Mapping[str, str]) -> list[bytes]:
    """A case's environment as entries, in order, with :data:`_RAW_EXTRAS` after them."""
    entries = [os.fsencode(name) + b"=" + os.fsencode(value) for name, value in environment.items()]
    return [*entries, *_RAW_EXTRAS]


def _by_the_rule(
    entries: Sequence[bytes], changes: Sequence[tuple[bytes, bytes | None]] = ()
) -> list[bytes]:
    """The environment rule both editions share, written out again so the harness checks it.

    Split at the first ``=``; drop an entry with no ``=`` or an empty name; keep the first of two
    entries with one name where it stood; then apply each change: a value replaces in place or is
    added at the end, ``None`` removes.
    """
    kept: dict[bytes, bytes] = {}
    for entry in entries:
        name, separator, value = entry.partition(b"=")
        if separator and name and name not in kept:
            kept[name] = value
    for name, replacement in changes:
        if replacement is None:
            kept.pop(name, None)
        else:
            kept[name] = replacement
    return [name + b"=" + value for name, value in kept.items()]


def _run_with_raw_environment(argv: Sequence[str], cwd: Path, entries: Sequence[bytes]) -> Outcome:
    """Run ``argv`` with exactly ``entries`` as its environment, in order, oddities included.

    ``subprocess`` and ``os.execve`` take the environment as a mapping, which cannot hold a name
    twice, an entry with no ``=``, or an empty name. The C library's ``execve`` takes the array
    itself, so it is called directly, in the child ``subprocess`` forked, once that child has its
    working directory, descriptors, and session. If it fails the child exits 126 rather than going
    on to ``subprocess``'s own ``exec`` with a different environment.
    """
    import ctypes

    libc = ctypes.CDLL(None, use_errno=True)
    executable = argv[0] if os.sep in argv[0] else shutil.which(argv[0]) or argv[0]
    path = os.fsencode(executable)
    arguments = (ctypes.c_char_p * (len(argv) + 1))(*(os.fsencode(item) for item in argv), None)
    variables = (ctypes.c_char_p * (len(entries) + 1))(*entries, None)

    def replace() -> None:
        libc.execve(path, arguments, variables)
        os._exit(126)

    return _run_captured(argv, cwd, {}, preexec=replace)


#: ``rt_sigaction`` on the two architectures whose kernel ``struct sigaction`` is four eight-byte
#: words (handler, flags, restorer, mask), which is all :func:`_ignoring_raw` declares.
_RT_SIGACTION = {"x86_64": 13, "aarch64": 134}

#: What ``signals-ignored`` hands an edition ignored: SIGHUP, SIGUSR1, and signal 33, which the C
#: library takes for itself once a process starts a thread, and SIGPIPE, which every child gets
#: back at its default action.
_HANDED_IGNORED = (signal.SIGHUP, signal.SIGUSR1, 33, signal.SIGPIPE)


def _ignoring_raw(signals: Sequence[int]) -> Callable[[], None]:
    """A ``preexec_fn`` that ignores ``signals``, through the system call.

    The C library refuses to touch 32 and 33, which it keeps for itself, but a caller is free to
    leave them ignored, and ``exec`` keeps them so. Where the layout is not declared here this
    ignores what the C library allows and leaves the rest.
    """
    import ctypes

    number = _RT_SIGACTION.get(os.uname().machine)
    libc = ctypes.CDLL(None, use_errno=True)
    ignore = (ctypes.c_uint64 * 4)(1, 0, 0, 0)  # SIG_IGN, no flags, no restorer, empty mask

    def hand_over() -> None:
        for signal_number in signals:
            if number is not None:
                libc.syscall(number, signal_number, ignore, None, 8)
            else:
                try:
                    signal.signal(signal_number, signal.SIG_IGN)
                except (OSError, ValueError):
                    pass

    return hand_over


def _ignore_sigchld() -> None:
    """Start a process with SIGCHLD ignored, which a caller can hand down across ``exec``."""
    signal.signal(signal.SIGCHLD, signal.SIG_IGN)


def _run_broken_stderr(argv: Sequence[str], cwd: Path, environment: Mapping[str, str]) -> Outcome:
    """Run with stderr on a pipe nobody reads; only the exit status and stdout are observed."""
    reader, writer = os.pipe()
    os.close(reader)
    try:
        completed = subprocess.run(
            list(argv),
            cwd=cwd,
            env=dict(environment),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=writer,
            check=False,
            start_new_session=True,
            timeout=TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired as error:
        return Outcome(124, _decode_output(error.stdout), "TIMEOUT\n")
    except OSError as error:
        return Outcome(127, "", f"could not invoke implementation: {error}\n")
    finally:
        os.close(writer)
    return Outcome(completed.returncode, _decode_output(completed.stdout), "")


def _ignore_sigint() -> None:
    """Start a process with SIGINT ignored, as a caller such as ``nohup`` may start it."""
    signal.signal(signal.SIGINT, signal.SIG_IGN)


def _run_captured(
    argv: Sequence[str],
    cwd: Path,
    environment: Mapping[str, str],
    *,
    preexec: Callable[[], None] | None = None,
    close_fds: bool = True,
) -> Outcome:
    try:
        completed = subprocess.run(
            list(argv),
            cwd=cwd,
            env=dict(environment),
            stdin=subprocess.DEVNULL,
            capture_output=True,
            check=False,
            start_new_session=True,
            timeout=TIMEOUT_SECONDS,
            preexec_fn=preexec,
            close_fds=close_fds,
        )
    except subprocess.TimeoutExpired as error:
        return Outcome(
            124,
            _decode_output(error.stdout),
            _decode_output(error.stderr)
            + f"\nTIMEOUT after {TIMEOUT_SECONDS} seconds\n",
        )
    except (OSError, subprocess.SubprocessError) as error:
        return Outcome(127, "", f"could not invoke implementation: {error}\n")
    return Outcome(
        completed.returncode,
        _decode_output(completed.stdout),
        _decode_output(completed.stderr),
    )


def _run_redirected(
    argv: Sequence[str], cwd: Path, environment: Mapping[str, str], stdout: int
) -> Outcome:
    """Run with stdout on a chosen descriptor; only stderr is observed."""
    try:
        completed = subprocess.run(
            list(argv),
            cwd=cwd,
            env=dict(environment),
            stdin=subprocess.DEVNULL,
            stdout=stdout,
            stderr=subprocess.PIPE,
            check=False,
            start_new_session=True,
            timeout=TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired as error:
        return Outcome(124, "", _decode_output(error.stderr) + "\nTIMEOUT\n")
    except OSError as error:
        return Outcome(127, "", f"could not invoke implementation: {error}\n")
    return Outcome(completed.returncode, "", _decode_output(completed.stderr))


#: Linux's ``F_SETPIPE_SZ``; named here because older Pythons do not export it.
_F_SETPIPE_SZ = 1031


def _run_interrupted(
    argv: Sequence[str],
    cwd: Path,
    environment: Mapping[str, str],
    *,
    ignore_sigint: bool = False,
) -> Outcome:
    """Interrupt the process while it is blocked writing a long document to a full pipe.

    The pipe is shrunk to one page and never read until the process has filled it, so the process
    is provably parked inside its stdout write -- long after start-up -- when SIGINT arrives. The
    observable result is the exit status and stderr; the bytes already written are discarded.

    With ``ignore_sigint`` the process starts with SIGINT ignored, as a caller may start it; the
    interrupt must then change nothing, so the pipe is drained afterwards and the write completes.
    """
    reader, writer = os.pipe()
    try:
        try:
            fcntl.fcntl(writer, _F_SETPIPE_SZ, 4096)
        except OSError:
            pass
        process = subprocess.Popen(
            list(argv),
            cwd=cwd,
            env=dict(environment),
            stdin=subprocess.DEVNULL,
            stdout=writer,
            stderr=subprocess.PIPE,
            start_new_session=True,
            preexec_fn=_ignore_sigint if ignore_sigint else None,
        )
        os.close(writer)
        writer = -1
        deadline = time.monotonic() + TIMEOUT_SECONDS
        capacity = fcntl.fcntl(reader, 1032)  # F_GETPIPE_SZ
        while time.monotonic() < deadline and process.poll() is None:
            buffered = bytearray(4)
            fcntl.ioctl(reader, termios.FIONREAD, buffered)
            if int.from_bytes(buffered, sys.byteorder) >= capacity:
                break
            time.sleep(0.01)
        if process.poll() is None:
            # Give the writer a moment to be parked in write(2) on the now-full pipe.
            time.sleep(0.1)
            process.send_signal(signal.SIGINT)
        if ignore_sigint:
            while os.read(reader, 65536):
                pass
        try:
            _, stderr = process.communicate(timeout=TIMEOUT_SECONDS)
        except subprocess.TimeoutExpired:
            process.kill()
            _, stderr = process.communicate()
            return Outcome(124, "", _decode_output(stderr) + "\nTIMEOUT\n")
        return Outcome(process.returncode, "", _decode_output(stderr))
    finally:
        os.close(reader)
        if writer >= 0:
            os.close(writer)


def _run_size_limited(argv: Sequence[str], cwd: Path, environment: Mapping[str, str]) -> Outcome:
    """Run with stdout on a regular file and RLIMIT_FSIZE at one page.

    A write past the limit raises SIGXFSZ, whose default action kills the writer. Both editions
    ignore it, so the write fails with EFBIG and is reported; only the exit status and stderr are
    observed.
    """
    import resource

    with tempfile.TemporaryFile() as sink:

        def limit() -> None:
            resource.setrlimit(resource.RLIMIT_FSIZE, (4096, 4096))

        try:
            completed = subprocess.run(
                list(argv),
                cwd=cwd,
                env=dict(environment),
                stdin=subprocess.DEVNULL,
                stdout=sink,
                stderr=subprocess.PIPE,
                check=False,
                start_new_session=True,
                timeout=TIMEOUT_SECONDS,
                preexec_fn=limit,
            )
        except subprocess.TimeoutExpired as error:
            return Outcome(124, "", _decode_output(error.stderr) + "\nTIMEOUT\n")
    return Outcome(completed.returncode, "", _decode_output(completed.stderr))


def _normalize_text(text: str, root: Path) -> str:
    normalized = text.replace(str(root), ROOT_TOKEN)
    normalized = _RUN_ID_VALUE.sub(r'\g<prefix>"<RUN_ID>"', normalized)
    normalized = _RUN_ID_PATH.sub("<RUN_ID>", normalized)
    normalized = _TIME_VALUE.sub(r'\g<prefix>"<TIME>"', normalized)
    normalized = _PID_VALUE.sub(r"\g<prefix><PID>", normalized)
    normalized = _DURATION_VALUE.sub(r"\g<prefix><DURATION>", normalized)
    return normalized


def _normalize_outcome(outcome: Outcome, root: Path) -> Outcome:
    return Outcome(
        outcome.returncode,
        _normalize_text(outcome.stdout, root),
        _normalize_text(outcome.stderr, root),
    )


def _describe(outcome: Outcome) -> str:
    return (
        f"Outcome(rc={outcome.returncode}, stdout={outcome.stdout!r}, "
        f"stderr={outcome.stderr!r})"
    )


#: Every subcommand the surface promises, at both levels.
#:
#: Named literally rather than scraped from either edition, so an edition that quietly loses a
#: subcommand fails here instead of teaching the harness its own smaller idea of the surface.
_SUBCOMMANDS: tuple[str, ...] = (
    "check",
    "config",
    "init",
    "net-doctor",
    "quickstart",
    "reap",
    "run",
    "status",
    "target",
    "userguide",
)

#: Long options accepted BEFORE the subcommand, and nowhere else.
_GLOBAL_OPTIONS = frozenset(
    {"--help", "--version", "--config", "--agent", "--json", "--dump-surface"}
)

#: Long options accepted AFTER each subcommand, and nowhere else.
_LOCAL_OPTIONS: dict[str, frozenset[str]] = {
    "run": frozenset(
        {"--help", "--cwd", "--timeout", "--wait-ready", "--no-cache", "--dry-run"}
    ),
    "check": frozenset({"--help"}),
    "config": frozenset({"--help"}),
    "init": frozenset({"--help", "--force", "--upgrade"}),
    "status": frozenset({"--help"}),
    "target": frozenset({"--help", "--no-cache"}),
    "reap": frozenset({"--help"}),
    "net-doctor": frozenset({"--help"}),
    "quickstart": frozenset({"--help"}),
    "userguide": frozenset({"--help"}),
}

#: The metavariable each value-taking option names; every other option is a flag.
_OPTION_VALUES: dict[str, str] = {
    "--agent": "NAME",
    "--config": "PATH",
    "--cwd": "PATH",
    "--timeout": "SECONDS",
    "--wait-ready": "SECONDS",
}

#: Subcommands that take the one quoted command line as their positional argument.
_POSITIONAL: dict[str, tuple[str, ...]] = {"check": ("<command>",), "run": ("<command>",)}


def _expected_surface() -> dict[str, object]:
    """The ``--dump-surface`` document both editions must print, written out from the tables."""

    def options(names: frozenset[str]) -> dict[str, object]:
        return {name: _OPTION_VALUES.get(name) for name in sorted(names | {"-h"})}

    return {
        "global_options": options(_GLOBAL_OPTIONS),
        "subcommands": {
            name: {
                "options": options(_LOCAL_OPTIONS[name]),
                "positional": list(_POSITIONAL.get(name, ())),
            }
            for name in sorted(_SUBCOMMANDS)
        },
    }


#: Every configuration key the two editions accept, and therefore every key `init` must write.
_CONFIG_KEYS: tuple[str, ...] = (
    "allow",
    "allow_subcommand",
    "broker",
    "cwd",
    "deny_anywhere",
    "deny_global",
    "deny_subcommand",
    "deny_subcommand_option",
    "max_panes",
    "prefixes",
    "probe_remote",
    "prompt_tail",
    "readiness",
    "ready_timeout_seconds",
    "retention_days",
    "shells",
    "spool_dir",
    "tab_name",
    "timeout_seconds",
    "value_options",
    "workspace",
)

_OPTION_TOKEN = re.compile(r"--[a-z][a-z0-9-]*")


def _block(text: str, header_prefix: str) -> list[str]:
    """Return the contiguous indented lines that follow the first line starting with a header."""
    lines = text.splitlines()
    for index, line in enumerate(lines):
        if line.startswith(header_prefix):
            body: list[str] = []
            for candidate in lines[index + 1 :]:
                if not candidate.strip():
                    break
                body.append(candidate)
            return body
    return []


def _declared_subcommands(text: str) -> tuple[str, ...]:
    return tuple(
        sorted(line.split()[0] for line in _block(text, "subcommands:") if line.split())
    )


def _declared_options(text: str, header_prefix: str) -> frozenset[str]:
    return frozenset(
        token
        for line in _block(text, header_prefix)
        for token in _OPTION_TOKEN.findall(line)
    )


def _help_surface(harness: Harness, report: Report, case: PairCase) -> None:
    """Compare every help screen byte for byte, and the whole two-level schema.

    ``report.exact`` on every help text already makes one edition growing a subcommand or an option
    the other lacks a divergence.  The literal expectations below add the case ``exact`` cannot
    catch: both editions dropping the same thing at once, which is agreement about a surface that
    no longer matches what the tool promises.
    """

    for flag in ("--help", "-h"):
        python, rust = harness.invoke(case, (flag,))
        report.exact(f"help/top-level{flag}", python, rust, expected_rc=0)
    python, rust = harness.invoke(case, ("--help",))
    report.require(
        "help/top-level-subcommands",
        _declared_subcommands(python.stdout) == tuple(sorted(_SUBCOMMANDS)),
        f"top-level help does not list exactly {sorted(_SUBCOMMANDS)}: {_describe(python)}",
    )
    report.require(
        "help/top-level-global-options",
        _declared_options(python.stdout, "global options") == _GLOBAL_OPTIONS,
        f"global option list changed: {_describe(python)}",
    )
    # The two levels must not document each other. This is the defect the surface exists to fix.
    top_level_local = _declared_options(python.stdout, "global options") & {
        option for options in _LOCAL_OPTIONS.values() for option in options
    } - {"--help"}
    report.require(
        "help/top-level-omits-local-options",
        not top_level_local,
        f"top-level help documents subcommand options {sorted(top_level_local)}",
    )

    for subcommand in _SUBCOMMANDS:
        for flag in ("--help", "-h"):
            python, rust = harness.invoke(case, (subcommand, flag))
            report.exact(f"help/{subcommand}{flag}", python, rust, expected_rc=0)
        python, rust = harness.invoke(case, (subcommand, "--help"))
        declared = _declared_options(python.stdout, "options:")
        report.require(
            f"help/{subcommand}-options",
            declared == _LOCAL_OPTIONS[subcommand],
            f"'{subcommand} --help' declares {sorted(declared)}, "
            f"expected {sorted(_LOCAL_OPTIONS[subcommand])}: {_describe(python)}",
        )
        leaked = declared & (_GLOBAL_OPTIONS - {"--help"})
        report.require(
            f"help/{subcommand}-omits-global-options",
            not leaked,
            f"'{subcommand} --help' documents global options {sorted(leaked)}",
        )
        # Help that is reached with globals in front is the same help.
        python, rust = harness.invoke(case, ("--json", subcommand, "--help"))
        report.exact(f"help/{subcommand}-after-a-global", python, rust, expected_rc=0)


def _surface_dump(harness: Harness, report: Report, case: PairCase) -> None:
    """``--dump-surface`` is the structured surface: identical bytes, and exactly the promise."""

    python, rust = harness.invoke(case, ("--dump-surface",))
    report.exact("surface/dump", python, rust, expected_rc=0)
    try:
        document = cast(dict[str, object], json.loads(python.stdout))
    except json.JSONDecodeError as error:
        report.require("surface/dump-is-json", False, f"{error}: {_describe(python)}")
        return
    version = document.pop("version", None)
    expected = _expected_surface()
    report.require(
        "surface/dump-matches-the-promised-surface",
        document == expected,
        f"--dump-surface printed {json.dumps(document, sort_keys=True)}, expected "
        f"{json.dumps(expected, sort_keys=True)}",
    )
    version_python, _ = harness.invoke(case, ("--version",))
    report.require(
        "surface/dump-version-is-the-version",
        version_python.stdout == f"herdr-run {version}\n",
        f"--dump-surface says version {version!r}; --version says {version_python.stdout!r}",
    )
    for label, args in (
        ("dump-surface-after-subcommand", ("check", "--dump-surface")),
        ("dump-surface-with-value", ("--dump-surface=x",)),
    ):
        python, rust = harness.invoke(case, args)
        report.exact(f"surface/{label}", python, rust, expected_rc=2)


def _levels(harness: Harness, report: Report, case: PairCase) -> None:
    """An option offered at the wrong level must be refused, identically, by both editions."""

    misplaced: tuple[tuple[str, tuple[str, ...]], ...] = (
        ("global-cwd", ("--cwd", "/tmp", "run", "git status")),
        ("global-timeout", ("--timeout", "5", "run", "git status")),
        ("global-wait-ready", ("--wait-ready", "5", "run", "git status")),
        ("global-no-cache", ("--no-cache", "target")),
        ("global-dry-run", ("--dry-run", "run", "git status")),
        ("local-agent", ("run", "--agent", "fixture-agent", "git status")),
        ("local-config", ("run", "--config", "x.yaml", "git status")),
        ("local-json", ("run", "--json", "git status")),
        ("foreign-dry-run", ("check", "--dry-run", "git status")),
        ("foreign-cwd", ("reap", "--cwd", "/tmp")),
        # Right level, wrong subcommand: `--config` is global, but these three read no
        # configuration file, so accepting it would be accepting an instruction and then
        # disregarding it. `--config P init` is the acute one: it reads as "write P".
        ("config-blind-init", ("--config", "x.yaml", "init")),
        ("config-blind-init-inline", ("--config=x.yaml", "init")),
        ("config-blind-init-force", ("--config", "x.yaml", "init", "--force")),
        ("config-blind-quickstart", ("--config", "x.yaml", "quickstart")),
        ("config-blind-userguide", ("--config", "x.yaml", "userguide")),
        # `--agent NAME` says who this invocation speaks for, and the one thing that does is name
        # a tab. These five resolve no tab, so accepting it would be accepting an instruction and
        # then doing nothing about it.
        ("agent-blind-check", ("--agent", "a", "check", "git status")),
        ("agent-blind-check-inline", ("--agent=a", "check", "git status")),
        ("agent-blind-init", ("--agent", "a", "init")),
        ("agent-blind-reap", ("--agent", "a", "reap")),
        ("agent-blind-quickstart", ("--agent", "a", "quickstart")),
        ("agent-blind-userguide", ("--agent", "a", "userguide")),
        # `--json` promises machine-readable output where a subcommand has it; these three have
        # only prose to give, so a caller who was humoured would fail at the parse of the output
        # rather than at the flag.
        ("json-blind-net-doctor", ("--json", "net-doctor")),
        ("json-blind-quickstart", ("--json", "quickstart")),
        ("json-blind-userguide", ("--json", "userguide")),
        # Unknown subcommands, unknown options, missing values, and repeated arguments.
        ("unknown-subcommand", ("frobnicate",)),
        ("unknown-global", ("--frobnicate", "config")),
        ("unknown-local", ("config", "--frobnicate")),
        ("missing-config-value", ("--config",)),
        ("missing-agent-value", ("config", "--agent")),
        ("missing-timeout-value", ("run", "--dry-run", "git status", "--timeout")),
        ("missing-command", ("run", "--dry-run")),
        ("extra-positional", ("check", "git status", "extra")),
        ("config-positional", ("config", "extra")),
        ("empty-subcommand", ("",)),
        ("double-dash-only", ("--",)),
        ("version-after-subcommand", ("config", "--version")),
    )
    for label, args in misplaced:
        python, rust = harness.invoke(case, args)
        report.exact(f"levels/{label}", python, rust, expected_rc=2)

    # Two blind globals at once must produce ONE refusal, and the same one in both editions.
    # `report.exact` already pins the agreement; naming --config pins which of the two it is, so
    # the editions cannot agree by both drifting to the other.
    python, rust = harness.invoke(case, ("--config", "x.yaml", "--agent", "a", "init"))
    report.exact("levels/two-blind-globals-give-one-refusal", python, rust, expected_rc=2)
    report.require(
        "levels/two-blind-globals-report-the-first",
        "argument --config:" in python.stderr and "argument --agent:" not in python.stderr,
        f"a --config/--agent pair did not report --config alone: {_describe(python)}",
    )

    # The globals a subcommand CAN observe stay accepted: the refusals are exactly as wide as the
    # blindness. `check` is the one that both refuses a global and accepts another.
    for label, args in (
        ("agent-sighted-run", ("--agent", "a", "run", "--dry-run", "git status")),
        ("json-sighted-check", ("--json", "check", "git status")),
        ("json-sighted-config", ("--json", "config")),
        ("repeated-agent-last-wins", ("--agent", "a", "--agent", "b", "config")),
        ("inline-agent", ("--agent=a", "config")),
        ("double-dash-command", ("check", "--", "git status")),
    ):
        python, rust = harness.invoke(case, args)
        report.exact(f"levels/{label}", python, rust, expected_rc=0)

    # The subcommand's own parse still wins, so `--help` and a bad local option are unaffected.
    python, rust = harness.invoke(case, ("--config", "x.yaml", "init", "--help"))
    report.exact("levels/config-blind-init-still-helps", python, rust, expected_rc=0)
    python, rust = harness.invoke(case, ("--config", "x.yaml", "init", "--nonsense"))
    report.exact("levels/config-blind-init-names-local-option", python, rust, expected_rc=2)
    report.require(
        "levels/config-blind-init-names-local-option-first",
        "--nonsense" in python.stderr,
        f"the local option error was replaced by the --config refusal: {_describe(python)}",
    )
    python, rust = harness.invoke(case, ("--agent", "a", "quickstart", "--nonsense"))
    report.exact("levels/agent-blind-names-local-option", python, rust, expected_rc=2)
    report.require(
        "levels/agent-blind-names-local-option-first",
        "--nonsense" in python.stderr,
        f"the local option error was replaced by the --agent refusal: {_describe(python)}",
    )


def _removed_bare_form(harness: Harness, report: Report, case: PairCase) -> None:
    """Running a command with no subcommand must fail, and must name what to type instead."""

    python, rust = harness.invoke(case, ("git status",))
    report.exact("bare-form/command-only", python, rust, expected_rc=2)
    report.require(
        "bare-form/command-only-names-run",
        "herdr-run run 'git status'" in python.stderr,
        f"the removed bare form did not name its replacement: {_describe(python)}",
    )

    python, rust = harness.invoke(case, ("release-agent", "git status"))
    report.exact("bare-form/agent-and-command", python, rust, expected_rc=2)
    report.require(
        "bare-form/agent-and-command-names-run",
        "herdr-run --agent release-agent run 'git status'" in python.stderr,
        f"the removed agent form did not name its replacement: {_describe(python)}",
    )

    python, rust = harness.invoke(case, ("run", "git", "status"))
    report.exact("bare-form/loose-words", python, rust, expected_rc=2)
    report.require(
        "bare-form/loose-words-refuse-rejoining",
        "ONE quoted argument" in python.stderr,
        f"loose command words were not refused: {_describe(python)}",
    )


def _bootstrap(harness: Harness, report: Report) -> None:
    case = harness.case("bootstrap")
    python, rust = harness.invoke(case, ("--version",))
    report.exact("bootstrap/version", python, rust, expected_rc=0)
    version_pattern = re.compile(
        r"herdr-run (?P<version>\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?)\n"
    )
    python_version = version_pattern.fullmatch(python.stdout)
    rust_version = version_pattern.fullmatch(rust.stdout)
    report.require(
        "bootstrap/version-shape",
        python_version is not None
        and rust_version is not None
        and python.stderr == "",
        f"unexpected version output: python={_describe(python)} rust={_describe(rust)}",
    )
    report.require(
        "bootstrap/version-source-parity",
        python_version is not None
        and rust_version is not None
        and python_version.group("version") == rust_version.group("version"),
        "Python package release version and Rust Cargo package version differ: "
        f"python={_describe(python)} rust={_describe(rust)}",
    )

    _help_surface(harness, report, case)
    _surface_dump(harness, report, case)
    _levels(harness, report, case)
    _removed_bare_form(harness, report, case)

    bare_python, bare_rust = harness.invoke(case, ())
    report.exact("bootstrap/bare", bare_python, bare_rust, expected_rc=0)
    for edition, outcome in (("python", bare_python), ("rust", bare_rust)):
        report.require(
            f"bootstrap/bare/{edition}",
            outcome.returncode == 0
            and outcome.stderr == ""
            and "subcommands:" in outcome.stdout,
            f"a bare invocation must print the subcommand list: {_describe(outcome)}",
        )

    python, rust = harness.invoke(
        case,
        ("--agent", "fixture-agent", "run", "--dry-run", "--", "git --help"),
    )
    report.exact("bootstrap/double-dash-command-help", python, rust, expected_rc=0)
    report.require(
        "bootstrap/double-dash-command-help-shape",
        python.stdout == "git --help\n" and python.stderr == "",
        f"command-local --help was intercepted: {_describe(python)}",
    )

    python, rust = harness.invoke(
        case,
        ("--ag", "fixture-agent", "run", "--dry", "git status"),
    )
    report.exact("bootstrap/no-option-abbreviations", python, rust, expected_rc=2)


def _policy(harness: Harness, report: Report) -> None:
    allowed = (
        ("git-status", "git status"),
        ("git-remote", "git ls-remote origin main"),
        ("prefix", "with-proxy git push origin HEAD:refs/heads/feature"),
        ("gh", "gh pr list --state open"),
        ("global-value", "git -C /tmp/repo log --oneline -5"),
        ("quoted-spaces", "git commit -m 'two words'"),
        ("empty-argument", "git commit -m ''"),
        ("mixed-quotes", "git commit -m pre\"mid\"'post'"),
        ("hash-literal", "gh issue view #12"),
        (
            "metacharacters-are-data",
            "git commit -m ';' '&&' '|' '$(id)' '`id`'",
        ),
        ("tilde", "git -C ~/work/repo status"),
        ("tilde-user", "git -C ~root/x status"),
        ("non-ascii-argument", "git commit -m 'caf\u00e9 \u202e \U0001f600'"),
        ("apostrophe-argument", "git commit -m \"it's\""),
        # An empty subcommand is still a subcommand: allowed unless a list names it.
        ("git-empty-subcommand", "git ''"),
        ("gh-empty-subcommand", "gh '' x"),
        # The scoped `-u` and `-x` stay usable where they name nothing, and the abbreviation rule
        # never reaches a different full option.
        ("push-u", "git push -u origin main"),
        ("add-u", "git add -u"),
        ("cherry-pick-x", "git cherry-pick -x abc123"),
        ("rebase-strategy-option", "git rebase -X theirs main"),
        ("clone-without-u", "git clone --depth 1 https://example.invalid/r"),
        ("longer-option-is-not-an-abbreviation", "git fetch --upload-packs x origin"),
        ("attr-source-log", "git --attr-source HEAD log -1"),
        # What `check` prints is typed text: a character a name may not contain is shown escaped,
        # in the subcommand= field and the rendered: line alike.
        ("bidi-subcommand", "git '\u202estatus'"),
        ("invisible-arguments", "git commit -m 'a\u200bb' '\u2028'"),
        ("private-use-argument", "git log '\ue000'"),
        ("unassigned-argument", "git log '\u0378'"),
        ("blank-argument", "git log '\U0001d159' '\U00016fe4'"),
        ("unicode-16-letter-argument", "git log '\u1c89'"),
    )
    for label, command in allowed:
        case = harness.case(f"policy-allow-{label}")
        python, rust = harness.invoke(case, ("check", command))
        report.exact(f"policy/allowed/{label}", python, rust, expected_rc=0)
        python, rust = harness.invoke(case, ("--json", "check", command))
        report.exact(f"policy/allowed-json/{label}", python, rust, expected_rc=0)

    refused = (
        ("empty", ""),
        ("blank", "   "),
        ("unbalanced-quote", "git status '"),
        ("trailing-escape", "git status \\"),
        ("curl", "curl https://example.invalid"),
        ("bash", "bash -lc true"),
        ("sh", "sh -c true"),
        ("python", "python3 -c pass"),
        ("rm", "rm -rf /tmp/example"),
        ("wrapper-only", "with-proxy"),
        ("wrapper-repeated", "with-proxy with-proxy git status"),
        ("absolute-program", "/bin/git status"),
        ("dot-program", "./git status"),
        ("parent-program", "../bin/gh pr list"),
        ("tilde-program", "~/bin/git status"),
        ("tilde-user-program", "~root status"),
        ("git-config", "git -c alias.pwn='!sh' pwn"),
        ("git-exec-path", "git --exec-path=/tmp log"),
        ("git-config-env", "git --config-env=x=y status"),
        ("git-namespace", "git --namespace=x status"),
        ("gh-alias", "gh alias set pwn '!sh'"),
        ("gh-extension", "gh extension exec pwn"),
        ("gh-ext", "gh ext exec pwn"),
        ("gh-codespace", "gh codespace ssh"),
        ("upload-pack", "git fetch --upload-pack=/tmp/evil origin"),
        ("receive-pack", "git push --receive-pack /tmp/evil origin"),
        ("control-bel", "git status\x07"),
        ("control-tab", "git\tstatus"),
        ("control-line-feed", "git status\nwhoami"),
        ("control-carriage-return", "git status\rwhoami"),
        ("control-escape", "git status\x1b[2J"),
        ("control-del", "git status\x7f"),
        ("control-c1", "git status\x85"),
        ("control-separator", "git status\x1c"),
        ("program-bidi-override", "curl\u202ex"),
        ("program-no-break-space", "curl\u00a0x"),
        ("program-line-separator", "curl\u2028x"),
        ("program-byte-order-mark", "curl\ufeffx"),
        ("program-private-use", "curl\ue000x"),
        ("program-combining-mark", "cur\u0301l x"),
        ("program-unassigned", "curl\u0378"),
        ("program-tag-character", "curl\U000e0001"),
        ("program-emoji", "\U0001f600 x"),
        ("program-apostrophe", "it's x"),
        ("program-quote-and-backslash", 'say"\\ x'),
        ("cargo-bare", "cargo"),
        ("cargo-fetch-default", "cargo fetch"),
        ("cargo-build", "cargo build --release"),
        ("cargo-unknown", "cargo something-new"),
        ("cargo-config", "cargo --config build.rustc-wrapper=/tmp/evil fetch"),
        # Every way to name the program git runs, including the abbreviations git accepts.
        ("upload-pack-abbreviated", "git ls-remote --upload-pac=/tmp/evil origin"),
        ("upload-pack-abbreviated-spaced", "git fetch --upload-p /tmp/evil origin"),
        ("upload-pack-shortest-abbreviation", "git fetch --u /tmp/evil origin"),
        ("receive-pack-abbreviated", "git push --receive-pac=/tmp/evil origin"),
        ("push-exec", "git push --exec=/tmp/evil origin main"),
        ("push-exec-abbreviated", "git push --exe=/tmp/evil origin main"),
        ("ls-remote-exec", "git ls-remote --exec=/tmp/evil origin"),
        ("archive-remote-exec", "git archive --remote=repo --exec=/tmp/evil HEAD"),
        ("clone-u", "git clone -u /tmp/evil https://example.invalid/r"),
        ("clone-u-attached", "git clone -u/tmp/evil https://example.invalid/r"),
        ("clone-u-clustered", "git clone -qu/tmp/evil https://example.invalid/r"),
        ("clone-u-after-globals", "git -C /tmp clone -u /tmp/evil repo"),
        ("rebase-x", "git rebase -x 'make test' main"),
        ("rebase-exec", "git rebase --exec 'make test' main"),
        ("difftool-extcmd-abbreviated", "git difftool --extc=/tmp/evil HEAD"),
        # A global option that takes the next argument must not hide the subcommand behind it.
        ("attr-source-daemon", "git --attr-source HEAD daemon --export-all"),
        ("attr-source-attached-daemon", "git --attr-source=HEAD daemon --export-all"),
        ("shallow-file-daemon", "git --shallow-file x daemon"),
        ("super-prefix-instaweb", "git --super-prefix x/ instaweb"),
    )
    for label, command in refused:
        case = harness.case(f"policy-refuse-{label}")
        python, rust = harness.invoke(case, ("check", command))
        report.exact(f"policy/refused/{label}", python, rust, expected_rc=77)
        python, rust = harness.invoke(case, ("--json", "check", command))
        report.exact(f"policy/refused-json/{label}", python, rust, expected_rc=77)

    # A refused run is audited; the REFUSED line must carry the same reason in both editions.
    audited = {".herdr-run.yaml": "spool_dir: .audit-spool\n"}
    for label, command in (
        ("bidi", "curl\u202ex"),
        ("control", "git status\x1b[2J"),
        ("tilde-program", "~root x"),
    ):
        case = harness.case(f"policy-refused-run-{label}", audited)
        python, rust = harness.invoke(case, ("--agent", "fixture-agent", "run", "--dry-run", command))
        report.exact(f"policy/refused-run/{label}", python, rust, expected_rc=77)
        report.exact(
            f"policy/refused-run/{label}/audit",
            _read_normalized(case.python_root / ".audit-spool" / "audit.jsonl", case.python_root),
            _read_normalized(case.rust_root / ".audit-spool" / "audit.jsonl", case.rust_root),
            expected_rc=0,
        )

    wildcard = {".herdr-run.yaml": 'allow: ["*"]\nprefixes: []\n'}
    for label, command, expected in (
        ("tilde-user-program", "~root x", 77),
        ("tilde-program", "~ x", 77),
        ("tilde-path-program", "~/bin/x y", 77),
        ("any-program", "curl https://example.invalid", 0),
        ("bidi-program", "'\u202ex' y", 0),
        ("no-break-space-program", "'tool\u00a0' 'sub\u2066'", 0),
    ):
        case = harness.case(f"policy-wildcard-{label}", wildcard)
        python, rust = harness.invoke(case, ("check", command))
        report.exact(f"policy/wildcard/{label}", python, rust, expected_rc=expected)

    double_dash = {
        ".herdr-run.yaml": 'allow: [git]\nprefixes: []\ndeny_global: {git: ["--", "-x"]}\n'
    }
    for label, command, expected in (
        ("clustered-short", "git -x-y log", 77),
        ("double-dash-token", "git -- log", 77),
        ("long-option", "git --x log", 0),
    ):
        case = harness.case(f"policy-deny-double-dash-{label}", double_dash)
        python, rust = harness.invoke(case, ("check", command))
        report.exact(f"policy/deny-double-dash/{label}", python, rust, expected_rc=expected)

    widened = {
        ".herdr-run.yaml": "allow: [git, cargo]\nprefixes: []\n",
    }
    for label, command in (
        ("fetch", "cargo fetch"),
        ("update", "cargo update -p serde"),
    ):
        case = harness.case(f"policy-custom-widened-{label}", widened)
        python, rust = harness.invoke(case, ("check", command))
        report.exact(f"policy/custom-widened/{label}", python, rust, expected_rc=0)

    for label, command in (
        ("config-before", "cargo --config build.rustc-wrapper=/tmp/evil fetch"),
        ("config-after", "cargo fetch --config build.rustc-wrapper=/tmp/evil"),
        ("config-attached-after", "cargo fetch --config=build.rustc-wrapper=/tmp/evil"),
        ("z-before-attached", "cargo -Zunstable-options fetch"),
        ("z-after", "cargo fetch -Z unstable-options"),
        ("z-after-attached", "cargo fetch -Zunstable-options"),
        ("z-after-clustered", "cargo fetch -qZunstable-options"),
    ):
        case = harness.case(f"policy-custom-widened-refuse-{label}", widened)
        python, rust = harness.invoke(case, ("check", command))
        report.exact(f"policy/custom-widened/refused/{label}", python, rust, expected_rc=77)

    minimum_cargo_guards = {
        ".herdr-run.yaml": (
            "allow: [cargo]\n"
            "deny_global: {}\n"
            "allow_subcommand:\n  cargo: [fetch]\n"
        ),
    }
    for label, command in (
        ("config-attached", "cargo fetch --config=x"),
        ("unstable-attached", "cargo fetch -Zunstable-options"),
    ):
        case = harness.case(f"policy-cargo-minimum-guard-{label}", minimum_cargo_guards)
        python, rust = harness.invoke(case, ("check", command))
        report.exact(f"policy/cargo-minimum-guard/{label}", python, rust, expected_rc=77)

    narrowed = {
        ".herdr-run.yaml": "allow: [git]\nprefixes: []\n",
    }
    case = harness.case("policy-custom-narrowed", narrowed)
    python, rust = harness.invoke(case, ("check", "gh pr list"))
    report.exact("policy/custom-narrowed", python, rust, expected_rc=77)

    # A deny entry ending in an invisible character used to be accepted, and then never matched:
    # `git -c core.pager=evil log` was ALLOWED. The configuration is refused instead, and so is
    # one ending in a character that draws as whatever the font makes of it.
    for label, entry in (
        ("combining-grapheme-joiner", "-c\u034f"),
        ("variation-selector", "-c\ufe0f"),
        ("hangul-filler", "-c\u3164"),
        ("braille-blank", "-c\u2800"),
        ("supplementary-variation-selector", "-c\U000e0100"),
        ("unassigned-tag-block", "-c\U000e0080"),
        ("null-notehead", "-c\U0001d159"),
        ("khitan-filler", "-c\U00016fe4"),
        ("noncharacter", "-c\ufdd0"),
        ("private-use", "-c\ue000"),
        ("supplementary-private-use", "-c\U00100000"),
        ("unassigned", "-c\u0378"),
        ("unassigned-supplementary", "-c\U000323b0"),
    ):
        case = harness.case(
            f"policy-invisible-deny-{label}",
            {".herdr-run.yaml": f"allow: [git]\ndeny_global:\n  git: [{entry}, --exec-path]\n"},
        )
        python, rust = harness.invoke(case, ("check", "git -c core.pager=evil log"))
        report.exact(f"policy/invisible-deny/{label}", python, rust, expected_rc=78)

    # A project's own option scoped to one subcommand, matched with the same abbreviation rule.
    scoped = {
        ".herdr-run.yaml": "allow: [gh]\nprefixes: []\n"
        "deny_subcommand_option:\n  gh:\n    pr: [--web]\n"
    }
    for label, command, expected in (
        ("denied", "gh pr view 3 --web", 77),
        ("abbreviated", "gh pr view 3 --we", 77),
        ("other-subcommand", "gh issue view 3 --web", 0),
    ):
        case = harness.case(f"policy-subcommand-option-{label}", scoped)
        python, rust = harness.invoke(case, ("check", command))
        report.exact(f"policy/subcommand-option/{label}", python, rust, expected_rc=expected)
        python, rust = harness.invoke(case, ("--json", "check", command))
        report.exact(f"policy/subcommand-option-json/{label}", python, rust, expected_rc=expected)

    # `~` is expanded before the policy judges an argument, so HOME cannot smuggle an option, a
    # subcommand, or a program past it. Each refused case here was ALLOWED, and then executed as
    # the expanded argv, while the policy judged the token before expansion.
    audited_home = {".herdr-run.yaml": "spool_dir: .audit-spool\n"}
    for label, home, command, expected in (
        ("exec-path", "--exec-path=/tmp/evil", "git ~ foo", 77),
        ("daemon", "daemon", "git ~ foo", 77),
        ("upload-pack", "--upload-pack=/tmp/evil", "git fetch ~", 77),
        ("upload-pack-gh", "--upload-pack=/tmp/evil", "gh repo view ~", 77),
        ("git-config", "-c", "git ~ core.pager=evil log", 77),
        ("program", "git", "~ status", 77),
        ("wrapper", "with-proxy", "~ git status", 77),
        ("control", "/a\x1bb", "git -C ~ log", 77),
        ("value-option", "-C", "git ~ /tmp log", 0),
        ("path", "<ROOT>/.home", "git -C ~/x status", 0),
        ("after-subcommand", "--exec-path=/x", "git log ~", 0),
    ):
        case = harness.case(f"policy-home-{label}", audited_home)
        environment = {"HOME": home}
        python, rust = harness.invoke(case, ("check", command), extra_env=environment)
        report.exact(f"policy/home/{label}", python, rust, expected_rc=expected)
        python, rust = harness.invoke(case, ("--json", "check", command), extra_env=environment)
        report.exact(f"policy/home-json/{label}", python, rust, expected_rc=expected)
        python, rust = harness.invoke(
            case, ("--agent", "a", "run", "--dry-run", command), extra_env=environment
        )
        report.exact(f"policy/home-dry-run/{label}", python, rust, expected_rc=expected)
        report.exact(
            f"policy/home-dry-run/{label}/audit",
            _read_normalized(case.python_root / ".audit-spool" / "audit.jsonl", case.python_root),
            _read_normalized(case.rust_root / ".audit-spool" / "audit.jsonl", case.rust_root),
            expected_rc=0,
        )


def _cli_grammar(harness: Harness, report: Report) -> None:
    """``--timeout`` and ``--wait-ready`` share one grammar: ASCII digits, whole milliseconds."""

    case = harness.case("cli-seconds")
    values = (
        "5",
        "0",
        "+5",
        ".5",
        "5.",
        "1e2",
        "1E1",
        "0.001",
        "0.0005",
        "0.0001",
        "31536000",
        "31536001",
        "1e300",
        "-1",
        "inf",
        "nan",
        "1_0",
        " 5",
        "5 ",
        "0x10",
        "",
        "\u0661\u0660",
        "1e\u0663",
        "5\u202e",
        "\u0665",
        "\uff15",
        "it's",
        "\x1b",
    )
    for value in values:
        for option in ("--timeout", "--wait-ready"):
            python, rust = harness.invoke(
                case, ("run", f"{option}={value}", "--dry-run", "git status")
            )
            report.same(
                f"grammar/{option}={value!r}", python, rust, allowed=frozenset({0, 2})
            )
    python, rust = harness.invoke(case, ("run", "--timeout", "-5", "--dry-run", "git status"))
    report.same("grammar/--timeout-separate-negative", python, rust, allowed=frozenset({2}))


def _agents(harness: Harness, report: Report) -> None:
    """Agent inference: one variable order, Unicode White_Space trimmed, names checked, non-UTF-8
    refused."""

    case = harness.case("agents")
    environments: tuple[tuple[str, dict[str, str]], ...] = (
        ("none", {}),
        ("herdr", {"HERDR_RUN_AGENT": "herdr-agent"}),
        ("orc-only", {"ORC_AGENT_NAME": "orc"}),
        ("blank-then-dg", {"HERDR_RUN_AGENT": " \t ", "DG_AGENT_NAME": "  dg  "}),
        ("empty-then-orc", {"HERDR_RUN_AGENT": "", "DG_AGENT_NAME": "", "ORC_AGENT_NAME": "o"}),
        ("ascii-whitespace", {"HERDR_RUN_AGENT": "\x0b\x0cagent\r\n"}),
        ("separator-not-trimmed", {"HERDR_RUN_AGENT": "\x1cagent\x1c"}),
        ("ideographic-space", {"HERDR_RUN_AGENT": "\u3000agent\u3000"}),
        ("no-break-space", {"HERDR_RUN_AGENT": "\u00a0agent"}),
        # Only Unicode White_Space: the variable is empty, and the next one names the agent.
        ("only-ideographic-space", {"HERDR_RUN_AGENT": "\u3000", "DG_AGENT_NAME": "dg"}),
        ("only-no-break-space", {"HERDR_RUN_AGENT": "\u00a0\u00a0", "DG_AGENT_NAME": "dg"}),
        ("only-line-separator", {"HERDR_RUN_AGENT": "\u2028", "ORC_AGENT_NAME": "orc"}),
        ("only-next-line", {"HERDR_RUN_AGENT": "\u0085", "DG_AGENT_NAME": "dg"}),
        # What is left may hold no control or invisible character.
        ("inner-no-break-space", {"HERDR_RUN_AGENT": "a\u00a0b"}),
        ("bidi-override", {"HERDR_RUN_AGENT": "agent\u202e"}),
        ("zero-width-space", {"HERDR_RUN_AGENT": "\u200b"}),
        ("inner-tab", {"HERDR_RUN_AGENT": "a\tb"}),
        ("non-ascii", {"HERDR_RUN_AGENT": "caf\u00e9"}),
        ("non-utf8", {"HERDR_RUN_AGENT": "\udcff"}),
        ("non-utf8-later", {"HERDR_RUN_AGENT": "", "DG_AGENT_NAME": "\udcfe"}),
        ("non-utf8-shadowed", {"HERDR_RUN_AGENT": "first", "DG_AGENT_NAME": "\udcfe"}),
    )
    for label, environment in environments:
        python, rust = harness.invoke(case, ("config",), extra_env=environment)
        report.same(f"agents/env/{label}", python, rust, allowed=frozenset({0, 78}))
    # A subcommand that names no tab never reads the agent, so a bad variable cannot break it.
    python, rust = harness.invoke(case, ("check", "git status"), extra_env={"HERDR_RUN_AGENT": "\udcff"})
    report.exact("agents/env/unread-by-check", python, rust, expected_rc=0)
    for label, value in (
        ("tab", "a\tb"),
        ("empty", ""),
        ("padded", " x "),
        ("non-utf8", "\udcff"),
        ("non-ascii", "caf\u00e9"),
        ("brace", "{agent}"),
        ("bidi-override", "a\u202eb"),
        ("no-break-space", "\u00a0"),
        ("word-joiner", "x\u2060y"),
        ("private-use", "agent\ue000"),
        ("unassigned", "a\u0378"),
        ("noncharacter", "a\ufdd0"),
        ("null-notehead", "x\U0001d159"),
        ("unicode-16-letter", "\u1c89"),
    ):
        python, rust = harness.invoke(case, ("--agent", value, "config"))
        report.same(f"agents/flag/{label}", python, rust, allowed=frozenset({0, 2, 78}))
    # The guide's promise: a variable holding only Unicode White_Space is skipped.
    python, _ = harness.invoke(
        case, ("config",), extra_env={"HERDR_RUN_AGENT": "\u3000", "DG_AGENT_NAME": "dg"}
    )
    report.require(
        "agents/env/white-space-falls-through",
        '"tab_label": "dg"' in python.stdout,
        f"a variable of only U+3000 must fall through to the next: {_describe(python)}",
    )


def _config_success(harness: Harness, report: Report) -> None:
    case = harness.case("config-default")
    python, rust = harness.invoke(case, ("--agent", "fixture-agent", "config"))
    report.exact("config/default", python, rust, expected_rc=0)
    report.require(
        "config/default-fields",
        '"source": "(built-in defaults)"' in python.stdout
        and '"project_root": "<ROOT>"' in python.stdout
        and '"tab_label": "fixture-agent"' in python.stdout
        and '"max_panes": 32' in python.stdout,
        f"default config omitted resolved fields: {_describe(python)}",
    )

    case = harness.case("config-trimmed-environment-agent")
    python, rust = harness.invoke(
        case,
        ("config",),
        extra_env={
            "HERDR_RUN_AGENT": "   ",
            "DG_AGENT_NAME": "  fixture-agent  ",
            "ORC_AGENT_NAME": "ignored-agent",
        },
    )
    report.exact("config/trimmed-environment-agent", python, rust, expected_rc=0)
    report.require(
        "config/trimmed-environment-agent-value",
        '"tab_label": "fixture-agent"' in python.stdout,
        f"environment agent was not trimmed: {_describe(python)}",
    )

    full = """\
workspace: command-deck
tab_name: "{project}-{agent}"
cwd: work/tree
allow: [git, gh, cargo]
prefixes: [with-proxy, envwrap]
deny_global:
  git: [-c]
  gh: []
  cargo: []
deny_subcommand:
  git: [daemon]
  gh: [alias]
  cargo: [install]
deny_anywhere: [--upload-pack]
allow_subcommand:
  cargo: [fetch, update]
value_options:
  git: [-C]
  gh: [-R]
  cargo: [--manifest-path]
spool_dir: .state/herdr
timeout_seconds: 12.5
retention_days: 7
max_panes: 12
ready_timeout_seconds: 3
readiness: process
prompt_tail: "$ "
shells: [bash, zsh]
probe_remote: https://example.invalid/repository
broker: systemd-run
"""
    case = harness.case("config-full", {".herdr-run.yaml": full})
    python, rust = harness.invoke(case, ("--agent", "fixture-agent", "config"))
    report.exact("config/full", python, rust, expected_rc=0)
    python, rust = harness.invoke(case, ("--json", "--agent", "fixture-agent", "config"))
    report.exact("config/full-json", python, rust, expected_rc=0)

    discovery = {
        ".herdr-run.yaml": "workspace: outer\n",
        "slot/.herdr-run.yaml": "workspace: inner\ntab_name: '{project}-{agent}'\n",
    }
    case = harness.case("config-nearest", discovery)
    python, rust = harness.invoke(
        case,
        ("--agent", "fixture-agent", "config"),
        cwd="slot/deep",
    )
    report.exact("config/discovery-nearest", python, rust, expected_rc=0)
    report.require(
        "config/discovery-nearest-value",
        '"workspace": "inner"' in python.stdout
        and '"project_root": "<ROOT>/slot"' in python.stdout,
        f"nearest config was not selected: {_describe(python)}",
    )

    precedence = {
        ".herdr-run.yaml": "workspace: yaml-wins\n",
        ".herdr-run.yml": "workspace: yml-loses\n",
    }
    case = harness.case("config-extension-precedence", precedence)
    python, rust = harness.invoke(case, ("config",))
    report.exact("config/yaml-before-yml", python, rust, expected_rc=0)
    report.require(
        "config/yaml-before-yml-value",
        '"workspace": "yaml-wins"' in python.stdout,
        f".yaml did not win over .yml: {_describe(python)}",
    )

    yaml_12 = """\
workspace: yes
allow: [git, on, off]
timeout_seconds: 0o10
"""
    case = harness.case("config-yaml-12", {".herdr-run.yaml": yaml_12})
    python, rust = harness.invoke(case, ("config",))
    report.exact("config/plain-scalar-rule", python, rust, expected_rc=0)
    report.require(
        "config/plain-scalar-rule-values",
        '"workspace": "yes"' in python.stdout
        and '"on"' in python.stdout
        and '"off"' in python.stdout
        and '"timeout_seconds": 8.0' in python.stdout,
        f"plain scalars resolved against the documented rule: {_describe(python)}",
    )

    # An explicit configuration path, spelled every way a caller might spell it.
    case = harness.case("config-explicit-paths", {"conf/settings.yaml": "workspace: explicit\n"})
    for label, path in (
        ("absolute", "<ROOT>/conf/settings.yaml"),
        ("double-slash-prefix", "/<ROOT>/conf/settings.yaml"),
        ("double-slash-inside", "<ROOT>//conf//settings.yaml"),
        ("relative", "conf/settings.yaml"),
        ("relative-dot-dot", "conf/../conf/settings.yaml"),
        ("relative-dot", "./conf/./settings.yaml"),
    ):
        python, rust = harness.invoke(case, ("--config", path, "config"))
        report.exact(f"config/explicit-path/{label}", python, rust, expected_rc=0)
        if harness.account_unavailable(report, f"config/explicit-path/{label}/status"):
            continue
        python, rust = harness.invoke(case, ("--config", path, "status"), account=True)
        report.exact(f"config/explicit-path/{label}/status", python, rust, expected_rc=0)
    if harness.shim is not None:
        harness.account_calls(report, "config/explicit-path", case)


def _malformed_contract(outcome: Outcome) -> bool:
    return (
        outcome.returncode == 78
        and outcome.stdout == ""
        and bool(outcome.stderr.strip())
        and outcome.stderr.startswith("herdr-run:")
        and not _crashed(outcome)
    )


def _config_malformed(harness: Harness, report: Report) -> None:
    malformed = (
        ("invalid-yaml", "workspace: [\n"),
        ("duplicate-top-key", "workspace: one\nworkspace: two\n"),
        (
            "duplicate-nested-key",
            "deny_global:\n  git: []\n  git: [--exec-path]\n",
        ),
        ("merge-key", "deny_global:\n  <<: {git: []}\n"),
        ("nonfinite-inf", "timeout_seconds: .inf\n"),
        ("nonfinite-nan", "timeout_seconds: .nan\n"),
        ("unknown-key", "allowlist: [git]\n"),
        ("unknown-keys-sorted", "zzz: 1\naaa: 2\n"),
        ("allow-scalar", "allow: git\n"),
        ("allow-non-string", "allow: [git, true]\n"),
        ("empty-allow", "allow: []\n"),
        (
            "cargo-positive-list-omitted",
            "allow: [git, cargo]\nallow_subcommand:\n  custom-tool: [inspect]\n",
        ),
        ("negative-timeout", "timeout_seconds: -1\n"),
        ("excessive-timeout", "timeout_seconds: 1e300\n"),
        ("timeout-just-over", "timeout_seconds: 31536001\n"),
        ("timeout-sub-millisecond", "timeout_seconds: 0.00001\n"),
        ("timeout-half-millisecond", "timeout_seconds: 0.0005\n"),
        ("ready-timeout-sub-millisecond", "ready_timeout_seconds: 0.0001\n"),
        ("negative-retention", "retention_days: -1\n"),
        ("fractional-retention", "retention_days: 1.5\n"),
        ("negative-max-panes", "max_panes: -1\n"),
        ("fractional-max-panes", "max_panes: 1.5\n"),
        ("boolean-max-panes", "max_panes: true\n"),
        ("excessive-max-panes", "max_panes: 1000001\n"),
        ("malformed-tab-template", 'tab_name: "{agent"\n'),
        ("unmatched-close-tab-template", 'tab_name: "agent}"\n'),
        ("unknown-tab-placeholder", 'tab_name: "{x}"\n'),
        ("attribute-tab-template", 'tab_name: "{agent.__class__}"\n'),
        ("format-tab-template", 'tab_name: "{agent:>10}"\n'),
        ("conversion-tab-template", 'tab_name: "{agent!r}"\n'),
        ("positional-tab-template", 'tab_name: "{}"\n'),
        ("quoted-tab-template", "tab_name: \"it's {\"\n"),
        ("config-control", 'workspace: "bad\\x1bname"\n'),
        ("config-c1-control", 'workspace: "bad\\u0085name"\n'),
        ("custom-tag", "workspace: !evil nope\n"),
        ("bad-readiness", "readiness: maybe\n"),
        ("bad-broker", "broker: shell\n"),
        ("huge-integer", "timeout_seconds: " + "9" * 5_000 + "\n"),
        ("unicode-surrogate", 'workspace: "\\uD800"\n'),
        ("workspace-number", "workspace: 5\n"),
        ("workspace-list", "workspace: [a]\n"),
        ("workspace-mapping", "workspace: {a: b}\n"),
        ("workspace-bool", "workspace: true\n"),
        ("workspace-float", "workspace: 1.5\n"),
        ("workspace-empty", "workspace: ''\n"),
        ("non-string-mapping-key", "allow: [git]\nallow_subcommand: {true: [x]}\n"),
        ("order-tab-before-allow", "tab_name: '{x}'\nallow: 5\n"),
        ("order-deny-anywhere-before-allow-subcommand", "deny_anywhere: 5\nallow_subcommand: 5\n"),
        ("order-broker-before-probe-remote", "probe_remote: 5\nbroker: 5\n"),
        ("order-unknown-before-type", "zzz: 1\nallow: 5\n"),
        ("deny-global-non-string", "deny_global: {git: [5]}\n"),
        ("deny-global-scalar", "deny_global: {git: x}\n"),
        ("deny-global-list", "deny_global: [git]\n"),
        ("value-options-control", 'value_options: {git: ["-\\tC"]}\n'),
        ("prompt-tail-number", "prompt_tail: 5\n"),
        ("shells-scalar", "shells: bash\n"),
        ("probe-remote-control", 'probe_remote: "a\\nb"\n'),
        ("workspace-null", "workspace: ~\n"),
        ("cwd-number", "cwd: 5\n"),
        ("retention-huge", "retention_days: " + "9" * 40 + "\n"),
        ("root-sequence", "- a\n- b\n"),
        ("root-scalar", "just text\n"),
        # A deny entry that looks like `-c` but is not would never match the `-c` it appears to
        # deny, so every name refuses an invisible character, and a raw U+0085/U+2028/U+2029 is
        # refused anywhere: YAML 1.1 reads it as a line break and YAML 1.2 does not.
        (
            "raw-line-separator-in-deny",
            b"allow: [git]\ndeny_global:\n  git: [-c\xe2\x80\xa8, --exec-path\xe2\x80\xa9]\n",
        ),
        ("raw-line-separator-in-deny-anywhere", b"deny_anywhere: [--upload-pack\xe2\x80\xa8]\n"),
        ("raw-paragraph-separator-in-value-options", b"value_options:\n  git: [-C\xe2\x80\xa9]\n"),
        ("raw-next-line-in-comment", b"allow: [git]\n# x\xc2\x85allow: [curl]\n"),
        ("escaped-line-separator-in-deny", 'allow: [git]\ndeny_global:\n  git: ["-c\\L", --exec-path]\n'),
        ("zero-width-space-in-deny-anywhere", 'deny_anywhere: ["--upload-pack\\u200b"]\n'),
        ("byte-order-mark-in-value-options", 'value_options:\n  git: ["-C\\ufeff"]\n'),
        ("no-break-space-in-allow", 'allow: ["git\\u00a0"]\n'),
        ("bidi-override-in-prefixes", 'prefixes: ["with-proxy\\u202e"]\n'),
        ("format-character", 'workspace: "a\\u202eb"\n'),
        ("word-joiner-in-tab-name", 'tab_name: "{agent}\\u2060"\n'),
        ("ideographic-space-in-deny-subcommand-key", 'deny_subcommand:\n  "git\\u3000": [daemon]\n'),
        # Default-ignorable code points and blank characters are invisible too: a deny entry
        # ending in a combining grapheme joiner never matched the `-c` it appeared to deny.
        (
            "combining-grapheme-joiner-in-deny",
            "allow: [git]\ndeny_global:\n  git: [-c\u034f, --exec-path]\n",
        ),
        ("hangul-filler-in-allow", 'allow: [git, "\\u3164"]\n'),
        ("halfwidth-hangul-filler-in-workspace", 'workspace: "a\\uffa0"\n'),
        ("hangul-choseong-filler-in-deny-subcommand", 'deny_subcommand:\n  git: ["daemon\\u115f"]\n'),
        ("khmer-inherent-vowel-in-tab-name", 'tab_name: "{agent}\\u17b4"\n'),
        ("mongolian-variation-selector-in-key", 'allow_subcommand:\n  "git\\u180b": [status]\n'),
        ("variation-selector-in-deny-anywhere", 'deny_anywhere: ["--exec\\ufe0f"]\n'),
        ("supplementary-variation-selector", 'value_options:\n  git: ["-C\\U000e0100"]\n'),
        ("unassigned-default-ignorable", 'shells: ["bash\\U000e0080"]\n'),
        ("braille-blank-in-prefixes", 'prefixes: ["with-proxy\\u2800"]\n'),
        (
            "invisible-in-deny-subcommand-option",
            'deny_subcommand_option:\n  git:\n    clone: ["-u\\u2065"]\n',
        ),
        ("deny-subcommand-option-not-an-object", "deny_subcommand_option:\n  git: [-u]\n"),
        # Flow forms that YAML readers read differently are refused rather than read one way.
        ("question-mark-in-flow-mapping", "allow: [git]\ndeny_subcommand: {?git: [log]}\n"),
        ("question-mark-in-flow-sequence", "allow: [?git]\n"),
        ("colon-starting-a-flow-line", "{workspace\n :elsewhere}\n"),
        ("not-utf8", b"workspace: caf\xe9\n"),
        ("not-utf8-in-comment", b"# \xff\nworkspace: x\n"),
        ("nul-byte", b"workspace: a\x00b\n"),
    )
    for label, document in malformed:
        case = harness.case(f"config-malformed-{label}", {".herdr-run.yaml": document})
        python, rust = harness.invoke(case, ("config",))
        report.exact(f"config/malformed/{label}", python, rust, expected_rc=78)
        report.require(
            f"config/malformed/{label}/contract",
            _malformed_contract(python),
            "expected a fail-closed rc 78 with a herdr-run diagnostic and no stdout; "
            f"python={_describe(python)}",
        )

    accepted = (
        ("empty-file", ""),
        ("comment-only", "# nothing\n"),
        ("document-markers", "---\nworkspace: marked\n...\n"),
        ("byte-order-mark", "\ufeffworkspace: bom\n"),
        ("crlf", "workspace: crlf\r\nallow: [git]\r\n"),
        ("float-formats", "timeout_seconds: 12.5\nready_timeout_seconds: 3\n"),
        ("float-exponent", "timeout_seconds: 1e3\nready_timeout_seconds: 1e-3\n"),
        ("float-large", "timeout_seconds: 31536000\nready_timeout_seconds: 100000.001\n"),
        ("integer-forms", "max_panes: 0x10\nretention_days: 0o7\n"),
        ("zero-timeout", "timeout_seconds: 0\n"),
        ("explicit-tags", "workspace: !!str 12\nmax_panes: !!int '5'\ntimeout_seconds: !!float 1\n"),
        ("leading-zero-string", "workspace: 007\n"),
        ("non-ascii", "workspace: caf\u00e9\n"),
        ("invisible-outside-names", 'prompt_tail: "\\u00a0$ "\nspool_dir: "out\\u00a0dir"\n'),
        ("doubled-braces", "tab_name: '{{agent}}-{agent}'\n"),
        ("cwd-absolute-dot-dot", "cwd: /a/../b\n"),
        ("cwd-relative-dot-dot", "cwd: ./x/../y\n"),
        ("cwd-double-slash", "cwd: //srv/x\n"),
        ("cwd-relative-double-slash", "cwd: x//y/\n"),
        ("spool-dir-absolute", "spool_dir: /var/tmp/herdr-spool\n"),
        ("null-values", "cwd: null\nprompt_tail:\n"),
        ("spool-dir-empty", "spool_dir: ''\n"),
        ("allow-any", 'allow: ["*"]\n'),
        ("max-panes-limit", "max_panes: 1000000\n"),
        ("retention-zero", "retention_days: 0\n"),
    )
    for label, document in accepted:
        case = harness.case(f"config-accepted-{label}", {".herdr-run.yaml": document})
        python, rust = harness.invoke(case, ("--agent", "fixture-agent", "config"))
        report.exact(f"config/accepted/{label}", python, rust, expected_rc=0)

    # Every document in the shared YAML corpus, read as a configuration file: the accepted ones
    # meet the configuration rules, the refused ones carry their positioned YAML message.
    corpus = cast(
        dict[str, list[dict[str, str]]],
        json.loads(
            (REPO_ROOT / "rs" / "herdr-run" / "testdata" / "yaml_cases.json").read_text(
                encoding="utf-8"
            )
        ),
    )
    for entry in corpus["cases"]:
        case = harness.case(f"config-yaml-{entry['name']}", {".herdr-run.yaml": entry["text"]})
        python, rust = harness.invoke(case, ("config",))
        report.same(
            f"config/yaml-corpus/{entry['name']}", python, rust, allowed=frozenset({0, 78})
        )

    # The reported repro: the file is refused, so the command it would have let through is not run.
    repro = b"allow: [git]\ndeny_global:\n  git: [-c\xe2\x80\xa8, --exec-path\xe2\x80\xa9]\n"
    case = harness.case("config-line-separator-repro", {".herdr-run.yaml": repro})
    python, rust = harness.invoke(case, ("check", "git -c core.pager=evil log"))
    report.exact("config/line-separator-repro/check", python, rust, expected_rc=78)

    case = harness.case("config-missing-explicit")
    python, rust = harness.invoke(case, ("--config", "missing.yaml", "config"))
    report.exact("config/malformed/missing-explicit", python, rust, expected_rc=78)
    report.require(
        "config/malformed/missing-explicit/contract",
        _malformed_contract(python),
        f"missing explicit config did not fail as configuration error; python={_describe(python)}",
    )


def _os_errors(harness: Harness, report: Report) -> None:
    """Every OS error reads ``<strerror> (os error N)`` in both editions, at the same moment."""

    case = harness.case("os-directory-as-config", setup=lambda root: (root / ".herdr-run.yaml").mkdir())
    python, rust = harness.invoke(case, ("config",))
    report.same("os/config-is-a-directory/discovered", python, rust, allowed=frozenset({0, 78}))
    python, rust = harness.invoke(case, ("--config", "<ROOT>/.herdr-run.yaml", "config"))
    report.exact("os/config-is-a-directory/explicit", python, rust, expected_rc=78)
    python, rust = harness.invoke(case, ("init", "--force"))
    report.same("os/init-over-a-directory", python, rust, allowed=frozenset({78}))

    case = harness.case(
        "os-dangling-symlink",
        setup=lambda root: (root / ".herdr-run.yaml").symlink_to(root / "missing-target.yaml"),
    )
    python, rust = harness.invoke(case, ("config",))
    report.same("os/dangling-symlink/discovered", python, rust, allowed=frozenset({0, 78}))
    python, rust = harness.invoke(case, ("--config", ".herdr-run.yaml", "config"))
    report.exact("os/dangling-symlink/explicit", python, rust, expected_rc=78)
    python, rust = harness.invoke(case, ("init",))
    report.same("os/dangling-symlink/init", python, rust, allowed=frozenset({0, 78}))
    python, rust = harness.invoke(case, ("init", "--force"))
    report.same("os/dangling-symlink/init-force", python, rust, allowed=frozenset({0, 78}))

    case = harness.case("os-missing-parent")
    python, rust = harness.invoke(case, ("--config", "no/such/dir/x.yaml", "config"))
    report.exact("os/missing-parent", python, rust, expected_rc=78)
    python, rust = harness.invoke(case, ("--config", "", "config"))
    report.same("os/empty-config-path", python, rust, allowed=frozenset({2, 78}))

    if os.geteuid() == 0:
        report.notes.append(
            "permission-denied cases were not run: the harness is running as root, which reads "
            "and writes regardless of mode bits"
        )
        return

    unreadable = harness.case(
        "os-unreadable-config",
        {".herdr-run.yaml": "workspace: x\n"},
        setup=lambda root: (root / ".herdr-run.yaml").chmod(0),
    )
    python, rust = harness.invoke(unreadable, ("config",))
    report.exact("os/unreadable-config", python, rust, expected_rc=78)

    read_only = harness.case("os-read-only-directory")
    for root in (read_only.python_root, read_only.rust_root):
        root.chmod(0o555)
    try:
        python, rust = harness.invoke(read_only, ("init",))
        report.exact("os/init-in-read-only-directory", python, rust, expected_rc=78)
    finally:
        for root in (read_only.python_root, read_only.rust_root):
            root.chmod(0o755)

    spool_blocked = harness.case(
        "os-spool-parent-is-a-file",
        {".herdr-run.yaml": "spool_dir: blocker/spool\n", "blocker": "not a directory\n"},
    )
    python, rust = harness.invoke(
        spool_blocked, ("--agent", "fixture-agent", "run", "--dry-run", "git status")
    )
    report.exact("os/spool-parent-is-a-file", python, rust, expected_rc=0)


def _non_utf8(harness: Harness, report: Report) -> None:
    """Bytes that are not UTF-8 in argv, the environment, and paths are handled identically."""

    case = harness.case("bytes-argv", {".herdr-run.yaml": "allow: [git, printf]\nprefixes: []\n"})
    for label, args in (
        ("check-argument", ("check", "git \udcff")),
        ("check-program", ("check", "\udcff x")),
        ("agent", ("--agent", "\udcff", "config")),
        ("config-path", ("--config", "\udcff.yaml", "config")),
        ("cwd", ("--agent", "a", "run", "--cwd", "\udcff", "--dry-run", "git status")),
        ("timeout", ("run", "--timeout", "\udcff", "--dry-run", "git status")),
        ("subcommand", ("\udcff",)),
        ("option", ("--\udcff",)),
        ("dry-run-argument", ("--agent", "a", "run", "--dry-run", "printf '\udce9'")),
    ):
        python, rust = harness.invoke(case, args)
        report.same(f"bytes/argv/{label}", python, rust, allowed=frozenset({0, 2, 77, 78}))

    def non_utf8_directory(root: Path) -> None:
        directory = root / "\udcff"
        directory.mkdir()
        (directory / ".herdr-run.yaml").write_text("workspace: bytes\n", encoding="utf-8")
        (directory / "deeper").mkdir()

    case = harness.case("bytes-project", setup=non_utf8_directory)
    for label, cwd, args in (
        ("project-root", "\udcff", ("config",)),
        ("below-project-root", "\udcff/deeper", ("config",)),
        ("check", "\udcff", ("check", "git status")),
        ("dry-run", "\udcff", ("--agent", "a", "run", "--dry-run", "git status")),
        ("status", "\udcff", ("--agent", "a", "status")),
        ("init", "\udcff/deeper", ("init",)),
    ):
        # Every subcommand that reads the directory refuses one that is not valid UTF-8.
        python, rust = harness.invoke(case, args, cwd=cwd)
        report.same(f"bytes/project/{label}", python, rust, allowed=frozenset({0, 78}))

    case = harness.case("bytes-home", setup=lambda root: (root / ".home" / "\udcff").mkdir())
    for label, command in (
        ("tilde-argument", "git -C ~/x status"),
        ("no-tilde", "git status"),
        ("tilde-user", "git -C ~root/x status"),
    ):
        python, rust = harness.invoke(
            case, ("check", command), extra_env={"HOME": "<ROOT>/.home/\udcff"}
        )
        report.same(f"bytes/home/{label}", python, rust, allowed=frozenset({0, 77, 78}))


def _locales(harness: Harness, report: Report) -> None:
    """The locale changes nothing: argv is read and output is written as UTF-8 bytes."""

    config = {".herdr-run.yaml": "allow: [printf, git]\nprefixes: []\nworkspace: caf\u00e9\n"}
    case = harness.case("locales", config)
    for locale_name in ("C", "POSIX", "en_US", "en_US.iso885915", "C.UTF-8", "xx_XX.nonexistent"):
        environment = {"LC_ALL": locale_name, "LANG": locale_name}
        for label, args in (
            ("dry-run", ("--agent", "caf\u00e9", "run", "--dry-run", "printf '\u00e9 \u202e'")),
            ("dry-run-json", ("--json", "--agent", "a", "run", "--dry-run", "printf '\u00e9'")),
            ("refusal", ("check", "curl \u00e9")),
            ("config", ("--agent", "\u00e9", "config")),
            ("status", ("--agent", "\u00e9", "status")),
            ("non-utf8-argument", ("check", "git \udce9")),
            ("utf8-argument", ("check", "git \u65e5\u672c")),
            ("utf8-refusal", ("check", "curl \u65e5\u672c")),
            ("help", ("--help",)),
            ("quickstart", ("quickstart",)),
            # An argument that reads like the interpreter's own `-X utf8=0` is not one: it must
            # not keep the Python edition in the locale's encoding, where the byte below would
            # read as a letter instead of being refused.
            ("utf8-option-lookalike", ("--agent", "\udce9", "run", "--dry-run", "--", "-Xutf8=0")),
            ("utf8-option-lookalike-split", ("--agent", "\udce9", "check", "-X", "utf8=0")),
        ):
            # `status` asks Herdr whether a server runs: the fake one, behind the shim.
            reaches_herdr = label == "status"
            skipped = f"locale/{locale_name}/{label}"
            if reaches_herdr and harness.account_unavailable(report, skipped):
                continue
            python, rust = harness.invoke(
                case, args, extra_env=environment, account=reaches_herdr
            )
            report.same(
                f"locale/{locale_name}/{label}", python, rust, allowed=frozenset({0, 2, 77, 78})
            )
    if harness.shim is not None:
        harness.account_calls(report, "locale", case)


def _output_failures(harness: Harness, report: Report) -> None:
    """A stdout that cannot be written, or a SIGINT, ends both editions the same way."""

    case = harness.case("output-failures", {".herdr-run.yaml": "allow: [git]\nprefixes: []\n"})
    commands: tuple[tuple[str, tuple[str, ...]], ...] = (
        ("version", ("--version",)),
        ("help", ("--help",)),
        ("surface", ("--dump-surface",)),
        ("quickstart", ("quickstart",)),
        ("config", ("--agent", "a", "config")),
        ("check", ("check", "git status")),
        ("check-json", ("--json", "check", "git status")),
        ("dry-run", ("--agent", "a", "run", "--dry-run", "git status")),
        ("refusal", ("check", "curl x")),
        ("usage-error", ("--frobnicate",)),
    )
    for mode, allowed in (
        ("closed-stdout", frozenset({0, 2, 77})),
        ("dev-full", frozenset({1, 2, 77})),
        ("broken-pipe", frozenset({1, 2, 77})),
    ):
        for label, args in commands:
            python, rust = harness.invoke(case, args, mode=mode)
            report.same(f"output/{mode}/{label}", python, rust, allowed=allowed)
            # The interpreter's buffering setting must not change the outcome either: a buffered
            # write that failed used to be retried at shutdown, which printed `Exception ignored`
            # and exited 120 unless PYTHONUNBUFFERED was set.
            if mode != "closed-stdout":
                python, rust = harness.invoke(
                    case, args, mode=mode, extra_env={"PYTHONUNBUFFERED": "1"}
                )
                report.same(f"output/{mode}/{label}/unbuffered", python, rust, allowed=allowed)
    # A stderr that cannot be written is ignored, and must leave nothing behind for shutdown.
    for label, args in (("refusal", ("check", "curl x")), ("usage-error", ("--frobnicate",))):
        python, rust = harness.invoke(case, args, mode="broken-stderr")
        report.exact(
            f"output/broken-stderr/{label}", python, rust, expected_rc=77 if label == "refusal" else 2
        )
    python, rust = harness.invoke(case, ("--version",), mode="dev-full")
    report.require(
        "output/dev-full-names-the-failure",
        python.returncode == 1
        and python.stderr
        == "herdr-run: cannot write stdout: No space left on device (os error 28)\n",
        f"a full device must be reported as one line with the OS error: {_describe(python)}",
    )
    # The user guide is many pages long, so it is still being written when SIGINT arrives.
    python, rust = harness.invoke(case, ("userguide",), mode="sigint-while-writing")
    report.exact("output/sigint/userguide", python, rust, expected_rc=-signal.SIGINT)


def _dry_run(harness: Harness, report: Report) -> None:
    config = """\
allow: [printf]
prefixes: []
spool_dir: .audit-spool
"""
    case = harness.case("dry-run", {".herdr-run.yaml": config})
    command = "printf '[%s]' 'two words' '; rm -rf /' '$(id)'"
    python, rust = harness.invoke(
        case,
        ("--json", "--agent", "fixture-agent", "run", "--dry-run", command),
    )
    report.exact("dry-run/success-json", python, rust, expected_rc=0)
    expected = (
        "{\n"
        '  "program": "printf",\n'
        "  \"rendered\": \"printf '[%s]' 'two words' '; rm -rf /' '$(id)'\",\n"
        '  "verdict": "allowed"\n'
        "}\n"
    )
    report.require(
        "dry-run/success-json-shape",
        python.stdout == expected and python.stderr == "",
        f"dry-run JSON/rendering changed: {_describe(python)}",
    )
    for label, args in (
        ("text", ("--agent", "fixture-agent", "run", "--dry-run", command)),
        ("non-ascii", ("--agent", "caf\u00e9", "run", "--dry-run", "printf 'caf\u00e9 \u202e'")),
        ("tilde", ("--agent", "fixture-agent", "run", "--dry-run", "printf ~/x ~root/y")),
        ("cwd-absolute", ("--agent", "a", "run", "--cwd", "/a/../b", "--dry-run", "printf x")),
        ("cwd-relative", ("--agent", "a", "run", "--cwd", "x/../y", "--dry-run", "printf x")),
        ("timeouts", ("--agent", "a", "run", "--timeout", "1.5", "--wait-ready", "0.25", "--dry-run", "printf x")),
    ):
        python, rust = harness.invoke(case, args)
        report.exact(f"dry-run/{label}", python, rust, expected_rc=0)

    python_audit = _read_normalized(
        case.python_root / ".audit-spool" / "audit.jsonl",
        case.python_root,
    )
    rust_audit = _read_normalized(
        case.rust_root / ".audit-spool" / "audit.jsonl",
        case.rust_root,
    )
    report.exact("dry-run/audit-jsonl", python_audit, rust_audit, expected_rc=0)


def _modes(root: Path, relative: Sequence[str]) -> Outcome:
    lines = []
    for name in relative:
        try:
            mode = stat.S_IMODE((root / name).lstat().st_mode)
        except OSError as error:
            lines.append(f"{name} missing ({errno.errorcode.get(error.errno or 0, '?')})")
            continue
        lines.append(f"{name} {mode:04o}")
    return Outcome(0, "\n".join(lines) + "\n", "")


def _spool(harness: Harness, report: Report) -> None:
    """Spool directories are private at every level, and an un-ignored spool is named exactly."""

    nested = harness.case("spool-nested", {".herdr-run.yaml": "spool_dir: a/b/c\n"})
    python, rust = harness.invoke(nested, ("--agent", "a", "run", "--dry-run", "git status"))
    report.exact("spool/nested-dry-run", python, rust, expected_rc=0)
    created = ("a", "a/b", "a/b/c", "a/b/c/audit.jsonl")
    python_modes = _modes(nested.python_root, created)
    rust_modes = _modes(nested.rust_root, created)
    report.exact("spool/nested-modes", python_modes, rust_modes, expected_rc=0)
    report.require(
        "spool/nested-modes-are-private",
        python_modes.stdout == "a 0700\na/b 0700\na/b/c 0700\na/b/c/audit.jsonl 0600\n",
        f"spool directories must be 0700 and the audit log 0600: {python_modes.stdout!r}",
    )

    git = shutil.which("git")
    if git is None:
        report.notes.append("the un-ignored spool warning was not compared: git is not installed")
        return

    def work_tree(root: Path) -> None:
        subprocess.run(
            [git, "init", "-q", str(root)],
            check=True,
            capture_output=True,
            env={**os.environ, "HOME": str(root / ".home"), "GIT_CONFIG_NOSYSTEM": "1"},
        )

    for label, spool_dir in (
        ("plain", ".herdr-run"),
        ("apostrophe-and-space", "out dir/it's"),
        ("non-ascii", "caf\u00e9/spool"),
    ):
        case = harness.case(
            f"spool-tracked-{label}",
            {".herdr-run.yaml": f"spool_dir: {json.dumps(spool_dir)}\n"},
            setup=work_tree,
        )
        environment = {"GIT_CONFIG_NOSYSTEM": "1"}
        python, rust = harness.invoke(
            case, ("--agent", "a", "run", "--dry-run", "git status"), extra_env=environment
        )
        report.exact(f"spool/tracked-warning/{label}", python, rust, expected_rc=0)
        report.require(
            f"spool/tracked-warning/{label}/present",
            "is NOT git-ignored" in python.stderr,
            f"an un-ignored spool in a git work tree was not reported: {_describe(python)}",
        )

    # Started with SIGCHLD ignored, every child is reaped by the kernel and its status lost: one
    # edition read `git check-ignore`'s "not ignored" as status 0 ("ignored") and stayed silent.
    case = harness.case("spool-tracked-sigchld-ignored", setup=work_tree)
    python, rust = harness.invoke(
        case,
        ("--agent", "a", "run", "--dry-run", "git status"),
        extra_env={"GIT_CONFIG_NOSYSTEM": "1"},
        mode="sigchld-ignored",
    )
    report.exact("spool/tracked-warning/sigchld-ignored", python, rust, expected_rc=0)
    report.require(
        "spool/tracked-warning/sigchld-ignored/present",
        "is NOT git-ignored" in python.stderr,
        f"an inherited ignored SIGCHLD hid the un-ignored spool: {_describe(python)}",
    )


def _net_doctor(harness: Harness, report: Report) -> None:
    """net-doctor refuses a probe the policy does not admit, before it runs anything."""

    for label, probe in (
        ("tilde", "~/repo"),
        ("tilde-user", "~nosuchuser-herdr/repo"),
        ("apostrophe", "/srv/it's repo"),
        ("control", "a\\u001bb"),
    ):
        config = f'allow: [gh]\nprobe_remote: "{probe}"\nspool_dir: .audit-spool\n'
        case = harness.case(f"net-doctor-{label}", {".herdr-run.yaml": config})
        python, rust = harness.invoke(case, ("--agent", "fixture-agent", "net-doctor"))
        report.same(f"net-doctor/refused/{label}", python, rust, allowed=frozenset({77, 78}))
        report.exact(
            f"net-doctor/refused/{label}/audit",
            _read_normalized(case.python_root / ".audit-spool" / "audit.jsonl", case.python_root),
            _read_normalized(case.rust_root / ".audit-spool" / "audit.jsonl", case.rust_root),
            expected_rc=0,
        )


def _read_normalized(path: Path, root: Path) -> Outcome:
    try:
        text = path.read_bytes().decode("utf-8", errors="surrogateescape")
    except FileNotFoundError:
        return Outcome(0, "(absent)\n", "")
    except OSError as error:
        return Outcome(1, "", f"cannot read {path.relative_to(root)}: {error.strerror}")
    return Outcome(0, _normalize_text(text, root), "")


#: Workspace label used by the reap and status fixtures. They run against the fake Herdr behind the
#: account-home shim, which has no workspace at all, so the label never resolves and a planted pane
#: stays UNKNOWN.
_REAP_WORKSPACE = "cross-differential-fixture"

_REAP_CONFIG = f"""\
workspace: {_REAP_WORKSPACE}
allow: [printf]
prefixes: []
spool_dir: .herdr-run
retention_days: 7
"""


def _reap_record(
    pane_id: str,
    workspace: object,
    exit_code: str = "0",
    *,
    agent: object = "kvm",
    tab: object = None,
) -> str:
    """One planted ``meta.json``, shaped exactly as the runner writes it."""
    return json.dumps(
        {
            "agent": agent,
            "exit_code": None if exit_code == "null" else int(exit_code),
            "pane_id": pane_id,
            "readiness": {
                "boot_id": "3f2b1c8e-0000-4000-8000-000000000001",
                "shell_pid": 4242,
                "shell_start_ticks": 900,
            },
            "run_id": "20260819T000000-kvm-1",
            "tab": {"id": "w1:t1", "label": "kvm"} if tab is None else tab,
            "workspace": {"id": "w1", "label": workspace}
            if isinstance(workspace, str)
            else workspace,
        },
        indent=2,
        sort_keys=True,
    )


def _reap(harness: Harness, report: Report) -> None:
    """Compare the REAPING VERDICTS, not just that both editions list the subcommand.

    ``reap`` is the one subcommand whose output can authorise destroying someone's running work, so
    "both help texts contain the word reap" is not a cross-edition guarantee of anything. These
    cases plant a run spool and compare the whole document byte for byte: counts, per-pane verdicts,
    reason wording, key ordering, and the retention window the candidate set is bounded by.

    Every case runs against the fake Herdr behind the account-home shim, which has no workspace,
    never against a Herdr installed on the host; the fake's calls are compared too, and must only
    read. Verdicts over live panes (STALE, SHELL_ALIVE, GONE, IN_FLIGHT) are paired in
    ``e2e/reap-*``.
    """
    if harness.account_unavailable(report, "reap/all"):
        return
    empty = harness.case("reap-empty", {".herdr-run.yaml": _REAP_CONFIG})
    python, rust = harness.invoke(empty, ("reap",), account=True)
    report.exact("reap/empty-spool", python, rust, expected_rc=0)
    report.require(
        "reap/empty-spool-shape",
        python.stderr == ""
        and '"considered": 0' in python.stdout
        and all(
            f'"{verdict}": 0' in python.stdout
            for verdict in ("STALE", "IN_FLIGHT", "SHELL_ALIVE", "GONE", "UNKNOWN", "OUT_OF_SCOPE")
        )
        and '"retention_days": 7' in python.stdout
        and f'"workspace": "{_REAP_WORKSPACE}"' in python.stdout
        and all(f'"{key}"' in python.stdout for key in ("occupancy", "max_panes", "without_record")),
        "an inert sweep must still print its own shape, the window bounding it, and what holds "
        "max_panes: "
        f"{_describe(python)}",
    )
    harness.account_calls(report, "reap/empty-spool", empty)

    scoped = harness.case(
        "reap-in-scope",
        {
            ".herdr-run.yaml": _REAP_CONFIG,
            ".herdr-run/runs/20260819T000000-kvm-1/meta.json": _reap_record(
                "w1:p1", _REAP_WORKSPACE
            ),
        },
    )
    python, rust = harness.invoke(scoped, ("reap",), account=True)
    report.exact("reap/unresolvable-workspace", python, rust, expected_rc=0)
    report.require(
        "reap/unresolvable-workspace-shape",
        '"STALE": 0' in python.stdout
        and '"UNKNOWN": 1' in python.stdout
        and '"reapable": []' in python.stdout
        and "herdr has no workspace labelled" in python.stdout,
        "a workspace herdr cannot resolve must reap nothing and say why: "
        f"{_describe(python)}",
    )
    harness.account_calls(report, "reap/unresolvable-workspace", scoped)

    foreign = harness.case(
        "reap-out-of-scope",
        {
            ".herdr-run.yaml": _REAP_CONFIG,
            ".herdr-run/runs/20260819T000000-kvm-1/meta.json": _reap_record(
                "w1:p1", "someone-elses"
            ),
        },
    )
    python, rust = harness.invoke(foreign, ("reap",), account=True)
    report.exact("reap/out-of-scope", python, rust, expected_rc=0)
    report.require(
        "reap/out-of-scope-shape",
        '"OUT_OF_SCOPE": 1' in python.stdout
        and '"STALE": 0' in python.stdout
        and '"reapable": []' in python.stdout,
        f"a pane recorded in another workspace must never be a candidate: {_describe(python)}",
    )
    harness.account_calls(report, "reap/out-of-scope", foreign)

    run_dir = ".herdr-run/runs/20260819T000000-kvm-1"
    planted: tuple[tuple[str, dict[str, str | bytes]], ...] = (
        ("tab-is-a-string", {f"{run_dir}/meta.json": _reap_record("w1:p1", _REAP_WORKSPACE, tab="x")}),
        ("workspace-is-a-number", {f"{run_dir}/meta.json": _reap_record("w1:p1", 5)}),
        ("agent-is-a-number", {f"{run_dir}/meta.json": _reap_record("w1:p1", _REAP_WORKSPACE, agent=7)}),
        (
            "apostrophe-labels",
            {
                f"{run_dir}/meta.json": _reap_record(
                    "w1:p1",
                    "it's-elsewhere",
                    agent="o'neil",
                    tab={"id": "w1:t1", "label": "o'neil"},
                )
            },
        ),
        (
            "control-labels",
            {
                f"{run_dir}/meta.json": _reap_record(
                    "w1:p1", "bad\u001bspace", tab={"id": "w1:t1", "label": "tab\u202e"}
                )
            },
        ),
        (
            "late-exit-code-not-utf8",
            {
                f"{run_dir}/meta.json": _reap_record("w1:p1", "elsewhere", "null"),
                f"{run_dir}/exit_code": b"\xff\n",
            },
        ),
        (
            "late-exit-code-valid",
            {
                f"{run_dir}/meta.json": _reap_record("w1:p1", "elsewhere", "null"),
                f"{run_dir}/exit_code": b" 7\n",
            },
        ),
        (
            "late-exit-code-out-of-range",
            {
                f"{run_dir}/meta.json": _reap_record("w1:p1", "elsewhere", "null"),
                f"{run_dir}/exit_code": b"2147483648\n",
            },
        ),
        ("meta-not-json", {f"{run_dir}/meta.json": "{not json"}),
        ("meta-not-utf8", {f"{run_dir}/meta.json": b'{"pane_id": "\xff"}'}),
        ("meta-nan", {f"{run_dir}/meta.json": '{"pane_id": "w1:p1", "exit_code": NaN}'}),
        ("meta-array", {f"{run_dir}/meta.json": "[]"}),
        # Integers no float can hold, and nesting past the shared depth limit: a skipped record in
        # both editions, never a traceback.
        (
            "meta-integer-past-float",
            {f"{run_dir}/meta.json": '{"pane_id": "w1:p1", "exit_code": 1' + "0" * 400 + "}"},
        ),
        (
            "meta-integer-past-digit-limit",
            {f"{run_dir}/meta.json": '{"pane_id": "w1:p1", "exit_code": ' + "9" * 5000 + "}"},
        ),
        (
            "meta-nested-too-deep",
            {f"{run_dir}/meta.json": '{"pane_id": "w1:p1", "x": ' + "[" * 200 + "]" * 200 + "}"},
        ),
        (
            "meta-nested-deep-enough-to-crash-a-recursive-reader",
            {f"{run_dir}/meta.json": '{"pane_id": "w1:p1", "x": ' + "[" * 20000 + "]" * 20000 + "}"},
        ),
        (
            "meta-nested-at-the-limit",
            {
                f"{run_dir}/meta.json": _reap_record("w1:p1", "elsewhere")[:-1]
                + ', "x": '
                + "[" * 126
                + "]" * 126
                + "}"
            },
        ),
    )
    for label, files in planted:
        case = harness.case(f"reap-planted-{label}", {".herdr-run.yaml": _REAP_CONFIG, **files})
        python, rust = harness.invoke(case, ("reap",), account=True)
        report.exact(f"reap/planted/{label}", python, rust, expected_rc=0)
        python, rust = harness.invoke(case, ("--json", "reap"), account=True)
        report.exact(f"reap/planted/{label}/json", python, rust, expected_rc=0)
        harness.account_calls(report, f"reap/planted/{label}", case)

    # Run directories are read oldest first by sorting their names, and the LAST record naming a
    # pane decides its scope. Sorted by raw bytes in both editions: by code point, a name that is
    # not UTF-8 (held as surrogate escapes) would sort the other way round against U+E000.
    def ordered_runs(root: Path) -> None:
        runs = root / ".herdr-run" / "runs"
        for name, workspace in (("\udcff", "someone-elses"), ("\ue000", _REAP_WORKSPACE)):
            directory = runs / name
            directory.mkdir(parents=True)
            (directory / "meta.json").write_text(
                _reap_record("w1:p1", workspace), encoding="utf-8"
            )

    case = harness.case(
        "reap-byte-ordered-runs", {".herdr-run.yaml": _REAP_CONFIG}, setup=ordered_runs
    )
    python, rust = harness.invoke(case, ("reap",), account=True)
    report.exact("reap/byte-ordered-runs", python, rust, expected_rc=0)
    report.require(
        "reap/byte-ordered-runs/last-by-bytes-wins",
        '"OUT_OF_SCOPE": 1' in python.stdout,
        f"the run directory named by byte 0xFF sorts last and must decide scope: {_describe(python)}",
    )
    harness.account_calls(report, "reap/byte-ordered-runs", case)


#: `status` fixtures run against the fake Herdr behind the account-home shim, never against a Herdr
#: installed on the host, so the report is the same everywhere.
_STATUS_CONFIG = f"""\
workspace: {_REAP_WORKSPACE}
allow: [git, gh]
prefixes: [with-proxy]
spool_dir: .herdr-run
"""

_STATUS_ANY_CONFIG = f"""\
workspace: {_REAP_WORKSPACE}
allow: ["*"]
prefixes: []
"""


def _documentation(harness: Harness, report: Report) -> None:
    """Both documentation commands print the same bytes in both editions.

    `quickstart` and `userguide` are each one shared source, including the installation section,
    and are compared byte for byte.

    The two documents are also compared to each other: `quickstart` is supposed to be a much
    shorter amended version, and two commands printing the same text would be one command with two
    names.
    """

    case = harness.case("documentation")
    quickstart_python, quickstart_rust = harness.invoke(case, ("quickstart",))
    report.exact("documentation/quickstart", quickstart_python, quickstart_rust, expected_rc=0)
    guide_python, guide_rust = harness.invoke(case, ("userguide",))
    report.exact("documentation/userguide", guide_python, guide_rust, expected_rc=0)
    report.require(
        "documentation/userguide-is-the-reference",
        "## Installation" in guide_python.stdout and len(guide_python.stdout) > 5000,
        f"the user guide lost its body or its installation section: {len(guide_python.stdout)} "
        "characters",
    )
    report.require(
        "documentation/quickstart-is-shorter",
        quickstart_python.stdout != guide_python.stdout
        and 2 * quickstart_python.stdout.count("\n") < guide_python.stdout.count("\n"),
        "quickstart is supposed to be a much shorter amended version of the guide: "
        f"{quickstart_python.stdout.count(chr(10))} lines against "
        f"{guide_python.stdout.count(chr(10))}",
    )

    # A configuration file too broken to load must not be able to withhold the documentation.
    broken = harness.case("documentation-broken-config", {".herdr-run.yaml": "allow: [\n"})
    python, rust = harness.invoke(broken, ("quickstart",))
    report.exact("documentation/quickstart-despite-broken-config", python, rust, expected_rc=0)
    python, rust = harness.invoke(broken, ("userguide",))
    report.require(
        "documentation/userguide-despite-broken-config",
        python == guide_python and rust == guide_rust,
        "a configuration file too broken to load withheld the user guide: "
        f"python={_describe(python)} rust={_describe(rust)}",
    )

    help_python, _ = harness.invoke(case, ("--help",))
    report.require(
        "documentation/help-distinguishes-them",
        "quickstart" in help_python.stdout
        and "userguide" in help_python.stdout
        and "Two documentation commands" in help_python.stdout,
        f"the top-level help does not say what each document is for: {_describe(help_python)}",
    )


def _status(harness: Harness, report: Report) -> None:
    """`status` must read the same and say the same, and must still be read-only in both.

    Against the fake Herdr behind the account-home shim; its calls are compared too, and must only
    read.
    """
    if harness.account_unavailable(report, "status/all"):
        return
    case = harness.case("status", {".herdr-run.yaml": _STATUS_CONFIG})
    python, rust = harness.invoke(case, ("--agent", "fixture-agent", "status"), account=True)
    report.exact("status/text", python, rust, expected_rc=0)
    for fragment in (
        "    file          <ROOT>/.herdr-run.yaml\n",
        "    project root  <ROOT>\n",
        "    spool dir     .herdr-run\n",
        "    allow         git, gh\n",
        "    prefixes      with-proxy\n",
        "    agent         fixture-agent\n",
        f"    workspace     {_REAP_WORKSPACE}\n",
        "    tab label     fixture-agent",
        "\nNothing was changed: status only reads.\n",
    ):
        report.require(
            f"status/text-says/{fragment.split()[0]}",
            fragment in python.stdout,
            f"status omitted {fragment!r}: {_describe(python)}",
        )

    json_python, json_rust = harness.invoke(
        case, ("--json", "--agent", "fixture-agent", "status"), account=True
    )
    report.exact("status/json", json_python, json_rust, expected_rc=0)
    report.require(
        "status/json-shape",
        '"agent": "fixture-agent"' in json_python.stdout
        and '"allow_any_program": false' in json_python.stdout
        and '"config_file": "<ROOT>/.herdr-run.yaml"' in json_python.stdout
        and '"label": "fixture-agent"' in json_python.stdout
        and f'"workspace": "{_REAP_WORKSPACE}"' in json_python.stdout,
        f"status JSON changed shape: {_describe(json_python)}",
    )

    # Nothing status did may have created state on disk either.
    for edition, root in (("python", case.python_root), ("rust", case.rust_root)):
        report.require(
            f"status/creates-nothing/{edition}",
            not (root / ".herdr-run").exists(),
            "status created spool state in a directory it was only supposed to describe",
        )
    harness.account_calls(report, "status", case)

    wildcard = harness.case("status-allow-any", {".herdr-run.yaml": _STATUS_ANY_CONFIG})
    python, rust = harness.invoke(wildcard, ("--agent", "fixture-agent", "status"), account=True)
    report.exact("status/allow-everything", python, rust, expected_rc=0)
    report.require(
        "status/allow-everything-shape",
        '    allow         any program ("*")\n' in python.stdout
        and "    prefixes      (none)\n" in python.stdout,
        f"the allow-everything mode was not reported as such: {_describe(python)}",
    )
    harness.account_calls(report, "status/allow-everything", wildcard)

    # Values that need quoting, and a timeout that is not an integer, read the same in both.
    quoted = harness.case(
        "status-quoted",
        {
            ".herdr-run.yaml": (
                f"workspace: {_REAP_WORKSPACE}\n"
                "allow: [git]\n"
                "timeout_seconds: 0.25\n"
                "ready_timeout_seconds: 1e-3\n"
                "cwd: /srv/it's\n"
                'prompt_tail: "\\u00bb "\n'
            )
        },
    )
    for label, args in (
        ("text", ("--agent", "o'neil", "status")),
        ("json", ("--json", "--agent", "o'neil", "status")),
    ):
        python, rust = harness.invoke(quoted, args, account=True)
        report.exact(f"status/quoted/{label}", python, rust, expected_rc=0)
    harness.account_calls(report, "status/quoted", quoted)


def _init(harness: Harness, report: Report) -> None:
    """`init` must write the same bytes in both editions, and both must then read them the same.

    Comparing the generated `.herdr-run.yaml` byte for byte is what lets the user guide point at
    that file instead of restating it: the file is the reference, so a reference that differed
    between editions would be two references.
    """

    # `--config PATH` reads as "write PATH" here and would in fact write ./.herdr-run.yaml, so it
    # is refused. The refusal has to be total: nothing written, in either edition.
    refused = harness.case("init-config-refused")
    python, rust = harness.invoke(refused, ("--config", "elsewhere.yaml", "init"))
    report.exact("init/config-is-refused", python, rust, expected_rc=2)
    report.require(
        "init/config-refusal-writes-nothing",
        not (refused.python_root / ".herdr-run.yaml").exists()
        and not (refused.rust_root / ".herdr-run.yaml").exists(),
        "a refused '--config PATH init' still wrote a configuration file",
    )

    # `--agent NAME init` is refused for the same reason and must be just as total: `init` names no
    # tab, so the name has nowhere to go, and a refusal that still wrote the file would be worse
    # than accepting the flag.
    refused = harness.case("init-agent-refused")
    python, rust = harness.invoke(refused, ("--agent", "a", "init"))
    report.exact("init/agent-is-refused", python, rust, expected_rc=2)
    report.require(
        "init/agent-refusal-writes-nothing",
        not (refused.python_root / ".herdr-run.yaml").exists()
        and not (refused.rust_root / ".herdr-run.yaml").exists(),
        "a refused '--agent NAME init' still wrote a configuration file",
    )

    case = harness.case("init")
    python, rust = harness.invoke(case, ("init",))
    report.exact("init/first-write", python, rust, expected_rc=0)

    python_file = _read_normalized(case.python_root / ".herdr-run.yaml", case.python_root)
    rust_file = _read_normalized(case.rust_root / ".herdr-run.yaml", case.rust_root)
    report.exact("init/template-bytes", python_file, rust_file, expected_rc=0)
    report.exact(
        "init/template-mode",
        _modes(case.python_root, (".herdr-run.yaml",)),
        _modes(case.rust_root, (".herdr-run.yaml",)),
        expected_rc=0,
    )
    for marker in (
        "a human-only knob",
        "DO NOT LET AN AGENT EDIT THIS SECTION",
        "worktrees/slotNN/",
        "ALLOW-EVERYTHING MODE",
        'allow: ["*"]',
    ):
        report.require(
            f"init/template-says/{marker[:24]}",
            marker in python_file.stdout,
            f"the generated configuration never says {marker!r}",
        )

    # Every knob has to be there, or the guide cannot point at this file instead of listing them.
    missing = [key for key in _CONFIG_KEYS if f"\n{key}:" not in python_file.stdout]
    report.require(
        "init/template-covers-every-key",
        not missing,
        f"the generated configuration never sets {missing}",
    )

    resolved_python, resolved_rust = harness.invoke(
        case, ("--agent", "fixture-agent", "config")
    )
    report.exact("init/config-after-init", resolved_python, resolved_rust, expected_rc=0)

    python, rust = harness.invoke(case, ("init",))
    report.exact("init/refuses-to-clobber", python, rust, expected_rc=78)
    report.require(
        "init/refuses-to-clobber-names-force",
        "--force" in python.stderr and python.stdout == "",
        f"a refused init must name --force on stderr and print nothing: {_describe(python)}",
    )

    python, rust = harness.invoke(case, ("init", "--force"))
    report.exact("init/force-overwrites", python, rust, expected_rc=0)


#: Configurations earlier releases wrote, byte for byte; ``policy_cases.json`` describes each one.
_EARLIER_CONFIGS = REPO_ROOT / "rs" / "herdr-run" / "testdata" / "earlier_configs"

#: The one ``deny_anywhere`` line every earlier release wrote, which this release extends.
_EARLIER_DENY_ANYWHERE = 'deny_anywhere: ["--upload-pack", "--receive-pack"]\n'


def _upgrades(harness: Harness, report: Report) -> None:
    """A configuration an earlier release wrote reads, warns, and upgrades the same in both.

    A configured deny list replaces the built-in one, so every file an earlier ``init`` wrote, and
    every copy of an earlier user guide's example, leaves out ``--exec``. Both editions must warn
    about it in the same words, apply git's built-in value options regardless, and rewrite the file
    with ``init --upgrade`` to the same bytes.
    """
    fixtures = sorted(_EARLIER_CONFIGS.iterdir())
    report.require(
        "upgrade/fixtures-present",
        len(fixtures) >= 6,
        f"expected the earlier configurations under {_EARLIER_CONFIGS}, found {fixtures}",
    )
    for fixture in fixtures:
        label = fixture.stem
        case = harness.case(
            f"upgrade-{label}", {".herdr-run.yaml": fixture.read_text(encoding="utf-8")}
        )
        for name, command, expected in (
            ("attr-source-daemon", "git --attr-source HEAD daemon", 77),
            ("shallow-file-daemon", "git --shallow-file x daemon", 77),
            ("push-exec", "git push --exec=/tmp/evil origin main", 0),
        ):
            python, rust = harness.invoke(case, ("check", command))
            report.exact(f"upgrade/{label}/before/{name}", python, rust, expected_rc=expected)
        report.require(
            f"upgrade/{label}/before/warns",
            python.stderr.startswith(
                "herdr-run: WARNING: <ROOT>/.herdr-run.yaml leaves out built-in deny entries, so "
                'they are not enforced: deny_anywhere "--exec". To adopt them, run '
                "'herdr-run init --upgrade' in <ROOT> or add them by hand.\n"
            ),
            f"an earlier configuration did not draw the warning: {_describe(python)}",
        )
        python, rust = harness.invoke(case, ("--agent", "fixture-agent", "config"))
        report.exact(f"upgrade/{label}/before/config", python, rust, expected_rc=0)

        python, rust = harness.invoke(case, ("init", "--upgrade"))
        report.exact(f"upgrade/{label}/upgrade", python, rust, expected_rc=0)
        report.exact(
            f"upgrade/{label}/upgraded-bytes",
            _read_normalized(case.python_root / ".herdr-run.yaml", case.python_root),
            _read_normalized(case.rust_root / ".herdr-run.yaml", case.rust_root),
            expected_rc=0,
        )
        report.exact(
            f"upgrade/{label}/upgraded-mode",
            _modes(case.python_root, (".herdr-run.yaml",)),
            _modes(case.rust_root, (".herdr-run.yaml",)),
            expected_rc=0,
        )
        python, rust = harness.invoke(case, ("check", "git push --exec=/tmp/evil origin main"))
        report.exact(f"upgrade/{label}/after/push-exec", python, rust, expected_rc=77)
        report.require(
            f"upgrade/{label}/after/no-warning",
            "WARNING" not in python.stderr,
            f"an upgraded configuration still drew a warning: {_describe(python)}",
        )
        python, rust = harness.invoke(case, ("--json", "init", "--upgrade"))
        report.exact(f"upgrade/{label}/again-json", python, rust, expected_rc=0)

    # A list somebody edited is left alone, and what it lacks is listed instead.
    edited = (_EARLIER_CONFIGS / "0.3.0-init-93b44b35.yaml").read_text(encoding="utf-8")
    edited = edited.replace(
        _EARLIER_DENY_ANYWHERE, 'deny_anywhere: ["--receive-pack", "--upload-pack"]\n'
    ).replace('git: ["-c", "--config-env", "--exec-path", "--namespace"]', 'git: ["-c"]')
    for label, json_flag in (("text", ()), ("json", ("--json",))):
        case = harness.case(f"upgrade-edited-{label}", {".herdr-run.yaml": edited})
        python, rust = harness.invoke(case, (*json_flag, "init", "--upgrade"))
        report.exact(f"upgrade/edited/{label}", python, rust, expected_rc=0)
        report.exact(
            f"upgrade/edited/{label}/bytes",
            _read_normalized(case.python_root / ".herdr-run.yaml", case.python_root),
            _read_normalized(case.rust_root / ".herdr-run.yaml", case.rust_root),
            expected_rc=0,
        )

    # What status says about it, in a workspace no real session carries.
    narrowed = f"workspace: {_REAP_WORKSPACE}\n{_EARLIER_DENY_ANYWHERE}deny_global:\n  git: [-c]\n"
    case = harness.case("upgrade-status", {".herdr-run.yaml": narrowed})
    for label, args in (
        ("text", ("--agent", "fixture-agent", "status")),
        ("json", ("--json", "--agent", "fixture-agent", "status")),
    ):
        python, rust = harness.invoke(case, args)
        report.exact(f"upgrade/status/{label}", python, rust, expected_rc=0)
    report.require(
        "upgrade/status/names-the-left-out-entries",
        '"setting": "deny_global.git"' in python.stdout and '"--exec"' in python.stdout,
        f"status JSON did not list the left-out entries: {_describe(python)}",
    )

    # A file `init --upgrade` cannot work on gets a warning that offers only a hand edit.
    case = harness.case("upgrade-other-name", {"conf/settings.yaml": _EARLIER_DENY_ANYWHERE})
    python, rust = harness.invoke(case, ("--config", "conf/settings.yaml", "check", "git status"))
    report.exact("upgrade/other-name-warning", python, rust, expected_rc=0)

    # git's value options are git's syntax: a configuration can add to them, never take one away.
    for label, config, command, expected in (
        ("seven-git-entries", "value_options:\n  git: [-C]\n", "git --attr-source HEAD daemon", 77),
        ("emptied", "value_options: {}\n", "git -C /tmp daemon", 77),
        ("added", "value_options:\n  git: [--frobnicate]\n", "git --frobnicate daemon log", 0),
    ):
        case = harness.case(f"upgrade-value-options-{label}", {".herdr-run.yaml": config})
        python, rust = harness.invoke(case, ("check", command))
        report.exact(f"upgrade/value-options/{label}", python, rust, expected_rc=expected)
        python, rust = harness.invoke(case, ("config",))
        report.exact(f"upgrade/value-options/{label}/config", python, rust, expected_rc=0)

    # The ways `init --upgrade` refuses, each the same in both and each writing nothing.
    case = harness.case("upgrade-no-file")
    python, rust = harness.invoke(case, ("init", "--upgrade"))
    report.exact("upgrade/no-file", python, rust, expected_rc=78)
    report.require(
        "upgrade/no-file-writes-nothing",
        not (case.python_root / ".herdr-run.yaml").exists()
        and not (case.rust_root / ".herdr-run.yaml").exists(),
        "a refused 'init --upgrade' wrote a configuration file",
    )
    case = harness.case("upgrade-yml", {".herdr-run.yml": _EARLIER_DENY_ANYWHERE})
    python, rust = harness.invoke(case, ("init", "--upgrade"))
    report.exact("upgrade/yml", python, rust, expected_rc=0)
    report.exact(
        "upgrade/yml-bytes",
        _read_normalized(case.python_root / ".herdr-run.yml", case.python_root),
        _read_normalized(case.rust_root / ".herdr-run.yml", case.rust_root),
        expected_rc=0,
    )
    refusals: tuple[tuple[str, tuple[str, ...], int], ...] = (
        ("force-and-upgrade", ("init", "--force", "--upgrade"), 2),
        ("config-and-upgrade", ("--config", "x.yaml", "init", "--upgrade"), 2),
        ("agent-and-upgrade", ("--agent", "a", "init", "--upgrade"), 2),
        ("misplaced-upgrade", ("--upgrade", "init"), 2),
    )
    for label, refused_args, expected in refusals:
        case = harness.case(f"upgrade-refused-{label}", {".herdr-run.yaml": _EARLIER_DENY_ANYWHERE})
        python, rust = harness.invoke(case, refused_args)
        report.exact(f"upgrade/refused/{label}", python, rust, expected_rc=expected)
        report.exact(
            f"upgrade/refused/{label}/untouched",
            _read_normalized(case.python_root / ".herdr-run.yaml", case.python_root),
            _read_normalized(case.rust_root / ".herdr-run.yaml", case.rust_root),
            expected_rc=0,
        )
    for label, text in (
        ("does-not-load", _EARLIER_DENY_ANYWHERE + "allowlist: [git]\n"),
        ("reads-differently", "deny_global: {git: [-c],\n" + _EARLIER_DENY_ANYWHERE + "}\n"),
    ):
        case = harness.case(f"upgrade-refused-{label}", {".herdr-run.yaml": text})
        python, rust = harness.invoke(case, ("init", "--upgrade"))
        report.exact(f"upgrade/refused/{label}", python, rust, expected_rc=78)
        report.require(
            f"upgrade/refused/{label}/untouched",
            (case.python_root / ".herdr-run.yaml").read_text(encoding="utf-8") == text
            and (case.rust_root / ".herdr-run.yaml").read_text(encoding="utf-8") == text,
            "a refused 'init --upgrade' still changed the file",
        )


def _deleted_cwd(harness: Harness, report: Report) -> None:
    """A deleted working directory reaches both tools, and both say the same about it."""

    case = harness.case("deleted-cwd", {".herdr-run.yaml": "allow: [git]\n"})
    for label, args, allowed in (
        ("version", ("--version",), frozenset({0})),
        ("config", ("config",), frozenset({78})),
        ("check", ("check", "git status"), frozenset({0, 78})),
        ("dry-run", ("--agent", "a", "run", "--dry-run", "git status"), frozenset({0, 78})),
        ("init", ("init",), frozenset({78})),
        ("quickstart", ("quickstart",), frozenset({0})),
    ):
        python, rust = harness.invoke(case, args, cwd="gone", mode="deleted-cwd")
        report.same(f"deleted-cwd/{label}", python, rust, allowed=allowed)


def _strip_launcher_lines(outcome: Outcome) -> tuple[Outcome, int]:
    stripped, count = _LAUNCHER_LINE.subn("", outcome.stderr)
    return Outcome(outcome.returncode, outcome.stdout, stripped), count


def _launchers(harness: Harness, report: Report) -> None:
    """Both engines are reachable through the documented launcher, and the TOOL agrees there too.

    The launcher's own lines are stripped before comparing. They differ by design (each names its
    engine, and the Rust path reports its cache verdict), and the launcher's other behaviour --
    under a deleted working directory, without ``HOME``, under a locale bash cannot load, and with
    a ``python3`` on ``PATH`` that is a wrapper rather than the interpreter (which may refuse an
    argument that is not UTF-8 before herdr-run starts) -- is shared by every tool and documented
    in ``cross/README.md`` as launcher behaviour. Bytes that are not UTF-8 are compared on the
    editions themselves, in ``_non_utf8``.
    """

    if not LAUNCHER.exists():
        report.notes.append(f"launcher cases were not run: {LAUNCHER} is missing")
        return
    case = harness.case("launchers", {".herdr-run.yaml": "allow: [git]\nprefixes: []\n"})
    command = (str(LAUNCHER),)
    for label, args, allowed in (
        ("version", ("--version",), frozenset({0})),
        ("help", ("--help",), frozenset({0})),
        ("surface", ("--dump-surface",), frozenset({0})),
        ("refusal", ("check", "curl\u202ex"), frozenset({77})),
        ("usage", ("--frobnicate",), frozenset({2})),
        ("dry-run", ("--agent", "a", "run", "--dry-run", "git status"), frozenset({0})),
    ):
        python, rust = harness.invoke_commands(
            case,
            command,
            command,
            args,
            rust_env={"DAGRUN_ENGINE": "rust"},
        )
        python_body, python_lines = _strip_launcher_lines(python)
        rust_body, rust_lines = _strip_launcher_lines(rust)
        report.same(f"launcher/{label}", python_body, rust_body, allowed=allowed)
        report.require(
            f"launcher/{label}/announces-the-engine",
            python_lines >= 1 and rust_lines >= 1,
            "each launch must name its engine: "
            f"python={_describe(python)} rust={_describe(rust)}",
        )


#: A stand-in ``git`` placed first on PATH. herdr-run asks git whether the spool is ignored before
#: a dry run, so this is a process each edition starts, and it records what it was handed.
_PROBE_GIT = """#!/bin/sh
{
  printf 'argv:'; for argument in "$@"; do printf ' [%s]' "$argument"; done; echo
  echo 'environ:'
  tr '\\0' '\\n' < "/proc/$$/environ"
  echo 'end of environ'
  grep -E '^Sig(Ign|Blk):' "/proc/$$/status"
  for descriptor in 0 1 2 3 4 5 6 7 8 9; do
    if [ -e "/proc/$$/fd/$descriptor" ]; then
      echo "fd $descriptor -> $(readlink "/proc/$$/fd/$descriptor" | sed 's/:\\[[0-9]*\\]/:[N]/')"
    fi
  done
} > "$HERDR_PROBE_DUMP"
exit 1
"""

#: A stand-in ``git`` that exits at once while a descendant keeps its stdout and stderr open.
_HOLDING_GIT = """#!/bin/sh
/usr/bin/sleep 60 &
exit 1
"""


#: A stand-in ``git`` with no ``#!`` line, which the kernel refuses to execute (ENOEXEC). The C
#: library's ``execvp`` would run it with ``/bin/sh`` instead; neither edition may.
_NO_SHEBANG_GIT = """touch "$HERDR_PROBE_DUMP"
exit 1
"""


def _exec_format(harness: Harness, report: Report) -> None:
    """A file the kernel cannot execute is an error in both editions, never a shell script.

    ``git check-ignore`` is the process a dry run starts. Before, the native edition's children
    were started through ``execvp``, which retried a script without ``#!`` under ``/bin/sh``: the
    stand-in ran, said "not ignored", and only that edition warned about the spool.
    """
    git = shutil.which("git")
    if git is None:
        report.notes.append("exec-format: not compared, git is not installed")
        return
    stand_in = _fake_bin(harness, "no-shebang-bin", _NO_SHEBANG_GIT)
    path = f"{stand_in}{os.pathsep}{os.environ.get('PATH', '/usr/bin:/bin')}"

    def work_tree(root: Path) -> None:
        subprocess.run(
            [git, "init", "-q", str(root)],
            check=True,
            capture_output=True,
            env={**os.environ, "HOME": str(root / ".home"), "GIT_CONFIG_NOSYSTEM": "1"},
        )

    case = harness.case("exec-format", {".herdr-run.yaml": "allow: [git]\n"}, setup=work_tree)
    python, rust = harness.invoke(
        case,
        ("--agent", "a", "run", "--dry-run", "git status"),
        extra_env={"PATH": path, "HERDR_PROBE_DUMP": "<ROOT>/ran.txt"},
    )
    report.exact("exec-format/check-ignore", python, rust, expected_rc=0)
    report.require(
        "exec-format/never-run-by-a-shell",
        not (case.python_root / "ran.txt").exists() and not (case.rust_root / "ran.txt").exists(),
        "a stand-in without #! was executed: "
        f"python={(case.python_root / 'ran.txt').exists()} "
        f"rust={(case.rust_root / 'ran.txt').exists()}",
    )


def _fake_bin(harness: Harness, name: str, script: str) -> Path:
    directory = harness.root / name
    directory.mkdir(exist_ok=True)
    git = directory / "git"
    git.write_text(script, encoding="utf-8")
    git.chmod(0o755)
    return directory


def _interpreter_coerces_c_locale(executable: str) -> bool:
    """Whether ``executable`` is a Python that adds ``LC_CTYPE`` under the C locale (PEP 538)."""
    try:
        completed = subprocess.run(
            [executable, "-c", "import os; print(os.environ.get('LC_CTYPE', ''))"],
            env={"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LANG": "C"},
            capture_output=True,
            text=True,
            check=False,
            timeout=TIMEOUT_SECONDS,
        )
    except (OSError, subprocess.SubprocessError):
        return False
    return completed.returncode == 0 and completed.stdout.strip() != ""


def _child_processes(harness: Harness, report: Report) -> None:
    """What each edition hands to a process it starts is the same: environment, descriptors, signals.

    The Python interpreter changes its own process before any herdr-run code runs: PEP 538 adds
    ``LC_CTYPE=C.UTF-8`` to the environment under the C locale, a closed descriptor reopened by
    ``os.open`` is close-on-exec, ``subprocess`` closes every inherited descriptor above 2, and
    SIGINT has a handler. The native runtime does none of that. Both editions now hand a child the
    caller's environment by the shared rule, ``/dev/null`` for a closed standard descriptor, no
    inherited descriptor above 2, SIGINT as the caller left it, and SIGPIPE and SIGXFSZ at their
    defaults.

    The stand-in reads its environment from ``/proc``, entry by entry and in order, as the kernel
    holds it, so a repeated name or an entry without ``=`` would show. The ``raw-environment``
    cases hand both editions such entries (:data:`_RAW_EXTRAS`), once under a locale that is not
    UTF-8, where the Python edition restarts itself in UTF-8 mode first, and require exactly what
    the rule makes of them.
    """

    probe = _fake_bin(harness, "probe-bin", _PROBE_GIT)
    path = f"{probe}{os.pathsep}{os.environ.get('PATH', '/usr/bin:/bin')}"
    config = {".herdr-run.yaml": "allow: [git]\nspool_dir: .spool\n"}
    for label, environment, mode in (
        ("plain", {}, "capture"),
        ("c-locale", {"LC_ALL": None, "LANG": "C", "LC_CTYPE": None}, "capture"),
        ("posix-locale", {"LC_ALL": None, "LANG": None, "LC_CTYPE": "POSIX"}, "capture"),
        ("latin-1-locale", {"LC_ALL": "en_US", "LANG": "en_US"}, "capture"),
        ("non-utf8-value", {"HERDR_RUN_TEST_BYTES": "caf\udce9"}, "capture"),
        ("inherited-descriptors", {}, "inherited-descriptors"),
        ("sigint-ignored", {}, "sigint-ignored"),
        ("raw-environment", {}, "raw-environment"),
        ("raw-environment-latin-1-locale", {"LC_ALL": "en_US", "LANG": "en_US"}, "raw-environment"),
    ):
        case = harness.case(f"child-{label}", config)
        extra_env = {**environment, "PATH": path, "HERDR_PROBE_DUMP": "<ROOT>/probe.txt"}
        python, rust = harness.invoke(
            case,
            ("--agent", "a", "run", "--dry-run", "git status"),
            extra_env=extra_env,
            mode=mode,
        )
        report.exact(f"child/{label}", python, rust, expected_rc=0)
        python_probe = _read_normalized(case.python_root / "probe.txt", case.python_root)
        rust_probe = _read_normalized(case.rust_root / "probe.txt", case.rust_root)
        report.exact(f"child/{label}/handed-over", python_probe, rust_probe, expected_rc=0)
        report.require(
            f"child/{label}/probe-ran",
            python_probe.stdout.startswith("argv: [-C]"),
            f"the stand-in git never ran: {_describe(python_probe)}",
        )
        if label == "c-locale":
            report.require(
                "child/c-locale/no-coerced-lc-ctype",
                "\nLC_CTYPE=" not in python_probe.stdout,
                f"PEP 538's LC_CTYPE reached a child: {python_probe.stdout!r}",
            )
            if not _interpreter_coerces_c_locale(harness.python_command[0]):
                report.notes.append(
                    "child/c-locale: this Python interpreter does not coerce the C locale "
                    "(PEP 538), so the case cannot observe a coerced LC_CTYPE here; a stock "
                    "CPython does, and the unit tests simulate it"
                )
        if label == "inherited-descriptors":
            report.require(
                "child/inherited-descriptors/shape",
                "fd 0 -> /dev/null\n" in python_probe.stdout
                and "fd 7 ->" not in python_probe.stdout,
                f"a child must find /dev/null at 0 and nothing at 7: {python_probe.stdout!r}",
            )
        if label == "sigint-ignored":
            mask = re.search(r"SigIgn:\s*([0-9a-f]+)", python_probe.stdout)
            report.require(
                "child/sigint-ignored/still-ignored",
                mask is not None and int(mask.group(1), 16) & (1 << (signal.SIGINT - 1)) != 0,
                f"a SIGINT the caller ignored must stay ignored: {python_probe.stdout!r}",
            )
        if mode == "raw-environment":
            entries = _raw_entries(harness.environment_for(case.python_root, extra_env))
            expected = [
                _normalize_text(entry.decode("utf-8", errors="surrogateescape"), case.python_root)
                for entry in _by_the_rule(entries)
            ]
            # Compared as text: a value may itself hold a line break.
            wanted = "".join(f"{entry}\n" for entry in expected)
            handed = python_probe.stdout.partition("environ:\n")[2].partition("end of environ\n")[0]
            report.require(
                f"child/{label}/by-the-rule",
                handed == wanted,
                _first_difference(wanted, handed),
            )


def _sigchld_handed_over(harness: Harness, report: Report) -> None:
    """A caller's ignored SIGCHLD is reset before either edition starts a process.

    The shell stand-in above cannot show it: a shell installs its own SIGCHLD handler as it
    starts. This stand-in is the Python interpreter, which leaves SIGCHLD as it found it, and it
    records the disposition it was handed before anything else runs.
    """
    directory = harness.root / "sigchld-bin"
    directory.mkdir(exist_ok=True)
    git = directory / "git"
    git.write_text(
        f"#!{sys.executable} -IS\n"
        "import os\n"
        "status = open('/proc/self/status', encoding='utf-8').read().splitlines()\n"
        "line = next(item for item in status if item.startswith('SigIgn:'))\n"
        "open(os.environ['HERDR_PROBE_DUMP'], 'w', encoding='utf-8').write(line + '\\n')\n"
        "raise SystemExit(1)\n",
        encoding="utf-8",
    )
    git.chmod(0o755)
    path = f"{directory}{os.pathsep}{os.environ.get('PATH', '/usr/bin:/bin')}"
    case = harness.case("sigchld-handed-over", {".herdr-run.yaml": "allow: [git]\n"})
    python, rust = harness.invoke(
        case,
        ("--agent", "a", "run", "--dry-run", "git status"),
        extra_env={"PATH": path, "HERDR_PROBE_DUMP": "<ROOT>/probe.txt"},
        mode="sigchld-ignored",
    )
    report.exact("child/sigchld-ignored", python, rust, expected_rc=0)
    python_probe = _read_normalized(case.python_root / "probe.txt", case.python_root)
    rust_probe = _read_normalized(case.rust_root / "probe.txt", case.rust_root)
    report.exact("child/sigchld-ignored/handed-over", python_probe, rust_probe, expected_rc=0)
    mask = re.search(r"SigIgn:\s*([0-9a-f]+)", python_probe.stdout)
    report.require(
        "child/sigchld-ignored/reset",
        mask is not None and int(mask.group(1), 16) & (1 << (signal.SIGCHLD - 1)) == 0,
        f"an inherited ignored SIGCHLD must be reset before a child starts: {python_probe.stdout!r}",
    )


def _held_pipe(harness: Harness, report: Report) -> None:
    """A child that exits while a descendant holds its output is bounded like one still running.

    herdr-run gives ``git check-ignore`` ten seconds. Both editions must stop waiting then, kill the
    process group, and carry on without the answer -- not wait the descendant's full minute.
    """

    holding = _fake_bin(harness, "holding-bin", _HOLDING_GIT)
    path = f"{holding}{os.pathsep}{os.environ.get('PATH', '/usr/bin:/bin')}"
    case = harness.case("held-pipe", {".herdr-run.yaml": "allow: [git]\n"})
    started = time.monotonic()
    python, rust = harness.invoke(
        case,
        ("--agent", "a", "run", "--dry-run", "git status"),
        extra_env={"PATH": path},
    )
    elapsed = time.monotonic() - started
    report.exact("held-pipe/bounded", python, rust, expected_rc=0)
    report.require(
        "held-pipe/within-the-bound",
        elapsed < 2 * TIMEOUT_SECONDS,
        f"waiting on a held pipe took {elapsed:.1f}s for the two editions together",
    )


def _signals_and_limits(harness: Harness, report: Report) -> None:
    """An ignored SIGINT stays ignored, and a file-size limit is an error, not a kill."""

    case = harness.case("signals")
    python, rust = harness.invoke(case, ("userguide",), mode="sigint-ignored-while-writing")
    report.exact("signals/sigint-ignored-while-writing", python, rust, expected_rc=0)
    python, rust = harness.invoke(case, ("userguide",), mode="file-size-limit")
    report.exact("signals/file-size-limit", python, rust, expected_rc=1)
    report.require(
        "signals/file-size-limit-is-reported",
        python.stderr == "herdr-run: cannot write stdout: File too large (os error 27)\n",
        f"a write past RLIMIT_FSIZE must fail with EFBIG, reported: {_describe(python)}",
    )


#: A directory name holding an escape sequence, a bidirectional override, and a no-break space.
_UNSAFE_NAME = "a\x1b[31mb\u202ec\u00a0d"


def _unsafe_paths(harness: Harness, report: Report) -> None:
    """A path or a name that holds a terminal control or an invisible character is shown quoted.

    Covers the status lines, the configuration messages that start with the file's path, the path
    ``init`` reports, and the project path in the un-ignored spool warning. ``status`` asks Herdr
    whether a server runs, so every case runs against the fake Herdr behind the account-home shim.
    """
    if harness.account_unavailable(report, "unsafe-paths/all"):
        return
    git = shutil.which("git")

    def setup(root: Path) -> None:
        good = root / _UNSAFE_NAME
        good.mkdir()
        (good / ".herdr-run.yaml").write_text(
            f"workspace: {_REAP_WORKSPACE}\nallow: [git]\n", encoding="utf-8"
        )
        broken = root / (_UNSAFE_NAME + "-broken")
        broken.mkdir()
        (broken / ".herdr-run.yaml").write_text("allow: [\n", encoding="utf-8")
        invalid = root / (_UNSAFE_NAME + "-invalid")
        invalid.mkdir()
        (invalid / ".herdr-run.yaml").write_text("allow: []\n", encoding="utf-8")
        (root / (_UNSAFE_NAME + "-init")).mkdir()
        if git is not None:
            subprocess.run(
                [git, "init", "-q", str(good)],
                check=True,
                capture_output=True,
                env={**os.environ, "HOME": str(root / ".home"), "GIT_CONFIG_NOSYSTEM": "1"},
            )

    case = harness.case("unsafe-paths", setup=setup)
    for label, cwd, args, allowed in (
        ("status", _UNSAFE_NAME, ("--agent", "a", "status"), frozenset({0})),
        ("broken-yaml", _UNSAFE_NAME + "-broken", ("check", "git status"), frozenset({78})),
        ("invalid-config", _UNSAFE_NAME + "-invalid", ("check", "git status"), frozenset({78})),
        ("missing-explicit", ".", ("--config", _UNSAFE_NAME + "/no.yaml", "config"), frozenset({78})),
        ("init", _UNSAFE_NAME + "-init", ("init",), frozenset({0})),
        ("init-again", _UNSAFE_NAME + "-init", ("init",), frozenset({78})),
        ("spool-warning", _UNSAFE_NAME, ("--agent", "a", "run", "--dry-run", "git status"), frozenset({0})),
    ):
        python, rust = harness.invoke(
            case, args, cwd=cwd, extra_env={"GIT_CONFIG_NOSYSTEM": "1"}, account=True
        )
        report.same(f"unsafe-paths/{label}", python, rust, allowed=allowed)
        shown = python.stdout + python.stderr
        report.require(
            f"unsafe-paths/{label}/nothing-raw",
            "\x1b" not in shown and "\u202e" not in shown and "\u00a0" not in shown,
            f"a control or invisible character reached the terminal raw: {_describe(python)}",
        )
    harness.account_calls(report, "unsafe-paths", case)


#: Characters JSON output must escape although JSON itself does not require it: a C1 control
#: (CSI), a bidirectional override, a line separator, a zero-width space, and a variation selector.
_JSON_UNSAFE_NAME = "p\u009b2Jq\u202er\u2028s\u200bt\ufe0fu"


def _unsafe_json(harness: Harness, report: Report) -> None:
    """JSON output escapes what a terminal acts on or a reader cannot see, as ``\\uXXXX``.

    ``config`` printed the project path raw, so a directory named with U+009B or U+202E reached
    the terminal through it. Every JSON document both editions print or store escapes DEL, the C1
    controls, and every invisible character; a JSON reader still gets the same strings back.
    """

    def setup(root: Path) -> None:
        project = root / _JSON_UNSAFE_NAME
        project.mkdir()
        (project / ".herdr-run.yaml").write_text(
            "allow: [printf]\nprefixes: []\nspool_dir: .spool\n", encoding="utf-8"
        )

    case = harness.case("unsafe-json", setup=setup)
    raw = ("\u009b", "\u202e", "\u2028", "\u200b", "\ufe0f")
    for label, args in (
        ("config", ("--agent", "a", "config")),
        ("check-json", ("--json", "check", "printf '\u202e\u200b'")),
        ("dry-run-json", ("--json", "--agent", "a", "run", "--dry-run", "printf '\u2028'")),
    ):
        python, rust = harness.invoke(case, args, cwd=_JSON_UNSAFE_NAME)
        report.exact(f"unsafe-json/{label}", python, rust, expected_rc=0)
        report.require(
            f"unsafe-json/{label}/nothing-raw",
            not any(character in python.stdout + python.stderr for character in raw),
            f"an unsafe character reached the terminal raw through JSON: {_describe(python)}",
        )
        try:
            json.loads(python.stdout)
        except json.JSONDecodeError as error:
            report.require(f"unsafe-json/{label}/is-json", False, f"{error}: {_describe(python)}")
    python_audit = _read_normalized(
        case.python_root / _JSON_UNSAFE_NAME / ".spool" / "audit.jsonl", case.python_root
    )
    rust_audit = _read_normalized(
        case.rust_root / _JSON_UNSAFE_NAME / ".spool" / "audit.jsonl", case.rust_root
    )
    report.exact("unsafe-json/audit", python_audit, rust_audit, expected_rc=0)
    report.require(
        "unsafe-json/audit/nothing-raw",
        python_audit.stdout != "(absent)\n"
        and not any(character in python_audit.stdout for character in raw),
        f"the audit log holds an unsafe character raw: {python_audit.stdout!r}",
    )


# --- end to end, against a fake Herdr -----------------------------------------------------------
#
# Both editions find Herdr only in fixed places under the account home the account database
# reports; neither reads PATH or HOME for it, and neither has an option naming an executable. That
# is production behaviour and stays so. The group below changes the one input that decides it
# instead: a shim built here at test time and preloaded into both editions makes getpwuid_r report
# a temporary directory as the account home, and a fake Herdr (``herdr_e2e/fake_herdr.py``) is
# installed under it. Each edition then runs its production client -- resolution, the session
# cache, readiness, the spool, eviction, reap -- against the same fake, and the two are compared
# step by step, call by call, and file by file. The shim also refuses to start ``systemd-run`` or
# any ``herdr`` outside that home, and logs what it refused; a case fails if anything was refused.

_E2E_DIRECTORY = Path(__file__).resolve().parent / "herdr_e2e"
_E2E_HOME_VARIABLE = "HERDR_RUN_E2E_ACCOUNT_HOME"
_E2E_REFUSALS_VARIABLE = "HERDR_RUN_E2E_REFUSALS"
#: Fixed places that precede the account home when herdr-run looks for Herdr.
_E2E_SYSTEM_HERDR = ("/usr/local/bin/herdr", "/usr/bin/herdr")
#: The prompt the fake panes show, and the shell configuration it is inferred from.
_E2E_BASHRC = "PS1='\\u@\\h:\\w\\$ '\n"
#: The fake reports a busy pane's foreground as its shell's PID plus this, so the number is never
#: another pane's shell.
_E2E_FOREGROUND_OFFSET = 2_000_000_000
_E2E_TIMEOUT_SECONDS = 90

_E2E_TICKS = re.compile(r'(?P<prefix>"shell_start_ticks"\s*:\s*)\d+')
#: A process identity in a reap reason, ``(identity <boot id>:<start ticks>)``: the boot is the
#: host's, the start ticks are the stand-in shell's and so differ between the two runs.
_E2E_IDENTITY = re.compile(r"(?P<prefix>\(identity [0-9a-f-]+:)\d+")
#: When a run happened (``last run 2026-10-12T08:55:41Z``).
_E2E_ISO_TIME = re.compile(r"\b\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ\b")
#: Durations each edition measured itself; the rest of each message is compared as written.
_E2E_MEASURED = (
    re.compile(r"(?P<prefix>\bexit -?\d+ in )\d+(?:\.\d+)?s"),
    re.compile(r"(?P<prefix>\bis not ready after )\d+(?:\.\d+)?(?:e-?\d+)?s"),
)

#: Where the groups outside ``e2e/*`` that reach a Herdr keep each edition's account home: inside
#: its project root, so paths through it normalize to ``<ROOT>`` like the project's own.
_ACCOUNT_DIRECTORY = ".account"
_ACCOUNT_REFUSALS = ".account-refusals.log"

#: The ``herdr`` the fake is reached through. It reads what the call was handed before an
#: interpreter can change it -- a Python interpreter ignores SIGPIPE and SIGXFSZ for itself as it
#: starts, and a launcher in front of one may ignore more -- and runs ``fake_herdr.py`` beside it.
#: ``read`` and ``case`` are built in, so the ignored signals are those of the very process
#: herdr-run started; ``od`` reads that process's environment from ``/proc``, which is fixed at
#: ``exec``, entry by entry and in order.
_E2E_LAUNCHER = """\
#!/bin/sh
signals=
while IFS= read -r line; do
  case $line in
    SigIgn:*) signals=${{line#SigIgn:}}; break ;;
  esac
done < "/proc/$$/status"
PATH=/usr/bin:/bin
HERDR_E2E_HANDED_SIGNALS=$signals
HERDR_E2E_HANDED_ENVIRON=$(od -An -v -tx1 "/proc/$$/environ" | tr -d ' \\n')
export HERDR_E2E_HANDED_SIGNALS HERDR_E2E_HANDED_ENVIRON
exec {python} -I {fake} "$@"
"""

#: Herdr calls that only read; anything else would change the session.
_READ_ONLY_CALLS = frozenset(
    {
        "status",
        "workspace list",
        "tab list",
        "pane list",
        "pane get",
        "pane process-info",
        "pane read",
    }
)


def _install_fake_herdr(account: Path) -> Path:
    """Make ``account`` a home the fake Herdr is found in: the launcher, the fake, a ``.bashrc``."""
    bin_directory = account / ".local" / "bin"
    bin_directory.mkdir(parents=True, exist_ok=True)
    fake = bin_directory / "fake_herdr.py"
    shutil.copyfile(_E2E_DIRECTORY / "fake_herdr.py", fake)
    herdr = bin_directory / "herdr"
    herdr.write_text(
        _E2E_LAUNCHER.format(python=shlex.quote(sys.executable), fake=shlex.quote(str(fake))),
        encoding="utf-8",
    )
    herdr.chmod(0o755)
    (account / ".bashrc").write_text(_E2E_BASHRC, encoding="utf-8")
    return herdr


def _fake_state(account: Path) -> dict[str, object]:
    try:
        loaded = json.loads((account / "herdr-state" / "state.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}
    return cast(dict[str, object], loaded) if isinstance(loaded, dict) else {}


def _fake_calls(account: Path) -> list[dict[str, object]]:
    """Every call the fake in ``account`` recorded: ``argv``, ``environ``, ``ignored_signals``."""
    try:
        text = (account / "herdr-state" / "calls.jsonl").read_bytes().decode(
            "utf-8", errors="surrogateescape"
        )
    except OSError:
        return []
    return [cast(dict[str, object], json.loads(line)) for line in text.splitlines() if line]


def _handed_line(index: int, call: Mapping[str, object]) -> str:
    """One call's verb, ignored signals, and environment, for comparison between editions.

    Only the verb of the arguments: the full argument vectors are compared on their own, under
    the normalization that names the fake's PIDs, which must not touch environment values.
    """
    words = cast(list[str], call["argv"])[:2]
    return (
        f"{index} {json.dumps(words)} ignored={call.get('ignored_signals')} "
        f"environ={json.dumps(call.get('environ'))}\n"
    )


def _reads_only(argv: Sequence[str]) -> bool:
    words = " ".join(argv[:1]) if argv[:1] == ["status"] else " ".join(argv[:2])
    return words in _READ_ONLY_CALLS


def _first_difference(python: str, rust: str, limit: int = 40) -> str:
    """Where two long texts part, as a short unified diff."""
    import difflib

    lines = list(
        difflib.unified_diff(
            python.splitlines(), rust.splitlines(), "python", "rust", lineterm="", n=1
        )
    )
    more = f"\n... and {len(lines) - limit} more diff lines" if len(lines) > limit else ""
    return "\n".join(lines[:limit]) + more


def _stat_fields(pid: int) -> list[str] | None:
    """The fields of ``/proc/<pid>/stat`` from the third (state) on, or ``None``."""
    try:
        text = Path(f"/proc/{pid}/stat").read_bytes().decode("utf-8", errors="surrogateescape")
    except OSError:
        return None
    return text.rsplit(") ", 1)[-1].split()


def _start_ticks(pid: int) -> int | None:
    """When ``pid`` started, in clock ticks since boot (field 22 of ``/proc/<pid>/stat``)."""
    fields = _stat_fields(pid)
    try:
        return int(fields[19]) if fields is not None else None
    except (IndexError, ValueError):
        return None


def _kill_if_same(pid: int, start: object, *, group: bool) -> bool:
    """SIGKILL ``pid`` -- and with ``group`` its process group -- only while it is still the process
    that started at ``start``; return whether it was.

    A PID whose start time differs has been given to another process, and nothing is sent to it,
    or to a process group of that number. The process is held through a pidfd while it is checked
    and signalled, so the process checked is the process signalled; while it lives, its PID, and
    so the group named after it, cannot be given to anything else.
    """
    if not isinstance(start, int):
        return False
    handle: int | None
    try:
        handle = os.pidfd_open(pid)
    except OSError as error:
        if error.errno != errno.ENOSYS:
            return False
        handle = None
    try:
        if _start_ticks(pid) != start:
            return False
        if group:
            try:
                os.killpg(pid, signal.SIGKILL)
            except OSError:
                pass
        try:
            if handle is None:
                os.kill(pid, signal.SIGKILL)
            else:
                signal.pidfd_send_signal(handle, signal.SIGKILL)
        except OSError:
            pass
        return True
    finally:
        if handle is not None:
            os.close(handle)


def _case_group_members(groups: frozenset[int], account: Path) -> list[tuple[int, int]]:
    """``(pid, start ticks)`` of each process in one of process ``groups`` that is this case's.

    The fake starts every pane command with ``HOME`` set to the case's own account home, so a
    process in such a group whose environment holds exactly that is the case's own, whatever
    became of the process the group was named after.
    """
    marker = b"\0HOME=" + os.fsencode(str(account)) + b"\0"
    wanted = {str(group) for group in groups}
    members: list[tuple[int, int]] = []
    for name in os.listdir("/proc"):
        if not name.isdigit():
            continue
        fields = _stat_fields(int(name))
        if fields is None or len(fields) < 20 or fields[2] not in wanted:
            continue
        try:
            environment = Path(f"/proc/{name}/environ").read_bytes()
        except OSError:
            continue
        if marker in b"\0" + environment:
            members.append((int(name), int(fields[19])))
    return members


def _fake_processes(account: Path) -> list[tuple[int, object]]:
    """``(pid, start ticks)`` of every process the fake in ``account`` started."""
    recorded = _fake_state(account).get("processes", [])
    pairs: list[tuple[int, object]] = []
    for entry in cast(list[object], recorded):
        if isinstance(entry, list) and len(entry) == 2 and isinstance(entry[0], int):
            pairs.append((entry[0], entry[1]))
    return pairs


def _end_fake_processes(account: Path) -> None:
    """End every process the fake in ``account`` started, and nothing else.

    Each process (and the group it leads) is signalled only while it is still the process that was
    recorded. When it is gone, what may remain of its group is found by identity instead
    (:func:`_case_group_members`): its number is never signalled blind.
    """
    leaderless = frozenset(
        pid for pid, start in _fake_processes(account) if not _kill_if_same(pid, start, group=True)
    )
    if leaderless:
        for member, member_start in _case_group_members(leaderless, account):
            _kill_if_same(member, member_start, group=False)


@dataclass
class _E2EEdition:
    """One edition's side of one end-to-end case: its own account home and project."""

    name: str
    command: tuple[str, ...]
    root: Path
    shim: Path

    @property
    def account(self) -> Path:
        return self.root / "account"

    @property
    def project(self) -> Path:
        return self.root / "project"

    @property
    def state_directory(self) -> Path:
        return self.account / "herdr-state"

    @property
    def refusals(self) -> Path:
        return self.root / "refusals.log"

    def state(self) -> dict[str, object]:
        return _fake_state(self.account)

    def behave(self, behaviour: Mapping[str, object]) -> None:
        self.state_directory.mkdir(parents=True, exist_ok=True)
        (self.state_directory / "behaviour.json").write_text(
            json.dumps(behaviour), encoding="utf-8"
        )

    def environment(self) -> dict[str, str | None]:
        return {
            _E2E_HOME_VARIABLE: str(self.account),
            _E2E_REFUSALS_VARIABLE: str(self.refusals),
            "LD_PRELOAD": str(self.shim),
        }

    def herdr(self, *args: str) -> None:
        """Call the fake as a person at the terminal would, e.g. to close a tab by hand."""
        environment = {**os.environ, **cast(dict[str, str], self.environment())}
        subprocess.run(
            [str(self.account / ".local" / "bin" / "herdr"), *args],
            env=environment,
            capture_output=True,
            check=False,
            timeout=TIMEOUT_SECONDS,
        )

    def shells(self) -> dict[str, int]:
        shells = self.state().get("shells", {})
        return {str(k): int(v) for k, v in cast(dict[str, object], shells).items()}  # type: ignore[call-overload]

    def kill_shell(self, pane: str) -> None:
        """Kill a pane's stand-in shell, if it is still the process the fake started."""
        pid = self.shells().get(pane)
        if pid is not None and _kill_if_same(
            pid, dict(_fake_processes(self.account)).get(pid), group=False
        ):
            _wait_until_gone(pid)

    def end(self) -> None:
        """End every process the fake started for this case, and nothing else."""
        _end_fake_processes(self.account)


def _wait_until_gone(pid: int) -> None:
    """Wait (briefly) until ``pid`` is no longer a running process; a zombie counts as gone."""
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        try:
            stat_text = Path(f"/proc/{pid}/stat").read_text(encoding="utf-8")
        except OSError:
            return
        if stat_text.rsplit(") ", 1)[-1].startswith("Z"):
            return
        time.sleep(0.02)


@dataclass(frozen=True)
class _E2EStep:
    """One step of a case: an invocation of herdr-run, or something done between invocations."""

    args: tuple[str, ...] = ()
    rc: int | None = None
    action: Callable[[_E2EEdition], None] | None = None
    mode: str = "capture"
    label: str = ""


def _run_step(*args: str, rc: int | None = None, mode: str = "capture") -> _E2EStep:
    return _E2EStep(args=args, rc=rc, mode=mode)


def _do(label: str, action: Callable[[_E2EEdition], None]) -> _E2EStep:
    return _E2EStep(action=action, label=label)


def _e2e_normalize(
    text: str, edition: _E2EEdition, shells: Mapping[str, int] | None = None
) -> str:
    """Normalize what differs between two processes: roots, run IDs, times, and PIDs.

    The fake's stand-in shells are named by their pane (``<SHELL:w1:p1>``), so a PID in a message
    stays meaningful and the two editions' different numbers compare equal.
    """
    normalized = text
    for pane, pid in (edition.shells() if shells is None else shells).items():
        normalized = re.sub(rf"\b{pid + _E2E_FOREGROUND_OFFSET}\b", f"<FOREGROUND:{pane}>", normalized)
        normalized = re.sub(rf"\b{pid}\b", f"<SHELL:{pane}>", normalized)
    normalized = _normalize_text(normalized, edition.root)
    normalized = _E2E_TICKS.sub(r"\g<prefix><TICKS>", normalized)
    normalized = _E2E_IDENTITY.sub(r"\g<prefix><TICKS>", normalized)
    normalized = _E2E_ISO_TIME.sub("<ISO-TIME>", normalized)
    for pattern in _E2E_MEASURED:
        normalized = pattern.sub(r"\g<prefix><MEASURED>s", normalized)
    return normalized


def _e2e_tree(edition: _E2EEdition) -> Outcome:
    """Every file and directory the case left, with its mode and (normalized) content."""
    skipped = {
        edition.account / ".local" / "bin" / "herdr",
        edition.account / ".local" / "bin" / "fake_herdr.py",
        edition.state_directory / "state.json",
        edition.state_directory / "state.json.lock",
        edition.state_directory / "behaviour.json",
        edition.state_directory / "calls.jsonl",
        edition.refusals,
    }
    shells = edition.shells()
    lines: list[str] = []
    for directory, subdirectories, files in os.walk(edition.root):
        here = Path(directory)
        # Not herdr-run's: Git's own repositories, and the probe remote the harness creates.
        subdirectories[:] = sorted(
            name for name in subdirectories if name != ".git" and here / name != edition.root / "remote"
        )
        for name in sorted(files):
            path = here / name
            # The account home holds what the harness put there and what herdr-run keeps under
            # .local/state; anything else in it (a login shell's history, say) is the host's.
            if path in skipped or (here == edition.account and name != ".bashrc"):
                continue
            try:
                content = path.read_bytes().decode("utf-8", errors="surrogateescape")
            except OSError as error:
                content = f"<unreadable: {error.strerror}>"
            mode = stat.S_IMODE(path.lstat().st_mode)
            relative = _e2e_normalize(str(path.relative_to(edition.root)), edition, shells)
            lines.append(f"{relative} {mode:04o}\n{_e2e_normalize(content, edition, shells)}\n")
        for name in subdirectories:
            path = here / name
            mode = stat.S_IMODE(path.lstat().st_mode)
            relative = _e2e_normalize(str(path.relative_to(edition.root)), edition, shells)
            lines.append(f"{relative}/ {mode:04o}\n")
    return Outcome(0, "".join(sorted(lines)), "")


def _compile_account_home_shim(directory: Path) -> tuple[Path | None, str]:
    """Build the shim with the system C compiler; return it, or ``None`` and why not."""
    compiler = next(
        (found for name in ("cc", "gcc", "clang") if (found := shutil.which(name)) is not None),
        None,
    )
    if compiler is None:
        return None, "no C compiler (cc, gcc, or clang) is on PATH"
    target = directory / "account_home_shim.so"
    try:
        completed = subprocess.run(
            [
                compiler,
                "-shared",
                "-fPIC",
                "-O2",
                "-o",
                str(target),
                str(_E2E_DIRECTORY / "account_home_shim.c"),
                "-ldl",
            ],
            capture_output=True,
            text=True,
            check=False,
            timeout=120,
        )
    except (OSError, subprocess.SubprocessError) as error:
        return None, f"{compiler} could not be run: {error}"
    if completed.returncode != 0:
        # A compiler that exists but cannot build the shim is a failure, not a reason to skip.
        raise RuntimeError(f"{compiler} failed to build the shim: {completed.stderr.strip()}")
    return target, compiler


class _EndToEnd:
    """Run end-to-end cases for both editions and compare everything they observably did."""

    def __init__(self, harness: Harness, report: Report, shim: Path) -> None:
        self.harness = harness
        self.report = report
        self.shim = shim
        self.serial = 0

    def _edition(self, name: str, command: tuple[str, ...], case_root: Path) -> _E2EEdition:
        edition = _E2EEdition(name, command, case_root / name, self.shim)
        _install_fake_herdr(edition.account)
        edition.project.mkdir(parents=True)
        return edition

    def shim_is_active(self) -> bool:
        """Prove, before any case, that each edition reads the fake account home and nothing else.

        ``~`` with ``HOME`` unset is expanded from the account database, which is exactly what
        herdr-run resolves Herdr from, and ``check`` contacts no Herdr. A shim that did not take
        effect would leave the real account home in place, and every later case would talk to the
        real Herdr installed there; so a failure here stops the group.
        """
        case_root = self.harness.root / "e2e-000-shim"
        active = True
        for name, command in (
            ("python", self.harness.python_command),
            ("rust", self.harness.rust_command),
        ):
            edition = self._edition(name, command, case_root)
            outcome = _run(
                command,
                ("--json", "check", "git -C ~/probe status"),
                cwd=edition.project,
                home=edition.account,
                extra_env={**edition.environment(), "HOME": None},
            )
            expected = f'"rendered": "git -C {edition.account}/probe status"'
            ok = outcome.returncode == 0 and expected in outcome.stdout
            self.report.require(
                f"e2e/shim-active/{name}",
                ok,
                f"the preloaded shim did not make {edition.account} the account home: "
                f"{_describe(outcome)}",
            )
            active = active and ok
        # The guard itself: a program named systemd-run must be refused, and logged, not run.
        guard = case_root / "guard"
        guard.mkdir()
        decoy = guard / "systemd-run"
        decoy.write_text(f"#!/bin/sh\ntouch {guard}/ran\n", encoding="utf-8")
        decoy.chmod(0o755)
        refusals = guard / "refusals.log"
        environment = {
            **os.environ,
            _E2E_HOME_VARIABLE: str(guard),
            _E2E_REFUSALS_VARIABLE: str(refusals),
            "LD_PRELOAD": str(self.shim),
        }
        probe = (
            "import subprocess, sys\n"
            "try:\n    subprocess.run([sys.argv[1]], check=False)\n"
            "except PermissionError:\n    print('refused')\n"
        )
        completed = subprocess.run(
            [sys.executable, "-I", "-c", probe, str(decoy)],
            env=environment,
            capture_output=True,
            text=True,
            check=False,
            timeout=TIMEOUT_SECONDS,
        )
        guarded = (
            completed.stdout == "refused\n"
            and not (guard / "ran").exists()
            and refusals.exists()
            and refusals.read_text(encoding="utf-8") == f"{decoy}\n"
        )
        self.report.require(
            "e2e/shim-refuses-systemd-run",
            guarded,
            f"the shim let a program named systemd-run start: {completed!r}",
        )
        return active and guarded

    def case(
        self,
        label: str,
        config: str,
        steps: Sequence[_E2EStep],
        *,
        behaviour: Mapping[str, object] | None = None,
        setup: Callable[[_E2EEdition], None] | None = None,
        expect: Sequence[tuple[int, str]] = (),
        inspect: Callable[[_E2EEdition, list[dict[str, object]]], str | None] | None = None,
    ) -> None:
        """Run ``steps`` for both editions and compare them; ``expect`` pins what step N must say.

        Agreement alone could be two editions failing the same way, so each ``(N, text)`` in
        ``expect`` requires ``text`` in step N's output, which keeps a case about what it claims.
        ``inspect``, given each edition and the calls its fake recorded, returns what is wrong
        with them, or ``None``.

        Besides every step's outcome and every file left behind, the comparison covers each Herdr
        call twice: its argument vector, and what it was handed -- its environment, entry by entry,
        and its ignored signals.
        """
        self.serial += 1
        safe = re.sub(r"[^A-Za-z0-9_.-]+", "-", label).strip("-")
        case_root = self.harness.root / f"e2e-{self.serial:03d}-{safe}"
        observed: dict[str, tuple[list[Outcome], Outcome, Outcome, str, str, str | None]] = {}
        for name, command in (
            ("python", self.harness.python_command),
            ("rust", self.harness.rust_command),
        ):
            edition = self._edition(name, command, case_root)
            (edition.project / ".herdr-run.yaml").write_text(config, encoding="utf-8")
            edition.behave(behaviour or {})
            if setup is not None:
                setup(edition)
            outcomes: list[Outcome] = []
            try:
                for step in steps:
                    if step.action is not None:
                        step.action(edition)
                        outcomes.append(Outcome(0, f"<{step.label}>\n", ""))
                        continue
                    outcome = _run(
                        command,
                        step.args,
                        cwd=edition.project,
                        home=edition.account,
                        extra_env=edition.environment(),
                        mode=step.mode,
                    )
                    outcomes.append(
                        Outcome(
                            outcome.returncode,
                            _e2e_normalize(outcome.stdout, edition),
                            _e2e_normalize(outcome.stderr, edition),
                        )
                    )
                recorded = _fake_calls(edition.account)
                calls_text = "".join(json.dumps(call["argv"]) + "\n" for call in recorded)
                calls = Outcome(0, _e2e_normalize(calls_text, edition), "")
                handed = _normalize_text(
                    "".join(_handed_line(index, call) for index, call in enumerate(recorded)),
                    edition.root,
                )
                tree = _e2e_tree(edition)
                refused = edition.refusals.read_text(encoding="utf-8") if edition.refusals.exists() else ""
                wrong = inspect(edition, recorded) if inspect is not None else None
            finally:
                edition.end()
            observed[name] = (outcomes, calls, tree, refused, handed, wrong)
        python, rust = observed["python"], observed["rust"]
        for index, step in enumerate(steps):
            title = step.label or " ".join(step.args)[:60]
            name = f"e2e/{label}/{index}:{title}"
            if step.rc is None:
                self.report.same(
                    name, python[0][index], rust[0][index], allowed=frozenset(range(-64, 256))
                )
            else:
                self.report.exact(name, python[0][index], rust[0][index], expected_rc=step.rc)
        for index, text in expect:
            shown = python[0][index].stdout + python[0][index].stderr
            self.report.require(
                f"e2e/{label}/{index}:says:{text[:40]}",
                text in shown,
                f"step {index} does not say {text!r}: {_describe(python[0][index])}",
            )
        self.report.exact(f"e2e/{label}/herdr-calls", python[1], rust[1], expected_rc=0)
        self.report.require(
            f"e2e/{label}/handed-to-herdr",
            python[4] == rust[4],
            _first_difference(python[4], rust[4]),
        )
        if inspect is not None:
            for edition_name, seen in (("python", python), ("rust", rust)):
                self.report.require(
                    f"e2e/{label}/inspected/{edition_name}", seen[5] is None, str(seen[5])
                )
        self.report.exact(f"e2e/{label}/files", python[2], rust[2], expected_rc=0)
        self.report.require(
            f"e2e/{label}/nothing-refused",
            python[3] == "" and rust[3] == "",
            "an edition tried to start systemd-run or a Herdr outside the fake account home: "
            f"python={python[3]!r} rust={rust[3]!r}",
        )


#: The configuration every end-to-end case starts from.
_E2E_CONFIG = """\
allow: [printf, sh, git, ls, sleep, cat]
prefixes: []
spool_dir: .spool
workspace: e2e-cmds
retention_days: 7
"""


def _cleanup_checks_identity(harness: Harness, report: Report) -> None:
    """The end-of-case cleanup signals a recorded process only while it is still that process.

    A sleeping stand-in recorded with another start time -- what a PID that has since been given
    to another process looks like -- must survive the cleanup; recorded with its own it must not.
    Nor must what is left of a process group whose leader has exited, which is found by the case's
    ``HOME`` instead of by the leader's number.
    """
    account = harness.root / "cleanup-account"
    (account / "herdr-state").mkdir(parents=True)
    sleep = shutil.which("sleep") or "/bin/sleep"
    environment = {"HOME": str(account), "PATH": "/usr/bin:/bin"}
    survivor = subprocess.Popen(
        [sleep, "30"], env=environment, start_new_session=True, stdin=subprocess.DEVNULL
    )
    try:
        started = _start_ticks(survivor.pid)
        spared = started is not None and not _kill_if_same(survivor.pid, started + 1, group=True)
        report.require(
            "e2e/cleanup/spares-a-reused-pid",
            spared and survivor.poll() is None,
            "a process recorded with another start time was signalled",
        )
        ended = _kill_if_same(survivor.pid, started, group=True)
        survivor.wait(timeout=TIMEOUT_SECONDS)
        report.require(
            "e2e/cleanup/ends-the-recorded-process",
            ended and survivor.returncode == -signal.SIGKILL,
            f"the recorded process was not ended: returncode {survivor.returncode}",
        )
    finally:
        if survivor.poll() is None:
            survivor.kill()
            survivor.wait()
    leader = subprocess.Popen(
        ["/bin/sh", "-c", f"{shlex.quote(sleep)} 30 >/dev/null 2>&1 & echo $!"],
        env=environment,
        start_new_session=True,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
    )
    assert leader.stdout is not None
    straggler = int(leader.stdout.readline())
    leader.stdout.close()
    leader_start = _start_ticks(leader.pid)
    leader.wait(timeout=TIMEOUT_SECONDS)
    try:
        (account / "herdr-state" / "state.json").write_text(
            json.dumps({"processes": [[leader.pid, leader_start]]}), encoding="utf-8"
        )
        _end_fake_processes(account)
        _wait_until_gone(straggler)
        fields = _stat_fields(straggler)
        report.require(
            "e2e/cleanup/ends-a-leaderless-group-by-identity",
            fields is None or fields[0] == "Z",
            f"a process left in the group of an exited leader was not ended: {straggler}",
        )
    finally:
        if _stat_fields(straggler) is not None:
            _kill_if_same(straggler, _start_ticks(straggler), group=False)


def _e2e_run_and_report(harness: Harness, report: Report, shim: Path) -> None:
    _cleanup_checks_identity(harness, report)
    e2e = _EndToEnd(harness, report, shim)
    a = ("--agent", "alice")
    config = _E2E_CONFIG

    # Running commands, and everything they leave behind.
    e2e.case(
        "run-basic",
        config,
        [_run_step(*a, "run", "printf 'hello %s\\n' world", rc=0)],
        expect=[(0, "hello world\n")],
    )
    e2e.case("run-json", config, [_run_step("--json", *a, "run", "printf 'a\\377b\\n'", rc=0)])
    e2e.case(
        "run-exit-status",
        config,
        [_run_step(*a, "run", "sh -c 'echo out; echo err >&2; exit 3'", rc=3)],
    )
    e2e.case("run-killed-by-a-signal", config, [_run_step(*a, "run", "sh -c 'kill -9 $$'")])
    e2e.case("run-bytes-not-utf8", config, [_run_step(*a, "run", "printf 'a\\377b\\n'", rc=0)])

    def subdirectory(edition: _E2EEdition) -> None:
        (edition.project / "sub").mkdir()
        (edition.project / "sub" / "file.txt").write_text("x\n", encoding="utf-8")

    e2e.case(
        "run-cwd",
        config,
        [_run_step(*a, "run", "--cwd", "sub", "ls", rc=0), _run_step(*a, "run", "--cwd", "/nonexistent", "ls")],
        setup=subdirectory,
    )
    e2e.case("run-refused", config, [_run_step(*a, "run", "curl x", rc=77)])
    e2e.case("run-agent-name", config, [_run_step("--agent", "caf\u00e9-\u2603", "run", "printf ok", rc=0)])
    e2e.case(
        "run-tab-schema",
        config + "tab_name: '{project}--{agent}'\n",
        [_run_step(*a, "run", "printf ok", rc=0)],
    )
    e2e.case(
        "session-cache-and-status",
        config,
        [
            _run_step(*a, "status", rc=0),
            _run_step(*a, "run", "printf 1", rc=0),
            _run_step(*a, "run", "printf 2", rc=0),
            _run_step("--agent", "bob", "run", "printf 3", rc=0),
            _run_step(*a, "status", rc=0),
            _run_step("--json", *a, "status", rc=0),
            _run_step(*a, "target", rc=0),
            _run_step("--agent", "carol", "target", "--no-cache", rc=0),
            _run_step("--json", "--agent", "dave", "target"),
        ],
    )

    # Reap verdicts over a live population.
    def tab_of(edition: _E2EEdition, label: str) -> str | None:
        tabs = cast(list[dict[str, str]], edition.state().get("tabs", []))
        return next((tab["id"] for tab in tabs if tab.get("label") == label), None)

    def kill_bobs_shell(edition: _E2EEdition) -> None:
        tab = tab_of(edition, "bob")
        panes = cast(list[dict[str, str]], edition.state().get("panes", []))
        for pane in panes:
            if pane.get("tab") == tab:
                edition.kill_shell(pane["id"])

    def close_alice(edition: _E2EEdition) -> None:
        tab = tab_of(edition, "alice")
        if tab is not None:
            edition.herdr("tab", "close", tab)

    e2e.case(
        "reap-verdicts",
        config,
        [
            _run_step(*a, "run", "printf 1", rc=0),
            _run_step("--agent", "bob", "run", "printf 2", rc=0),
            _run_step("reap", rc=0),
            _do("kill bob's shell", kill_bobs_shell),
            _run_step("reap", rc=0),
            _do("close alice's tab by hand", close_alice),
            _run_step("--json", "reap", rc=0),
            _run_step(*a, "status", rc=0),
        ],
        expect=[(2, '"SHELL_ALIVE": 2'), (4, '"STALE": 1'), (6, '"GONE": 1')],
    )
    e2e.case(
        "reap-in-flight-after-a-timeout",
        config,
        [
            # Three seconds, so the partial output is there even on a loaded host.
            _run_step(*a, "run", "--timeout", "3", "sh -c 'echo partial; echo perr >&2; sleep 30'"),
            _run_step("reap", rc=0),
        ],
        expect=[(0, "partial"), (1, '"IN_FLIGHT": 1')],
    )

    def other_workspace(edition: _E2EEdition) -> None:
        (edition.project / ".herdr-run.yaml").write_text(
            config.replace("workspace: e2e-cmds", "workspace: elsewhere"), encoding="utf-8"
        )

    e2e.case(
        "reap-out-of-scope",
        config,
        [
            _run_step(*a, "run", "printf 1", rc=0),
            _do("move to another workspace", other_workspace),
            _run_step("reap", rc=0),
        ],
        expect=[(2, '"OUT_OF_SCOPE": 1')],
    )

    # The pane cap: the least recently used idle tab makes room; a busy one never does.
    capped = config + "max_panes: 2\n"
    e2e.case(
        "pane-cap-eviction",
        capped,
        [
            _run_step("--agent", "a1", "run", "printf 1", rc=0),
            _run_step("--agent", "a2", "run", "printf 2", rc=0),
            _run_step("--agent", "a3", "run", "printf 3", rc=0),
            _run_step("--agent", "a4", "run", "printf 4", rc=0),
            _run_step("reap", rc=0),
            _run_step("--agent", "a1", "status", rc=0),
        ],
        expect=[(2, "replaced idle tab"), (3, "replaced idle tab")],
    )

    def everything_busy(edition: _E2EEdition) -> None:
        edition.behave({"busy": True})

    e2e.case(
        "pane-cap-all-busy",
        capped,
        [
            _run_step("--agent", "a1", "run", "printf 1", rc=0),
            _run_step("--agent", "a2", "run", "printf 2", rc=0),
            _do("every pane is busy", everything_busy),
            _run_step("--agent", "a3", "run", "printf 3"),
        ],
        expect=[(3, "could be replaced")],
    )

    # Readiness: a busy pane, a half-typed line, and prompts that cannot be judged.
    for label, behaviour in (
        ("busy", {"busy": True}),
        ("half-typed-line", {"pane_text": "user@host:~$ ls -la\n"}),
        ("no-pane-text", {"pane_text": ""}),
        ("unrecognised-prompt", {"pane_text": "weird prompt>\n"}),
        ("foreground-is-not-a-shell", {"fg_name": "python3"}),
    ):

        def become(edition: _E2EEdition, behaviour: Mapping[str, object] = behaviour) -> None:
            edition.behave(behaviour)

        e2e.case(
            f"readiness-{label}",
            config,
            [
                _run_step(*a, "run", "printf warm", rc=0),
                _do(label, become),
                _run_step(*a, "run", "--wait-ready", "0.2", "printf x"),
                _run_step(*a, "target"),
            ],
        )

    # Herdr failing, or answering with something that is not what it promises.
    hostile = "boom \x1b[31m red \u202e \u2028 done"
    for words in (
        "workspace list",
        "workspace create",
        "tab list",
        "tab create",
        "tab rename",
        "pane list",
        "pane process-info",
        "pane read",
        "pane run",
    ):
        # Another agent's run first, so the workspace exists and tabs are listed and created; the
        # calls that only a new workspace makes (creating it, renaming its first tab) run fresh.
        fresh = words in ("workspace list", "workspace create", "tab rename")

        def misbehave(edition: _E2EEdition, words: str = words) -> None:
            edition.behave({"fail": {words: hostile}})

        warm_up = [] if fresh else [_run_step("--agent", "bob", "run", "printf warm", rc=0)]
        e2e.case(
            f"failure-{words.replace(' ', '-')}",
            config,
            [
                *warm_up,
                _do(f"{words} fails", misbehave),
                _run_step(*a, "run", "--wait-ready", "0.2", "--timeout", "2", "printf x"),
            ],
            expect=[(len(warm_up) + 1, "\\u{202e}")],
        )
    for label, words, reply, said in (
        ("not-json", "workspace list", "not json", "non-JSON output"),
        (
            "id-not-a-string",
            "workspace list",
            '{"result": {"workspaces": [{"workspace_id": 5, "label": "e2e-cmds"}]}}',
            "is not a string",
        ),
        (
            "ambiguous-label",
            "workspace list",
            '{"result": {"workspaces": [{"workspace_id": "w", "label": "e2e-cmds"}, '
            '{"workspace_id": "v", "label": "e2e-cmds"}]}}',
            "is ambiguous",
        ),
        (
            "surrogate-under-a-duplicate-key",
            "workspace list",
            '{"result": {"workspaces": [], "x": "\\ud800", "x": 1}}',
            "non-JSON output",
        ),
        ("nested-too-deep", "workspace list", "[" * 200 + "]" * 200, "non-JSON output"),
        ("bytes-not-utf8", "workspace list", "\udcff\udcfe{}", "non-JSON output"),
        ("create-without-objects", "workspace create", '{"result": {}}', "is not an object"),
        (
            "process-info-for-another-pane",
            "pane process-info",
            '{"result": {"process_info": {"pane_id": "zz", "shell_pid": 5, '
            '"foreground_process_group_id": 5, "foreground_processes": [{"pid": 5}]}}}',
            'returned pane "zz"',
        ),
        (
            "pane-from-another-workspace",
            "pane list",
            '{"result": {"panes": [{"pane_id": "p", "tab_id": "t", "workspace_id": "other"}]}}',
            'from workspace "other"',
        ),
    ):
        e2e.case(
            f"reply-{label}",
            config,
            [_run_step(*a, "run", "--wait-ready", "0.2", "--timeout", "2", "printf x")],
            behaviour={"raw": {words: reply}},
            expect=[(0, said)],
        )

    # A Herdr the kernel cannot execute, and a caller that ignores SIGCHLD.
    def herdr_without_shebang(edition: _E2EEdition) -> None:
        herdr = edition.account / ".local" / "bin" / "herdr"
        herdr.write_text('touch "$0.ran"\necho \'{"server": {"running": true}}\'\n', encoding="utf-8")
        herdr.chmod(0o755)

    # Only `status`: a command that needs a server would try to start one through systemd-run
    # when this Herdr cannot answer, and the shim would (rightly) refuse it.
    e2e.case(
        "herdr-not-executable",
        config,
        [_run_step(*a, "status", rc=0), _run_step("--json", *a, "status", rc=0)],
        setup=herdr_without_shebang,
        expect=[(0, "no server is running")],
    )
    e2e.case(
        "sigchld-ignored",
        config,
        [
            _run_step(*a, "run", "printf x", rc=0, mode="sigchld-ignored"),
            _run_step(*a, "run", "sh -c 'exit 4'", rc=4, mode="sigchld-ignored"),
        ],
    )
    e2e.case(
        "sigchld-ignored-failing-herdr",
        config,
        [_run_step(*a, "run", "--timeout", "2", "printf x", mode="sigchld-ignored")],
        behaviour={"fail": {"pane run": "pane run refused"}},
    )

    # net-doctor through the fake pane, against a local repository and a missing one.
    git = shutil.which("git")
    if git is not None:

        def repository(edition: _E2EEdition) -> None:
            remote = edition.root / "remote"
            # Fixed dates, so both editions' remotes hold the same commit and print the same hash.
            environment = {
                **os.environ,
                "GIT_CONFIG_NOSYSTEM": "1",
                "HOME": str(edition.account),
                "GIT_AUTHOR_DATE": "2026-01-01T00:00:00Z",
                "GIT_COMMITTER_DATE": "2026-01-01T00:00:00Z",
            }
            subprocess.run([git, "init", "-q", str(remote)], check=True, capture_output=True, env=environment)
            subprocess.run(
                [git, "-C", str(remote), "-c", "user.email=e2e@example.invalid", "-c", "user.name=e2e",
                 "commit", "-q", "--allow-empty", "-m", "x"],
                check=True,
                capture_output=True,
                env=environment,
            )
            (edition.project / ".herdr-run.yaml").write_text(
                config + f"probe_remote: {remote}\n", encoding="utf-8"
            )

        e2e.case(
            "net-doctor-local-repository",
            config,
            [_run_step(*a, "net-doctor")],
            setup=repository,
            expect=[(0, "rc=0 SUCCEEDED")],
        )
    e2e.case(
        "net-doctor-missing-repository",
        config + "probe_remote: /nonexistent/repository\n",
        [_run_step(*a, "net-doctor")],
    )

    # What a process either edition starts is handed. The environment holds a repeated name, an
    # entry without '=', and empty names (_RAW_EXTRAS); every Herdr call must get exactly what the
    # shared rule makes of it, with PATH removed and HOME replaced where it stands, and the direct
    # net-doctor probe (whose login shell records its own environment) the same without changes.
    def herdr_calls_by_the_rule(edition: _E2EEdition, calls: list[dict[str, object]]) -> str | None:
        raw = _raw_entries(_environment(edition.account, edition.environment()))
        account = os.fsencode(str(edition.account))
        herdr = [
            entry.decode("utf-8", errors="surrogateescape")
            for entry in _by_the_rule(raw, ((b"PATH", None), (b"HOME", account)))
        ]
        if not calls:
            return "the fake Herdr recorded no call"
        for call in calls:
            handed = cast(list[str], call.get("environ") or [])
            if handed != herdr:
                words = cast(list[str], call["argv"])[:2]
                return f"{words} was handed:\n" + _first_difference(
                    "\n".join(herdr), "\n".join(handed)
                )
        logins = edition.project / "login-environments.txt"
        if not logins.exists():
            return None
        probe = [entry.decode("utf-8", errors="surrogateescape") for entry in _by_the_rule(raw)]
        blocks = logins.read_bytes().decode("utf-8", errors="surrogateescape").split("--- login\n")
        marked = "\nHERDR_E2E_VALUE=a=b\n"
        direct = next((block for block in blocks if marked in f"\n{block}"), None)
        if direct is None:
            return f"no login shell recorded the raw environment: {blocks!r}"
        # Compared as text: a value may itself hold a line break.
        wanted = "".join(f"{entry}\n" for entry in probe)
        if direct != wanted:
            return "the direct net-doctor probe was handed:\n" + _first_difference(wanted, direct)
        return None

    def record_login_environments(edition: _E2EEdition) -> None:
        if git is not None:
            repository(edition)
        dump = shlex.quote(str(edition.project / "login-environments.txt"))
        (edition.account / ".bash_profile").write_text(
            f"{{ echo '--- login'; tr '\\0' '\\n' < \"/proc/$$/environ\"; }} >> {dump}\n",
            encoding="utf-8",
        )

    raw_steps = [_run_step(*a, "run", "printf x", rc=0, mode="raw-environment")]
    if git is not None:
        raw_steps.append(_run_step(*a, "net-doctor", rc=0, mode="raw-environment"))
    e2e.case(
        "raw-environment",
        config,
        raw_steps,
        setup=record_login_environments,
        inspect=herdr_calls_by_the_rule,
    )

    # Signals the caller left ignored stay ignored in every Herdr call, signal 33 included, which
    # the C library takes for itself once a process starts a thread; SIGPIPE comes back at its
    # default action.
    kept_ignored = {int(signal.SIGHUP), int(signal.SIGUSR1)}
    if os.uname().machine in _RT_SIGACTION:
        kept_ignored.add(33)
    reset = {int(signal.SIGPIPE), int(signal.SIGXFSZ), int(signal.SIGCHLD)}

    def handed_ignored_signals(edition: _E2EEdition, calls: list[dict[str, object]]) -> str | None:
        if not calls:
            return "the fake Herdr recorded no call"
        for call in calls:
            ignored = set(cast(list[int], call.get("ignored_signals") or []))
            if not kept_ignored <= ignored or ignored & reset:
                words = cast(list[str], call["argv"])[:2]
                return (
                    f"{words} was handed {sorted(ignored)} ignored; it must keep "
                    f"{sorted(kept_ignored)} and must not keep {sorted(reset)}"
                )
        return None

    e2e.case(
        "signals-ignored",
        config,
        [
            _run_step(*a, "run", "printf x", rc=0, mode="signals-ignored"),
            _run_step(*a, "status", rc=0, mode="signals-ignored"),
        ],
        inspect=handed_ignored_signals,
    )

    # Retention: completed runs older than the window go when a later run is written.
    def plant_runs(edition: _E2EEdition) -> None:
        runs = edition.project / ".spool" / "runs"
        now = time.time()
        for name, age_days, files in (
            ("20200101T000000-old-1", 10, {"exit_code": "0\n", "stdout": "x", "meta.json": "{}"}),
            ("20200101T000000-old-unfinished-2", 10, {"stdout": "x"}),
            ("20260101T000000-recent-3", 1, {"exit_code": "0\n"}),
            ("not-a-run", 10, {"exit_code": "0\n"}),
            ("20200101T000000-bad-marker-4", 10, {"exit_code": "abc"}),
            ("20200101T000000-just-inside-5", 6.999, {"exit_code": "0\n"}),
            ("20200101T000000-just-outside-6", 7.001, {"exit_code": "0\n"}),
        ):
            directory = runs / name
            directory.mkdir(parents=True)
            stamp = now - age_days * 86400
            for file_name, content in files.items():
                path = directory / file_name
                path.write_text(content, encoding="utf-8")
                os.utime(path, (stamp, stamp))
            os.utime(directory, (stamp, stamp))
        os.symlink("/tmp", runs / "20200101T000000-link-7")
        (runs / "20200101T000000-file-8").write_text("x", encoding="utf-8")

    e2e.case("retention", config, [_run_step(*a, "run", "printf x", rc=0)], setup=plant_runs)

    # A session cache that is corrupt, stale, or hostile is never trusted, and is replaced.
    for label, content in (
        ("garbage", "not json"),
        ("array", "[]"),
        (
            "surrogate-under-a-duplicate-key",
            '{"e2e-cmds\\u0000alice": {"pane_id": "\\ud800", "pane_id": "w9:p9", "tab_id": "w9:t9", '
            '"tab_label": "alice", "workspace_id": "w9", "workspace_label": "e2e-cmds"}}',
        ),
        (
            "stale-ids",
            '{"e2e-cmds\\u0000alice": {"pane_id": "w9:p9", "tab_id": "w9:t9", "tab_label": "alice", '
            '"workspace_id": "w9", "workspace_label": "e2e-cmds"}}',
        ),
        (
            "wrong-types",
            '{"e2e-cmds\\u0000alice": {"pane_id": 5, "tab_id": "w9:t9", "tab_label": "alice", '
            '"workspace_id": "w9", "workspace_label": "e2e-cmds"}}',
        ),
        ("nested-too-deep", "[" * 300 + "]" * 300),
        ("not-a-number", '{"x": NaN}'),
        ("byte-order-mark", "\ufeff{}"),
    ):

        def plant_cache(edition: _E2EEdition, content: str = content) -> None:
            spool = edition.project / ".spool"
            spool.mkdir(parents=True, exist_ok=True)
            (spool / "session-cache.json").write_text(content, encoding="utf-8")

        e2e.case(
            f"cache-{label}",
            config,
            [_run_step(*a, "run", "printf x", rc=0), _run_step(*a, "target", rc=0)],
            setup=plant_cache,
        )


def _prepare_account_shim(harness: Harness, report: Report) -> None:
    """Build the account-home shim and prove it takes effect, before any group that needs it.

    Every invocation that may reach a Herdr -- the ``e2e/*`` group, and ``status``, ``reap``, and
    the unsafe-path cases outside it -- runs against a fake Herdr behind this shim, never against a
    Herdr installed on the host. When the shim cannot be had, those invocations are skipped, as
    listed and required skips, or (when building or proving it failed) not run at all.
    """
    present = [path for path in _E2E_SYSTEM_HERDR if os.path.exists(path)]
    if present:
        harness.shim_skip = (
            "system-herdr",
            f"{present[0]} exists, and herdr-run looks there before the account home",
        )
        return
    try:
        shim, compiler = _compile_account_home_shim(harness.root)
    except RuntimeError as error:
        report.require("e2e/shim-builds", False, str(error))
        return
    if shim is None:
        harness.shim_skip = ("c-compiler", compiler)
        return
    report.notes.append(f"e2e: the account-home shim was built with {compiler}")
    if not _EndToEnd(harness, report, shim).shim_is_active():
        report.notes.append(
            "e2e: nothing that could reach a Herdr ran, because the shim check failed"
        )
        return
    harness.shim = shim


def _end_to_end(harness: Harness, report: Report) -> None:
    """Compare both editions against one fake Herdr, through their production clients."""
    if harness.account_unavailable(report, "e2e/all"):
        return
    assert harness.shim is not None
    _e2e_run_and_report(harness, report, harness.shim)


def build_report(
    python_command: Sequence[str],
    rust_command: Sequence[str],
    *,
    launchers: bool = True,
) -> Report:
    """Run the complete differential and return its structured report without printing."""

    report = Report()
    with tempfile.TemporaryDirectory(prefix="herdr-run-cross-") as temporary:
        harness = Harness(Path(temporary), python_command, rust_command)
        _prepare_account_shim(harness, report)
        _bootstrap(harness, report)
        _policy(harness, report)
        _cli_grammar(harness, report)
        _agents(harness, report)
        _config_success(harness, report)
        _config_malformed(harness, report)
        _os_errors(harness, report)
        _non_utf8(harness, report)
        _locales(harness, report)
        _output_failures(harness, report)
        _dry_run(harness, report)
        _spool(harness, report)
        _net_doctor(harness, report)
        _init(harness, report)
        _upgrades(harness, report)
        _status(harness, report)
        _documentation(harness, report)
        _reap(harness, report)
        _deleted_cwd(harness, report)
        _child_processes(harness, report)
        _sigchld_handed_over(harness, report)
        _held_pipe(harness, report)
        _signals_and_limits(harness, report)
        _unsafe_paths(harness, report)
        _unsafe_json(harness, report)
        _exec_format(harness, report)
        _end_to_end(harness, report)
        if launchers:
            _launchers(harness, report)
    report.notes.append(
        "the live Herdr protocol is compared end to end (e2e/*) against one fake Herdr that a "
        "preloaded account-home shim puts where production looks, without any executable "
        "override in either edition; the golden corpora in rs/herdr-run/testdata still pin each "
        "protocol call, readiness rule, and record format, through both editions' unit tests"
    )
    report.notes.append(
        "nothing reaches an installed Herdr: status/*, reap/*, unsafe-paths/*, and the status "
        "calls in config/* and locale/* run against the fake Herdr behind the account-home shim, "
        "with no workspace, and their Herdr calls are compared and must only read; the reap/* "
        "cases pair verdicts over planted records, and STALE, SHELL_ALIVE, GONE, and IN_FLIGHT "
        "over live panes are paired in e2e/reap-*"
    )
    report.notes.append(
        "NUL cannot be represented in a POSIX argv entry; all other terminal-control classes are covered"
    )
    report.notes.append(
        "the editions are invoked directly (the Python package under the real interpreter, the "
        "Rust binary); common/bin/herdr-run is a smoke test with its own lines stripped, and its "
        "launcher behaviour (deleted working directory, HOME, locale warnings) is listed in "
        "cross/README.md rather than compared"
    )
    report.notes.append(
        "a multibyte locale that is not UTF-8 (ja_JP.eucJP) is not installed on every host, so the "
        "Python edition's restart from /proc/self/cmdline is compared under ISO-8859-1 here and "
        "pinned by unit tests for the multibyte case"
    )
    return report


def compare_herdr_run(
    python_command: Sequence[str],
    rust_command: Sequence[str],
) -> int:
    """Run, print, and return the status of the Python/Rust black-box comparison."""

    report = build_report(python_command, rust_command)
    for note in report.notes:
        print(f"NOTE [herdr-run] {note}")
    for label, kind, reason in report.skipped:
        print(f"  SKIPPED [{kind}; REQUIRED] {label}: {reason}")
    if report.failures:
        for failure in report.failures:
            print(f"DIVERGENCE [{failure}]")
        print(
            f"cross[herdr-run]: {len(report.failures)} divergence(s) "
            f"out of {report.checks} paired checks"
        )
        return 1
    if report.skipped:
        # Run standalone there is no way to declare a partial run: every skip is required.
        print(
            f"cross[herdr-run]: INCOMPLETE - {report.checks} paired checks agree, but "
            f"{len(report.skipped)} required check(s) did not run"
        )
        return 1
    print(f"cross[herdr-run]: OK - {report.checks} paired checks agree")
    return 0


def _decode_argv(encoded: str, option: str) -> list[str]:
    try:
        decoded: object = json.loads(encoded)
    except json.JSONDecodeError as error:
        raise ValueError(f"{option} must be valid JSON: {error}") from error
    if not isinstance(decoded, list):
        raise ValueError(f"{option} must decode to a JSON array of strings")
    values = cast(list[object], decoded)
    if not values or not all(isinstance(value, str) for value in values):
        raise ValueError(f"{option} must decode to a non-empty JSON array of strings")
    return [value for value in values if isinstance(value, str)]


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="black-box Python/Rust differential for herdr-run",
    )
    parser.add_argument(
        "--python-command-json",
        required=True,
        help='complete Python implementation argv as JSON, e.g. ["python3","-m","herdr_run"]',
    )
    parser.add_argument(
        "--rust-command-json",
        required=True,
        help='complete Rust implementation argv as JSON, e.g. ["rs/bin/herdr-run"]',
    )
    namespace = parser.parse_args(list(argv) if argv is not None else None)
    python_encoded = cast(str, namespace.python_command_json)
    rust_encoded = cast(str, namespace.rust_command_json)
    try:
        python_command = _decode_argv(python_encoded, "--python-command-json")
        rust_command = _decode_argv(rust_encoded, "--rust-command-json")
    except ValueError as error:
        parser.error(str(error))
    return compare_herdr_run(python_command, rust_command)


if __name__ == "__main__":
    raise SystemExit(main())
