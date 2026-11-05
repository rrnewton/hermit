"""Execute an admitted command in a pane and collect its real stdout, stderr, and exit code.

**Results are read from FILES, never scraped from the terminal.** The filesystem is shared across
the sandbox boundary, so the pane can redirect into a spool directory this process reads directly.
Scraping would inherit every terminal problem at once — an 80-column pane hard-wraps output
mid-token, the scrollback is finite so long output is silently truncated, ANSI and progress bars
corrupt the text, and there is no exit code anywhere on screen. The wrapper is::

    { cd <cwd> && <command> ; } >stdout 2>stderr; printf '%s\\n' "$?" >exit_code

so ``exit_code`` appearing is both the completion signal and the result, and a failed ``cd`` is
reported as a failure of the run rather than silently running in the wrong directory.

Because ``exit_code`` is written by a separate command after the redirection closes, its EXISTENCE
is what marks completion — but a file can exist while still being written, so a read that does not
parse as an integer is treated as an incomplete write rather than as a corrupt result.
"""

from __future__ import annotations

import math
import os
import stat
import time
import fcntl
from collections.abc import Callable, Iterator
from contextlib import contextmanager
from dataclasses import dataclass

from herdr_run.allowlist import Admission, reject_terminal_controls
from herdr_run.audit import make_private_dirs
from herdr_run.client import HerdrClient
from herdr_run.config import MAX_TIMEOUT_SECONDS, Config
from herdr_run.errors import (
    ConfigError,
    HerdrRunError,
    HerdrUnavailable,
    PaneBusy,
    Refused,
    RunTimeout,
)
from herdr_run.identity import current_boot_id, process_start_ticks
from herdr_run.readiness import Readiness, assess, infer_prompt_tail
from herdr_run.retention import prune_runs, runs_root
from herdr_run.session import Target
from herdr_run.state import open_lock_file, pane_lock_path
from herdr_run.textfmt import (
    MAX_EXIT_MARKER_BYTES,
    format_round3,
    json_text,
    number_text,
    os_error_text,
    parse_exit_marker,
)

__all__ = ["RunResult", "execute", "wait_ready", "spool_paths", "read_output_bytes"]


@dataclass(frozen=True)
class SpoolPaths:
    """Where one run's captured output and exit code live on disk."""

    directory: str
    stdout: str
    stderr: str
    exit_code: str


@dataclass(frozen=True)
class RunResult:
    """A completed run: the command's own result plus the evidence it was allowed to run."""

    exit_code: int
    stdout: str
    stderr: str
    stdout_bytes: bytes
    stderr_bytes: bytes
    run_id: str
    spool: SpoolPaths
    target: Target
    readiness: Readiness
    duration_seconds: float


def spool_paths(config: Config, run_id: str) -> SpoolPaths:
    """Resolve the spool file paths for one run id."""
    root = _spool_root(config)
    directory = os.path.join(root, "runs", run_id)
    return SpoolPaths(
        directory=directory,
        stdout=os.path.join(directory, "stdout"),
        stderr=os.path.join(directory, "stderr"),
        exit_code=os.path.join(directory, "exit_code"),
    )


def _spool_root(config: Config) -> str:
    root = config.spool_dir
    if not os.path.isabs(root):
        root = os.path.join(config.project_root, root)
    return root


def _ensure_private_directory(path: str) -> None:
    make_private_dirs(path)
    metadata = os.lstat(path)
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
        raise NotADirectoryError(f"run spool path is not a real directory: {path}")


def _create_private_file(path: str, contents: bytes = b"") -> None:
    flags = (
        os.O_CREAT
        | os.O_TRUNC
        | os.O_WRONLY
        | getattr(os, "O_CLOEXEC", 0)
        | getattr(os, "O_NOFOLLOW", 0)
    )
    descriptor = os.open(path, flags, 0o600)
    try:
        os.fchmod(descriptor, 0o600)
        remaining = memoryview(contents)
        while remaining:
            written = os.write(descriptor, remaining)
            if written == 0:
                raise OSError(f"short write while creating {path}")
            remaining = remaining[written:]
    finally:
        os.close(descriptor)


def make_run_id(agent: str, *, now: float, pid: int) -> str:
    """A per-run identifier that is unique per (time, process) and readable in a directory listing."""
    stamp = time.strftime("%Y%m%dT%H%M%S", time.gmtime(now))
    # ASCII letters, digits, '-' and '_' only: Unicode character properties differ between
    # language and library versions, and a run ID names a directory both editions must agree on.
    safe_agent = (
        "".join(
            char if (char.isascii() and char.isalnum()) or char in "-_" else "-" for char in agent
        )
        or "agent"
    )
    return f"{stamp}-{safe_agent}-{pid}"


def _allocate_spool(config: Config, base_run_id: str) -> tuple[str, SpoolPaths]:
    """Create a never-reused run directory, adding a bounded suffix on collision.

    A timestamp-to-the-second plus PID is readable but not unique across repeated library calls or
    PID reuse. Reusing a directory could consume its old ``exit_code`` before the new command had
    even started. Atomic directory creation makes the filesystem arbitrate uniqueness.
    """
    root = _spool_root(config)
    runs = os.path.join(root, "runs")
    try:
        _ensure_private_directory(root)
    except OSError as exc:
        raise HerdrUnavailable(
            f"cannot create private run spool root {root}: {os_error_text(exc)}"
        ) from exc
    try:
        _ensure_private_directory(runs)
    except OSError as exc:
        raise HerdrUnavailable(
            f"cannot create private run spool parent {runs}: {os_error_text(exc)}"
        ) from exc
    for collision in range(10_000):
        run_id = base_run_id if collision == 0 else f"{base_run_id}-{collision}"
        spool = spool_paths(config, run_id)
        try:
            os.mkdir(spool.directory, mode=0o700)
        except FileExistsError:
            continue
        except OSError as exc:
            raise HerdrUnavailable(
                f"cannot create run spool {spool.directory}: {os_error_text(exc)}"
            ) from exc
        return run_id, spool
    raise HerdrUnavailable(
        f"could not allocate a unique run spool after 10000 collisions for {base_run_id}"
    )


@contextmanager
def _pane_lock(
    target: Target,
    *,
    timeout: float,
    monotonic: Callable[[], float],
    sleep: Callable[[float], None],
    poll_interval: float,
) -> Iterator[None]:
    """Hold the per-pane launch/result lock, bounded by the readiness wait budget."""
    path = pane_lock_path(target.pane_id)
    handle = open_lock_file(path)
    deadline = monotonic() + max(timeout, 0.0)
    try:
        while True:
            try:
                fcntl.flock(handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if monotonic() >= deadline:
                    raise PaneBusy(
                        f"pane {target.pane_id} ({target.tab_label}) is already reserved by "
                        "another herdr-run. Nothing was executed."
                    )
                sleep(poll_interval)
            except OSError as exc:
                raise HerdrUnavailable(
                    f"cannot lock pane {target.pane_id}: {os_error_text(exc)}"
                ) from exc
        yield
    finally:
        # Closing releases `flock` even when readiness, launch, or collection raised.
        handle.close()


def wait_ready(
    client: HerdrClient,
    config: Config,
    target: Target,
    *,
    prompt_tail: str | None,
    timeout: float,
    sleep: Callable[[float], None] = time.sleep,
    monotonic: Callable[[], float] = time.monotonic,
    poll_interval: float = 0.25,
    required_consecutive: int = 2,
) -> Readiness:
    """Poll until the pane is ready, or raise :class:`PaneBusy`.

    ``required_consecutive`` readings are demanded because ``herdr pane run`` submits a line
    asynchronously: for a few milliseconds after a previous command is submitted the kernel has not
    yet moved the foreground process group, so a single sample can catch a busy pane looking idle.
    """
    _validate_timeout(timeout, "readiness timeout")
    deadline = monotonic() + max(timeout, 0.0)
    consecutive = 0
    readings = 0
    last: Readiness | None = None
    while True:
        last = assess(client, target.pane_id, config, prompt_tail=prompt_tail)
        readings += 1
        if last.ready:
            consecutive += 1
            if consecutive >= required_consecutive:
                return last
        else:
            consecutive = 0
        # The deadline governs how long we WAIT for a busy pane, but it must never cut the
        # confirmation short: with the default `ready_timeout_seconds: 0` a single reading would
        # otherwise be taken and then rejected for not being two, so an idle pane could never be
        # declared ready. Always allow at least `required_consecutive` readings.
        if readings >= required_consecutive and monotonic() >= deadline:
            break
        sleep(poll_interval)

    assert last is not None  # the loop always takes at least one reading
    if last.ready:
        # Ran out of time while the pane looked ready but had not yet been ready long enough.
        raise PaneBusy(
            f"pane {target.pane_id} ({target.tab_label}) looked idle but not for "
            f"{required_consecutive} consecutive checks within {number_text(timeout)}s: "
            f"{last.describe()}"
        )
    raise PaneBusy(
        f"pane {target.pane_id} ({target.tab_label}) is not ready after {number_text(timeout)}s: "
        f"{last.describe()}. "
        "Nothing was executed."
    )


def build_shell_command(admission: Admission, spool: SpoolPaths, cwd: str) -> str:
    """Render the exact line typed into the pane.

    Every interpolated value is shell-quoted: ``admission.rendered`` quotes each argv token, and the
    paths are quoted here. Nothing in this string is attacker-chosen unquoted text.
    """
    import shlex

    for description, value in (
        ("working directory", cwd),
        ("stdout spool path", spool.stdout),
        ("stderr spool path", spool.stderr),
        ("exit-code spool path", spool.exit_code),
    ):
        try:
            reject_terminal_controls(value, description)
        except Refused as exc:
            raise HerdrUnavailable(str(exc)) from exc

    inner = (
        f"umask 077; {{ cd {shlex.quote(cwd)} && {admission.rendered} ; }} "
        f">{shlex.quote(spool.stdout)} 2>{shlex.quote(spool.stderr)}; "
        f"printf '%s\\n' \"$?\" >{shlex.quote(spool.exit_code)}"
    )
    # The target may be fish or another non-POSIX interactive shell. Keep the text typed into that
    # shell to a portable command invocation and run the actual wrapper under sh.
    return f"command sh -c {shlex.quote(inner)}"


def read_exit_code(path: str) -> int | None:
    """Read a completion marker: the recorded exit code, or ``None`` while it is absent,
    incompletely written, or not a marker at all (see :func:`herdr_run.textfmt.parse_exit_marker`).
    """
    try:
        with open(path, "rb") as handle:
            payload = handle.read(MAX_EXIT_MARKER_BYTES + 1)
    except OSError:
        return None
    return parse_exit_marker(payload)


def _read_bytes(path: str) -> bytes:
    try:
        with open(path, "rb") as handle:
            return handle.read()
    except OSError as exc:
        raise HerdrUnavailable(
            f"cannot read run spool output {path}: {os_error_text(exc)}"
        ) from exc


def _read_text_best_effort(path: str) -> str:
    try:
        return _read_bytes(path).decode("utf-8", errors="replace")
    except HerdrUnavailable:
        return ""


def read_output_bytes(result: RunResult) -> tuple[bytes, bytes]:
    """Return the byte-exact stdout/stderr captured with this immutable result."""
    return result.stdout_bytes, result.stderr_bytes


def _validate_timeout(value: float, what: str) -> None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ConfigError(f"{what}: must be a number")
    if not math.isfinite(value):
        raise ConfigError(f"{what}: must be finite")
    if value < 0:
        raise ConfigError(f"{what}: must not be negative")
    if value > MAX_TIMEOUT_SECONDS:
        raise ConfigError(f"{what}: must not exceed {MAX_TIMEOUT_SECONDS:.0f} seconds")


def execute(
    client: HerdrClient,
    config: Config,
    target: Target,
    admission: Admission,
    *,
    agent: str,
    cwd: str,
    ready_timeout: float,
    timeout: float,
    now: Callable[[], float] = time.time,
    monotonic: Callable[[], float] = time.monotonic,
    sleep: Callable[[float], None] = time.sleep,
    poll_interval: float = 0.25,
    home: str | None = None,
) -> RunResult:
    """Run ``admission`` in ``target``'s pane and return its real result."""
    _validate_timeout(ready_timeout, "readiness timeout")
    _validate_timeout(timeout, "command timeout")
    lock_started = monotonic()
    with _pane_lock(
        target,
        timeout=ready_timeout,
        monotonic=monotonic,
        sleep=sleep,
        poll_interval=poll_interval,
    ):
        elapsed_for_lock = max(0.0, monotonic() - lock_started)
        remaining_ready = max(0.0, ready_timeout - elapsed_for_lock)
        prompt_tail = infer_prompt_tail(config, home=home)
        readiness = wait_ready(
            client,
            config,
            target,
            prompt_tail=prompt_tail,
            timeout=remaining_ready,
            sleep=sleep,
            monotonic=monotonic,
            poll_interval=poll_interval,
        )

        base_run_id = make_run_id(agent, now=now(), pid=os.getpid())
        # Retention is applied as a side effect of writing a new run, so it cannot silently stop
        # the way a cron or timer can. Prune before allocation so a zero-day window cannot select
        # the brand-new run itself. It never raises; see :mod:`herdr_run.retention`.
        prune_runs(
            runs_root(config.spool_dir, config.project_root),
            retention_days=config.retention_days,
        )
        run_id, spool = _allocate_spool(config, base_run_id)
        # Pre-create output files; exit_code deliberately remains absent as the completion signal.
        try:
            for path in (spool.stdout, spool.stderr):
                _create_private_file(path)
            _create_private_file(
                os.path.join(spool.directory, "command"),
                (admission.rendered + "\n").encode("utf-8"),
            )
        except OSError as exc:
            raise HerdrUnavailable(
                f"cannot initialize run spool {spool.directory}: {os_error_text(exc)}"
            ) from exc

        started = monotonic()
        client.run(target.pane_id, build_shell_command(admission, spool, cwd))

        deadline = started + max(timeout, 0.0)
        while True:
            code = read_exit_code(spool.exit_code)
            if code is not None:
                stdout_bytes = _read_bytes(spool.stdout)
                stderr_bytes = _read_bytes(spool.stderr)
                return RunResult(
                    exit_code=code,
                    stdout=stdout_bytes.decode("utf-8", errors="replace"),
                    stderr=stderr_bytes.decode("utf-8", errors="replace"),
                    stdout_bytes=stdout_bytes,
                    stderr_bytes=stderr_bytes,
                    run_id=run_id,
                    spool=spool,
                    target=target,
                    readiness=readiness,
                    duration_seconds=monotonic() - started,
                )
            if monotonic() >= deadline:
                break
            sleep(poll_interval)

        # Record the run BEFORE reporting the timeout, with a null exit code. This is the only
        # writer that can produce the state :mod:`herdr_run.reap` calls IN FLIGHT, and it is the
        # literal truth: the command is still running in a pane this process does not own. Without
        # it the reaper's "the agent is thinking" rule has no way of ever being true, and the one
        # pane that provably still has work in it would be the one pane leaving no evidence.
        _write_unfinished_meta(
            run_id=run_id,
            spool=spool,
            agent=agent,
            admission=admission,
            config=config,
            target=target,
            readiness=readiness,
            duration_seconds=monotonic() - started,
        )

        # Carry partial output with the typed failure. A bare message would make a timed-out run
        # look falsely empty to a caller that does not inspect the spool directory.
        timed_out = RunTimeout(
            f"command did not finish within {number_text(timeout)}s. It is STILL RUNNING in pane "
            f"{target.pane_id} ({target.tab_label}) and was not killed. Partial output is in "
            f"{spool.directory}; the exit code will appear in {spool.exit_code} when it finishes."
        )
        timed_out.partial_stdout = _read_text_best_effort(spool.stdout)
        timed_out.partial_stderr = _read_text_best_effort(spool.stderr)
        timed_out.spool_directory = spool.directory
        raise timed_out


def write_meta(result: RunResult, admission: Admission, config: Config, agent: str) -> str:
    """Record what ran, where, and on what evidence it was allowed to. Returns the file path.

    The readiness block records the pane shell's ``boot_id`` and ``shell_start_ticks`` alongside its
    pid. That triple is not decoration: :mod:`herdr_run.reap` refuses to call a tab stale on a bare
    pid, so a record without it can never authorise anything, and the reaper would be inert no
    matter how many runs it had to look at. Either field may be ``null`` when ``/proc`` could not be
    read, which the policy reads as UNKNOWN — the safe direction.
    """
    return _write_meta(
        run_id=result.run_id,
        spool=result.spool,
        agent=agent,
        admission=admission,
        config=config,
        target=result.target,
        readiness=result.readiness,
        exit_code=result.exit_code,
        duration_seconds=result.duration_seconds,
    )


def _write_meta(
    *,
    run_id: str,
    spool: SpoolPaths,
    agent: str,
    admission: Admission,
    config: Config,
    target: Target,
    readiness: Readiness,
    exit_code: int | None,
    duration_seconds: float,
) -> str:
    """Write one run record. ``exit_code`` is ``None`` only for a run still running in the pane."""
    path = os.path.join(spool.directory, "meta.json")
    document = {
        "run_id": run_id,
        "agent": agent,
        "argv": list(admission.argv),
        "program": admission.program,
        "prefix": list(admission.prefix),
        "subcommand": admission.subcommand,
        "rendered": admission.rendered,
        "exit_code": exit_code,
        "duration_seconds": format_round3(duration_seconds),
        "workspace": {"label": target.workspace_label, "id": target.workspace_id},
        "tab": {"label": target.tab_label, "id": target.tab_id},
        "pane_id": target.pane_id,
        "created": list(target.created),
        "from_cache": target.from_cache,
        "readiness": {
            "process_idle": readiness.process.idle,
            "process_reason": readiness.process.reason,
            "shell_pid": readiness.process.shell_pid,
            "boot_id": current_boot_id(),
            "shell_start_ticks": process_start_ticks(readiness.process.shell_pid),
            "foreground_pgid": readiness.process.foreground_pgid,
            "prompt_verdict": readiness.prompt.verdict,
            "prompt_reason": readiness.prompt.reason,
            "prompt_tail": readiness.prompt.tail,
        },
        "config_source": config.source_path,
    }
    encoded = (json_text(document, indent=2) + "\n").encode(
        "utf-8"
    )
    try:
        _create_private_file(path, encoded)
    except OSError as exc:
        raise HerdrUnavailable(f"cannot write run metadata {path}: {os_error_text(exc)}") from exc
    return path


def _write_unfinished_meta(
    *,
    run_id: str,
    spool: SpoolPaths,
    agent: str,
    admission: Admission,
    config: Config,
    target: Target,
    readiness: Readiness,
    duration_seconds: float,
) -> None:
    """Record a run that timed out and is STILL RUNNING, with ``exit_code`` null.

    Best effort by construction: a failure to record the run must not replace the timeout the
    caller actually needs to hear about. The reaper reads the null as IN FLIGHT and spares the
    pane; :func:`herdr_run.sweep.load_run_records` later re-reads the spool's ``exit_code`` file,
    so a command that finishes after we stopped waiting stops looking in-flight forever.
    """
    try:
        _write_meta(
            run_id=run_id,
            spool=spool,
            agent=agent,
            admission=admission,
            config=config,
            target=target,
            readiness=readiness,
            exit_code=None,
            duration_seconds=duration_seconds,
        )
    except HerdrRunError:
        return
