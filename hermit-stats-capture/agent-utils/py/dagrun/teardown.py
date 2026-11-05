"""Fast teardown for step process trees and complete run scopes.

The module prefers cgroup-wide termination, falls back to process-group signaling when
needed, and includes helpers for removing stale scopes and processes associated with a
working directory. Enforcement degradation is reported visibly.
"""

from __future__ import annotations

import errno
import itertools
import os
import shutil
import signal
import subprocess
import sys
import time
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from pathlib import Path
from types import FrameType
from typing import Protocol, runtime_checkable

from dagrun.procstat import parse_process_stat
from dagrun.protocols import CgroupManager

__all__ = [
    "CGROUP_ROOT",
    "ProcessGroupLeader",
    "ReapSummary",
    "reap",
    "reap_many",
    "install_scope_teardown",
    "kill_cgroup",
    "stop_systemd_scope",
    "current_cgroup_path",
    "outer_scope_cgroup",
    "reap_external",
    "reap_processes_by_cwd",
    "stop_leftover_scopes",
    "STEP_NONCE_ENV",
]

#: cgroup-v2 unified hierarchy mount point. Linux-only.
CGROUP_ROOT = Path("/sys/fs/cgroup")
STEP_NONCE_ENV = "DAGRUN_STEP"

_DESCENDANT_KILL_SWEEPS = 4
_nonce_sequence = itertools.count()
_unboxed_reap_warned = False


def _warn(message: str) -> None:
    """Emit a visible degraded-enforcement warning (No Silent Failure)."""
    print(f"[teardown] ⚠ {message}", file=sys.stderr)


def mint_step_nonce() -> str:
    """Mint a run-unique per-step ownership token normally inherited by descendants."""
    return f"{os.getpid()}:{next(_nonce_sequence)}:{time.time_ns()}"


def _proc_entry_disappeared(exc: OSError) -> bool:
    """Whether a procfs read failed because the snapshotted PID vanished.

    Only the kernel's explicit disappearance errors are safe to omit.  Treating
    permission, I/O, or malformed-path failures as disappearance would turn an
    incomplete process scan into a false proof that teardown succeeded.
    """
    return exc.errno in (errno.ENOENT, errno.ESRCH)


def _proc_descendants(
    root: int, *, proc_root: Path = Path("/proc")
) -> tuple[list[int], bool]:
    """Return descendants deepest-first and whether the procfs scan was complete.

    A PID disappearing after ``listdir`` is a benign race.  Every other read or
    parse failure makes the result incomplete: the unreadable record could be a
    descendant, so callers must not interpret its omission as an empty tree.
    """
    children: dict[int, list[int]] = {}
    try:
        entries = os.listdir(proc_root)
    except OSError:
        return [], False
    complete = True
    for entry in entries:
        if not entry.isdigit():
            continue
        pid = int(entry)
        try:
            stat = parse_process_stat((proc_root / str(pid) / "stat").read_bytes())
        except OSError as exc:
            if not _proc_entry_disappeared(exc):
                complete = False
            continue
        if stat is None or stat.pid != pid:
            complete = False
            continue
        children.setdefault(stat.ppid, []).append(pid)
    out: list[int] = []
    stack = [root]
    seen: set[int] = set()
    while stack:
        parent = stack.pop()
        for child in children.get(parent, ()):
            if child not in seen:
                seen.add(child)
                out.append(child)
                stack.append(child)
    out.reverse()
    return out, complete


def _root_can_have_descendants(root: int, *, proc_root: Path = Path("/proc")) -> bool:
    """Whether a parentage sweep from ``root`` can find anything at all.

    Linux reparents a process's children when it exits, before it becomes a
    zombie, so a root that is gone, or a zombie with no thread left, has no
    children: the sweep would read every record in the process table and find
    nothing.  That walk is the dominant cost of tearing down a finished step on
    a busy host, and it ran on every reap, including the one after the step's
    own exit.

    A zombie is not always an exited process: a thread-group leader that called
    ``pthread_exit`` shows as a zombie while its other threads run, and its
    children stay attached to them, so a zombie keeps the sweep while its task
    directory lists any other thread.  A root that is still running, or whose
    record or threads cannot be read, keeps the sweep.
    """
    try:
        stat = parse_process_stat((proc_root / str(root) / "stat").read_bytes())
    except OSError as exc:
        return not _proc_entry_disappeared(exc)
    if stat is None or stat.state not in ("Z", "X", "x"):
        return True
    try:
        return len(os.listdir(proc_root / str(root) / "task")) > 1
    except OSError:
        return True


def _kill_descendants(root: int) -> int:
    """SIGKILL descendants and require a complete empty scan before success.

    Forking races require repeated snapshots.  An incomplete or non-empty final
    snapshot is not evidence that the tree is gone, so the bounded fallback
    reports that degradation visibly instead of silently declaring success.
    """
    if root <= 1:
        return 0
    killed: set[int] = set()
    confirmed_empty = False
    for _ in range(_DESCENDANT_KILL_SWEEPS):
        descendants, complete = _proc_descendants(root)
        if complete and not descendants:
            confirmed_empty = True
            break
        for pid in descendants:
            if pid <= 1 or pid == os.getpid():
                continue
            killed.add(pid)
            try:
                os.kill(pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError, OSError):
                pass
    if not confirmed_empty:
        _warn(
            f"could not confirm an empty descendant tree for process group {root} "
            f"after {_DESCENDANT_KILL_SWEEPS} procfs sweep(s)"
        )
    return len(killed)


def _live_process_group_from_stat(
    stat: bytes | str, *, expected_pid: int | None = None
) -> int | None:
    """Return a live process's group from one Linux ``/proc/PID/stat`` record.

    The parenthesized command may itself contain spaces and parentheses, so fields must be
    interpreted only after the final ``)``.  Zombies deliberately return ``None``: their group
    leader cannot be wait-reaped until the supervisor regains control, and treating that leader as
    live would spend the whole diagnostic grace after a cooperative SIGTERM exit.
    """
    parsed = parse_process_stat(stat)
    return (
        None
        if parsed is None
        or parsed.state == "Z"
        or (expected_pid is not None and parsed.pid != expected_pid)
        else parsed.pgrp
    )


def _live_process_groups(
    pgids: set[int], *, proc_root: Path = Path("/proc")
) -> set[int] | None:
    """Return requested groups containing a non-zombie process.

    ``killpg(pgid, 0)`` reports success while an unreaped group leader is a zombie. A supervisor
    cannot reap that child until teardown returns, so using the signal probe makes every cooperative
    SIGTERM exit consume the entire grace. `/proc/<pid>/stat` exposes both state and process-group id;
    one walk handles a whole cancellation batch and deliberately excludes zombies.

    ``None`` means the procfs snapshot was incomplete. Callers then retain the
    conservative signal-probe fallback rather than assuming an unreadable or
    malformed process disappeared.
    """
    if not pgids:
        return set()
    try:
        entries = os.listdir(proc_root)
    except OSError:
        return None
    live: set[int] = set()
    for entry in entries:
        if not entry.isdigit():
            continue
        try:
            stat = (proc_root / entry / "stat").read_bytes()
        except OSError as exc:
            if not _proc_entry_disappeared(exc):
                return None
            continue
        parsed = parse_process_stat(stat)
        if parsed is None or parsed.pid != int(entry):
            return None
        if parsed.state == "Z":
            continue
        if parsed.pgrp in pgids:
            live.add(parsed.pgrp)
    return live


def _kill_by_nonce(nonce: str) -> int:
    """SIGKILL processes carrying one exact runner-minted environment token.

    This is a best-effort closer for environment-preserving escapees, not a security boundary: a
    hostile child can scrub its environment. Cgroup containment is the only robust whole-subtree
    primitive used by the runner.
    """
    if not nonce:
        return 0
    needle = f"{STEP_NONCE_ENV}={nonce}".encode()
    killed: set[int] = set()
    for _ in range(_DESCENDANT_KILL_SWEEPS):
        fresh = 0
        try:
            entries = os.listdir("/proc")
        except OSError:
            break
        for entry in entries:
            if not entry.isdigit():
                continue
            pid = int(entry)
            if pid <= 1 or pid == os.getpid():
                continue
            try:
                with open(f"/proc/{pid}/environ", "rb") as handle:
                    carries = needle in handle.read().split(b"\0")
            except OSError:
                continue
            if not carries:
                continue
            if pid not in killed:
                killed.add(pid)
                fresh += 1
            try:
                os.kill(pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError, OSError):
                pass
        if fresh == 0:
            break
    return len(killed)


@runtime_checkable
class ProcessGroupLeader(Protocol):
    """The one thing :func:`reap` needs from a launched step: its process-group id.

    A :class:`subprocess.Popen` satisfies this structurally. The caller MUST have started
    the process with ``start_new_session=True`` so the leader's ``pid`` equals the
    process-group id (``pgid``); that group id stays valid for ``killpg`` while any member
    is alive, even after the leader itself has been ``wait()``-reaped. Reading the stored
    ``pid`` therefore remains safe after leader exit.
    """

    pid: int


# --------------------------------------------------------------------------------------
# Single-step teardown  (ports validate.py Runner._reap)
# --------------------------------------------------------------------------------------


#: Grace between the SIGTERM that lets a step SAY what it was doing and the
#: SIGKILL that guarantees it stops. See :func:`reap` for why this exists.
REAP_TERM_GRACE_S = 5.0
_REAP_POLL_S = 0.1


def reap(
    process: ProcessGroupLeader,
    cgroups: CgroupManager | None,
    tag: str | None,
    *,
    term_grace_s: float = REAP_TERM_GRACE_S,
    nonce: str | None = None,
) -> None:
    """Tear down one step's tree: SIGTERM, a grace, then ``cgroup.kill``/``killpg``.

    WHY THE SIGTERM PHASE EXISTS -- it is what makes a timeout ATTRIBUTABLE.

    A step that FAILS reports itself. A step KILLED by a timeout does not: the
    process dies mid-run, so its output must ALREADY have named what was in
    flight or that information does not exist afterwards. It cannot be recovered
    by parsing logs later, because there is no output to parse.

    Many inner runners handle SIGTERM by cancelling and naming the unit that was
    still executing; on SIGKILL they cannot, and the log simply stops. Measured
    both ways against one such runner, same timeout and same slow unit, with the
    signal as the only variable: under SIGKILL the in-flight unit was named zero
    times, and the log ended at the start-of-run banner; under SIGTERM the runner
    printed a cancellation line naming the exact unit id. So the identity was
    always available -- this function used to destroy it by going straight to
    SIGKILL. The fix is not more logging; it is giving existing logging a chance
    to run.

    THE HARD PHASE IS UNCHANGED AND STILL GUARANTEED. After the grace this does
    exactly what it always did -- ``cgroup.kill`` first, then ``killpg`` -- so a
    genuinely wedged tree still cannot occupy a runner. The grace only bounds how
    long we wait for a cooperative exit; a step that ignores SIGTERM is killed on
    schedule, and one that honours it is reaped sooner than before.

    When per-step containment is available, writing the step's child ``cgroup.kill``
    SIGKILLs the ENTIRE subtree atomically, including ``setsid`` / double-fork escapees a
    process-group kill misses (an escapee changes session/pgid but not cgroup membership).
    The ``killpg`` that follows is a belt-and-suspenders for the no-cgroup path and is
    harmless once the cgroup already cleared the group.

    ``process.pid`` is used as the pgid (valid because the caller started the step with
    ``start_new_session=True``); the guard refuses ``pgid <= 1`` and the runner's OWN
    process group so a reap can never signal the runner itself.

    When enabled containment cannot perform ``cgroup.kill``, a warning makes the degraded
    teardown visible before the process-group fallback runs.
    """
    reap_many(((process, tag, nonce),), cgroups, term_grace_s=term_grace_s)


def _hard_reap(
    process: ProcessGroupLeader,
    cgroups: CgroupManager | None,
    tag: str | None,
    nonce: str | None,
) -> None:
    """Perform the non-graceful half of one reap after any shared TERM window."""
    pgid = process.pid
    signalable = pgid > 1 and pgid != os.getpgrp()
    contained = False
    if cgroups is not None and tag is not None and cgroups.enabled:
        contained = cgroups.kill(tag)
        if not contained:
            _warn(
                f"cgroup.kill for step {tag!r} failed; falling back to process-group "
                "kill plus /proc ownership sweeps."
            )
    else:
        global _unboxed_reap_warned
        if not _unboxed_reap_warned:
            _unboxed_reap_warned = True
            _warn(
                "steps are unboxed; teardown uses process-group and /proc ownership sweeps "
                "instead of atomic cgroup.kill"
            )

    if signalable:
        try:
            os.killpg(pgid, signal.SIGKILL)
        except (ProcessLookupError, OSError):
            pass  # whole group already gone — an expected, benign race, not a degraded skip

    if not contained and _root_can_have_descendants(pgid):
        swept = _kill_descendants(pgid)
        if swept:
            _warn(f"step {tag!r}: killed {swept} descendant(s) outside its process group")
    if nonce:
        swept = _kill_by_nonce(nonce)
        if swept:
            _warn(
                f"step {tag!r}: killed {swept} process(es) by ownership nonce "
                "(an environment-preserving setsid/double-fork escapee)"
            )


def reap_many(
    processes: Sequence[tuple[ProcessGroupLeader, str | None, str | None]],
    cgroups: CgroupManager | None,
    *,
    term_grace_s: float = REAP_TERM_GRACE_S,
) -> None:
    """Tear down several steps with ONE shared SIGTERM window, then hard-reap each.

    Eager cancellation and whole-run timeout used to call :func:`reap` serially, granting every
    resistant process group its own five-second grace. Enough in-flight steps could therefore carry
    the internal run timeout past the outer systemd cushion and lose the evidence it was designed to
    preserve. Signal every group first and charge the grace once for the entire cancellation batch.

    The liveness walk ignores zombies. A child that honored SIGTERM cannot be ``wait()``-reaped until
    its supervisor regains control; treating that zombie as live made even a cooperative exit consume
    the full grace.
    """
    items = tuple(processes)
    own_group = os.getpgrp()
    active: set[int] = set()
    if term_grace_s > 0:
        for process, _tag, _nonce in items:
            pgid = process.pid
            if pgid <= 1 or pgid == own_group:
                continue
            try:
                os.killpg(pgid, signal.SIGTERM)
            except (ProcessLookupError, OSError):
                continue
            active.add(pgid)

    deadline = time.monotonic() + max(0.0, term_grace_s)
    while active and time.monotonic() < deadline:
        observed = _live_process_groups(active)
        if observed is None:
            observed = set()
            for pgid in active:
                try:
                    os.killpg(pgid, 0)
                except (ProcessLookupError, OSError):
                    continue
                observed.add(pgid)
        active = observed
        if active:
            time.sleep(min(_REAP_POLL_S, max(0.0, deadline - time.monotonic())))

    for process, tag, nonce in items:
        _hard_reap(process, cgroups, tag, nonce)


# --------------------------------------------------------------------------------------
# Outer-scope cgroup helpers  (port validate_cgroup.kill_scope_cgroup / stop_scope)
# --------------------------------------------------------------------------------------


def kill_cgroup(cgroup: Path) -> bool:
    """Atomically SIGKILL a cgroup and every descendant via its ``cgroup.kill`` file.

    Ports ``validate_cgroup.kill_scope_cgroup``. Returns ``True`` when the write landed,
    ``False`` on ``OSError`` (e.g. the file is gone / not writable). Best-effort; never
    raises — the caller decides whether a ``False`` here is worth a warning.
    """
    try:
        (cgroup / "cgroup.kill").write_text("1")
        return True
    except OSError:
        return False


def stop_systemd_scope(unit: str, scope_cgroup: Path | None = None, *, timeout: int = 15) -> bool:
    """Tear down a whole ``systemd --user`` transient scope: ``cgroup.kill`` then stop.

    Ports ``validate_cgroup.stop_scope``. Two steps for speed AND cleanliness:

    1. ``cgroup.kill`` the scope's cgroup directly — an INSTANT atomic SIGKILL of every
       member, including processes (e.g. a browser) that IGNORE the SIGTERM that
       ``systemctl stop`` would send first and would otherwise stall the stop in
       ``stop-sigterm`` for the unit's full ``TimeoutStopSec``.
    2. ``systemctl --user stop`` to deactivate and garbage-collect the now-empty unit.

    Pass ``scope_cgroup`` (resolved ahead of time via :func:`outer_scope_cgroup`) to skip
    the ``systemctl show`` lookup for step 1 — important inside a signal handler, where
    shelling out under load can stall. Returns ``True`` iff the ``systemctl stop`` call
    completed. Best-effort throughout; never raises.
    """
    if not unit:
        return False
    cg = scope_cgroup if scope_cgroup is not None else _systemd_scope_cgroup(unit)
    if cg is not None:
        kill_cgroup(cg)
    try:
        subprocess.run(
            ["systemctl", "--user", "stop", unit],
            capture_output=True,
            timeout=timeout,
        )
        return True
    except (subprocess.TimeoutExpired, OSError):
        return False


def _systemd_scope_cgroup(unit: str) -> Path | None:
    """Filesystem cgroup path of a ``systemd --user`` unit, via ``systemctl show``.

    Ports ``validate_cgroup._scope_cgroup_path`` / ``kill_zombie_processes._scope_cgroup_path``.
    """
    try:
        r = subprocess.run(
            ["systemctl", "--user", "show", unit, "--property=ControlGroup", "--value"],
            capture_output=True,
            text=True,
            timeout=8,
        )
    except (subprocess.TimeoutExpired, OSError):
        return None
    cg = r.stdout.strip()
    return (CGROUP_ROOT / cg.lstrip("/")) if cg else None


def current_cgroup_path() -> Path | None:
    """This process's own cgroup-v2 path from ``/proc/self/cgroup`` (unified line ``0::``).

    Ports the ``/proc/<pid>/cgroup`` parse in ``kill_zombie_processes._process_cgroup_path``
    for ``self``. Reads with no ``systemctl`` shell-out, so it is safe to call before
    installing a signal handler (which must not shell out under load).
    """
    try:
        for line in Path("/proc/self/cgroup").read_text().splitlines():
            hierarchy, controllers, relative = line.split(":", 2)
            if hierarchy == "0" and controllers == "":
                return CGROUP_ROOT / relative.lstrip("/")
    except (OSError, ValueError):
        pass
    return None


def outer_scope_cgroup(*, supervisor_name: str = "supervisor") -> Path | None:
    """Resolve the OUTER run scope's cgroup from this process's own cgroup, no shell-out.

    When the runner lives in a ``<scope>/<supervisor_name>`` child, the scope is the parent; if this
    process is already at a ``*.scope`` leaf, that leaf is the scope. Returns ``None`` when the
    process is not inside a recognizable scope.
    """
    mine = current_cgroup_path()
    if mine is None:
        return None
    if mine.name == supervisor_name:
        return mine.parent
    if mine.name.endswith(".scope"):
        return mine
    return None


# --------------------------------------------------------------------------------------
# Signal-handler installer
# --------------------------------------------------------------------------------------


def install_scope_teardown(
    *,
    scope_cgroup: Path | None = None,
    systemd_unit: str | None = None,
    on_teardown: Callable[[], None] | None = None,
    signals: Sequence[int] = (signal.SIGINT, signal.SIGTERM),
) -> bool:
    """Install a SIGINT/SIGTERM handler that tears down the WHOLE outer scope, then exits.

    Killing only the runner process can leave ``setsid``-escapee orphans (servers or browsers)
    alive in the scope cgroup because killpg and systemd ``--collect`` can miss them. On signal,
    the handler instead SIGKILLs the entire scope subtree atomically.

    Provide EITHER a ``systemd_unit`` (a ``systemd --user`` transient scope, torn down with
    :func:`stop_systemd_scope`) OR a ``scope_cgroup`` path (a directly-delegated cgroup,
    torn down with :func:`kill_cgroup`). ``scope_cgroup`` doubles as the pre-resolved path
    that lets the systemd path skip the in-handler ``systemctl show`` (resolve it once now
    with :func:`outer_scope_cgroup`, not inside the handler). ``on_teardown`` runs first —
    use it to release a lock the normal ``finally`` block will no longer reach once the
    scope teardown SIGKILLs this process.

    The handler prints its reason, tears down, then ``os._exit(128 + signum)`` so an aborted
    run exits with the conventional signal code. This is intentionally the SIGNAL path only;
    the NORMAL-exit backstop belongs elsewhere (see
    :meth:`CgroupManager.kill_all_remaining`, which does NOT stop the scope and so preserves
    a successful run's exit code).

    Returns ``True`` when a handler was installed. Returns ``False`` (No Silent Failure: a
    visible, inspectable signal to the caller) when neither a unit nor a scope cgroup was
    given — there is simply nothing to tear down, e.g. a run with no cgroup delegation.
    """
    if systemd_unit is None and scope_cgroup is None:
        return False

    def _on_signal(signum: int, _frame: FrameType | None) -> None:
        scope_name = systemd_unit or str(scope_cgroup)
        try:
            sys.stderr.write(
                f"\n[teardown] signal {signum} — tearing down scope {scope_name} "
                "(kills all steps + orphans)…\n"
            )
            sys.stderr.flush()
        except OSError:
            pass
        if on_teardown is not None:
            try:
                on_teardown()
            except Exception as exc:  # a teardown callback must never eat the exit path
                _warn(f"on_teardown callback raised during signal {signum}: {exc!r}")
        torn_down = False
        if systemd_unit is not None:
            torn_down = stop_systemd_scope(systemd_unit, scope_cgroup)
        elif scope_cgroup is not None:
            torn_down = kill_cgroup(scope_cgroup)
        if not torn_down:
            _warn(
                f"scope teardown of {scope_name} did not confirm success; orphaned "
                "processes may remain in the scope cgroup."
            )
        # If teardown somehow did not already kill us, exit with the signal code.
        os._exit(128 + signum)

    installed = False
    for sig in signals:
        try:
            signal.signal(sig, _on_signal)
            installed = True
        except (ValueError, OSError) as exc:
            _warn(f"could not install teardown handler for signal {sig}: {exc!r}")
    return installed


# --------------------------------------------------------------------------------------
# External reaper  (ports kill_zombie_processes.py, parameterized by cwd + patterns)
# --------------------------------------------------------------------------------------


@dataclass(frozen=True)
class ReapSummary:
    """Outcome of an external reap pass: which PIDs were killed / failed, scopes stopped."""

    killed: tuple[int, ...] = ()
    failed: tuple[int, ...] = ()
    scopes_stopped: int = 0

    @property
    def ok(self) -> bool:
        """True when nothing that was targeted failed to die."""
        return not self.failed


def _running_processes() -> list[str]:
    """Lines of ``ps aux`` (PID + full command), or ``[]`` with a visible warning on error.

    Ports ``kill_zombie_processes.get_processes``.
    """
    try:
        result = subprocess.run(
            ["ps", "aux"], capture_output=True, text=True, timeout=10
        )
        return result.stdout.splitlines()
    except (subprocess.TimeoutExpired, OSError) as exc:
        _warn(f"could not read process list (ps aux): {exc!r}")
        return []


def _proc_cwd(pid: int) -> str | None:
    """Resolved working directory of a PID via ``/proc/<pid>/cwd``, or ``None``."""
    try:
        return os.readlink(f"/proc/{pid}/cwd")
    except OSError:
        return None


def _cmdline_targets_cwd(cmd: str, pid: int, cwd: str) -> bool:
    """Does a process belong to ``cwd``? By its command line OR its actual ``/proc`` cwd.

    The originating implementation matches the working directory as a substring of the command line
    (``current_dir in cmd``); we additionally check the real ``/proc/<pid>/cwd`` so a
    process launched with a relative path (whose cwd never appears in ``argv``) is still
    correctly attributed — the same robustness the scope reaper already relied on.
    """
    if cwd in cmd:
        return True
    resolved = _proc_cwd(pid)
    return resolved is not None and (resolved == cwd or resolved.startswith(cwd + os.sep))


def kill_process(pid: int, description: str, *, term_grace_s: float = 0.5) -> bool:
    """SIGTERM a PID, wait a grace period, then SIGKILL if it is still alive.

    Ports ``kill_zombie_processes.kill_process``. Returns ``True`` when the process existed
    and was signalled, ``False`` when it was already dead, permission was denied, or another
    OS error occurred (each reported visibly rather than swallowed).
    """
    print(f"  Killing PID {pid}: {description}")
    try:
        os.kill(pid, signal.SIGTERM)
        time.sleep(term_grace_s)
        try:
            os.kill(pid, 0)  # signal 0 only probes existence
            os.kill(pid, signal.SIGKILL)
            print("    (used SIGKILL)")
        except ProcessLookupError:
            pass  # exited during the grace period
        return True
    except ProcessLookupError:
        print("    (already dead)")
        return False
    except PermissionError:
        _warn(f"permission denied killing PID {pid} ({description})")
        return False
    except OSError as exc:
        _warn(f"error killing PID {pid} ({description}): {exc!r}")
        return False


def reap_processes_by_cwd(
    cwd: str,
    patterns: Sequence[str],
    *,
    protect: Callable[[int], bool] | None = None,
    dry_run: bool = False,
) -> tuple[list[int], list[int]]:
    """Kill leftover processes that (a) match a command-line ``pattern`` and (b) belong to
    ``cwd``, sparing this process and any PID a caller-supplied ``protect`` predicate keeps.

    Command patterns and the ``protect`` predicate are caller-supplied. A protection rule
    should retain every PID belonging to a live run so in-progress work is never reaped.

    Returns ``(killed_pids, failed_pids)``. With ``dry_run=True`` nothing is signalled and
    the would-be-killed PIDs are returned in ``killed_pids``.
    """
    own_pid = os.getpid()
    killed: list[int] = []
    failed: list[int] = []
    for line in _running_processes():
        if line.startswith("USER"):  # ps header
            continue
        parts = line.split(None, 10)
        if len(parts) < 11:
            continue
        try:
            pid = int(parts[1])
        except ValueError:
            continue
        cmd = parts[10]
        if pid == own_pid:
            continue
        if not any(pattern in cmd for pattern in patterns):
            continue
        if not _cmdline_targets_cwd(cmd, pid, cwd):
            continue
        if protect is not None and protect(pid):
            continue
        description = cmd[:80]
        if dry_run:
            print(f"  [dry-run] would kill PID {pid}: {description}")
            killed.append(pid)
            continue
        (killed if kill_process(pid, description) else failed).append(pid)
    return killed, failed


def stop_leftover_scopes(
    cwd: str,
    *,
    scope_glob: str,
    is_live: Callable[[Path], bool] | None = None,
) -> int:
    """Stop leftover ``systemd --user`` transient scopes that belong to ``cwd``.

    A ``systemctl stop`` kills a scope's whole cgroup, including ``setsid`` escapees that a per-PID
    scan misses. A scope is stopped only when one of its processes has a
    ``/proc/<pid>/cwd`` within ``cwd`` (walked RECURSIVELY, since cgroup-v2 ``cgroup.procs``
    is per-cgroup and a live orphan may sit in a per-step child cgroup while the scope's own
    ``cgroup.procs`` is empty); a concurrent scope rooted elsewhere is left untouched.

    ``scope_glob`` selects candidate units (e.g. ``"validate-*.scope"``). ``is_live``, when
    given, spares a scope the caller still considers a live run (e.g. its supervisor PID is
    alive) even if it belongs to ``cwd``. Returns the count of scopes stopped.
    """
    if not shutil.which("systemctl"):
        return 0
    try:
        r = subprocess.run(
            ["systemctl", "--user", "list-units", "--type=scope", "--no-legend",
             "--plain", scope_glob],
            capture_output=True,
            text=True,
            timeout=8,
        )
    except (subprocess.TimeoutExpired, OSError) as exc:
        _warn(f"could not list systemd scopes ({scope_glob!r}): {exc!r}")
        return 0

    stopped = 0
    for line in r.stdout.splitlines():
        parts = line.split()
        if not parts or not parts[0].endswith(".scope"):
            continue
        unit = parts[0]
        cg = _systemd_scope_cgroup(unit)
        if cg is None or not cg.exists():
            continue
        if is_live is not None and is_live(cg):
            print(f"  Preserved live scope: {unit}")
            continue
        if not _scope_belongs_to_cwd(cg, cwd):
            continue  # cross-checkout (or empty) scope — DO NOT touch
        # cgroup.kill FIRST for an instant atomic SIGKILL of the whole subtree (a browser
        # ignores the SIGTERM `systemctl stop` sends first); then stop GCs the empty unit.
        if not kill_cgroup(cg):
            _warn(f"cgroup.kill of leftover scope {unit} failed; relying on systemctl stop only.")
        try:
            subprocess.run(["systemctl", "--user", "stop", unit],
                           capture_output=True, timeout=8)
            print(f"  Stopped leftover scope (this checkout): {unit}")
            stopped += 1
        except (subprocess.TimeoutExpired, OSError) as exc:
            _warn(f"failed to stop leftover scope {unit}: {exc!r}")
    return stopped


def _scope_belongs_to_cwd(cgroup: Path, cwd: str) -> bool:
    """True when any process anywhere in a scope's cgroup subtree has its cwd within ``cwd``.

    Ports the recursive ``cgroup.procs`` / ``/proc/<pid>/cwd`` walk in
    ``kill_zombie_processes.stop_my_validate_scopes``.
    """
    prefix = cwd + os.sep
    try:
        procs_files = list(cgroup.rglob("cgroup.procs"))
    except OSError:
        return False
    for procs in procs_files:
        try:
            pids = procs.read_text().split()
        except OSError:
            continue
        for pid in pids:
            resolved = _proc_cwd(int(pid)) if pid.isdigit() else None
            if resolved is not None and (resolved == cwd or resolved.startswith(prefix)):
                return True
    return False


def reap_external(
    cwd: str,
    patterns: Sequence[str],
    *,
    scope_glob: str | None = None,
    is_live: Callable[[Path], bool] | None = None,
    protect: Callable[[int], bool] | None = None,
    dry_run: bool = False,
) -> ReapSummary:
    """One-call external reap: leftover PIDs by cwd + leftover systemd scopes by cwd.

    Composes :func:`reap_processes_by_cwd` with :func:`stop_leftover_scopes`. Scope reaping
    runs only when ``scope_glob`` is provided and ``dry_run`` is off.
    """
    killed, failed = reap_processes_by_cwd(cwd, patterns, protect=protect, dry_run=dry_run)
    scopes_stopped = 0
    if scope_glob is not None and not dry_run:
        scopes_stopped = stop_leftover_scopes(cwd, scope_glob=scope_glob, is_live=is_live)
    return ReapSummary(
        killed=tuple(killed),
        failed=tuple(failed),
        scopes_stopped=scopes_stopped,
    )
