"""Detect conflicting local processes before a run starts.

Callers provide conflict matchers; this module supplies process parsing, working-directory
scoping, and ancestor exclusion. Vanished ``/proc`` entries degrade to documented fallback
behavior instead of widening a check silently.
"""

from __future__ import annotations

import os
import shlex
from collections.abc import Collection, Iterator, Sequence
from dataclasses import dataclass
from typing import Protocol, runtime_checkable

from dagrun.procstat import parse_process_stat

__all__ = [
    "command_basename",
    "proc_cwd",
    "path_within",
    "ancestor_pids",
    "ProcessInfo",
    "ConflictMatcher",
    "ExecutableMatcher",
    "SubstringMatcher",
    "is_conflicting_process",
    "iter_conflicts",
]


def command_basename(cmd: str) -> str:
    """Best-effort basename of ``argv[0]`` from a ``ps`` command string.

    Tolerates shell-quoted argv (``'/path/to/tool' test ...``) by trying :func:`shlex.split`
    first and falling back to a whitespace split when the line is not valid shell syntax.
    Returns ``""`` for an empty command. An :class:`ExecutableMatcher` therefore keys on
    the real executable rather than a substring of the whole line.
    """
    try:
        parts = shlex.split(cmd)
    except ValueError:
        parts = cmd.split()
    if not parts:
        return ""
    return os.path.basename(parts[0])


def proc_cwd(pid: str) -> str | None:
    """Resolve a process's working directory via ``/proc/<pid>/cwd`` (Linux).

    Returns the absolute, symlink-resolved cwd, or ``None`` when it cannot be determined —
    the process may have exited mid-scan, the reader may lack permission, or the platform
    may have no ``/proc``. NEVER raises, so a transient PID or a non-Linux host degrades to
    the command-line scope in :meth:`ProcessInfo.within_dir` instead of crashing the check.
    """
    try:
        return os.path.realpath(os.readlink(f"/proc/{pid}/cwd"))
    except (OSError, ValueError):
        return None


def path_within(child: str | None, parent: str | None) -> bool:
    """True if ``child`` is ``parent`` or a descendant of it.

    Compared with a trailing separator so a sibling directory like ``<parent>-other`` does
    NOT match ``<parent>``. Either argument being ``None``/empty yields ``False``.
    """
    if not child or not parent:
        return False
    parent = parent.rstrip(os.sep)
    return child == parent or child.startswith(parent + os.sep)


def ancestor_pids(start_pid: int | None = None) -> set[int]:
    """The set of PIDs from ``start_pid`` (default: this process) up the parent chain to 1.

    Walk ``/proc/<pid>/stat`` following the PPID field. The set is meant to be passed as the
    ``self_ancestors`` argument to :func:`is_conflicting_process`, excluding the launching
    wrapper/shell chain from the scan so a check invoked directly from a driver script does
    not flag its own ancestry. Linux-only; degrades to ``{start_pid}`` when ``/proc`` is
    unavailable.
    """
    pids: set[int] = set()
    pid = os.getpid() if start_pid is None else start_pid
    while pid and pid not in pids:
        pids.add(pid)
        try:
            with open(f"/proc/{pid}/stat", "rb") as handle:
                stat = parse_process_stat(handle.read())
            if stat is None or stat.pid != pid:
                break
            pid = stat.ppid
        except OSError:
            break
    return pids


@dataclass(frozen=True)
class ProcessInfo:
    """One parsed ``ps aux`` row: the fields a matcher needs, computed once.

    ``ps aux`` columns are ``USER PID %CPU %MEM VSZ RSS TTY STAT START TIME COMMAND``; the
    command (column 11) is kept intact by splitting at most 10 times.
    """

    #: The original, unmodified ``ps`` line.
    raw: str
    #: The PID column, kept as a string (as ``ps`` emits it).
    pid: str
    #: The PID parsed to ``int``, or ``None`` when the column was not numeric.
    pid_int: int | None
    #: The full command line (``ps`` COMMAND column, possibly containing spaces).
    cmd: str

    @classmethod
    def parse(cls, ps_line: str) -> "ProcessInfo | None":
        """Parse one ``ps aux`` line, or ``None`` to skip it.

        Returns ``None`` for the ``USER ...`` header row and for any line with fewer than
        the 11 expected columns (a malformed or truncated row), so a caller can treat
        ``None`` uniformly as "nothing to inspect here".
        """
        if ps_line.startswith("USER"):
            return None
        parts = ps_line.split(None, 10)
        if len(parts) < 11:
            return None
        pid = parts[1]
        try:
            pid_int: int | None = int(pid)
        except ValueError:
            pid_int = None
        return cls(raw=ps_line, pid=pid, pid_int=pid_int, cmd=parts[10])

    @property
    def basename(self) -> str:
        """Basename of ``argv[0]`` (see :func:`command_basename`)."""
        return command_basename(self.cmd)

    def cwd(self) -> str | None:
        """This process's real working directory, or ``None`` (see :func:`proc_cwd`)."""
        return proc_cwd(self.pid)

    def within_dir(self, current_dir: str) -> bool:
        """True if this process is operating within ``current_dir``.

        Scoped by the process's REAL working directory (``/proc/<pid>/cwd``) so a tool run
        in a *sibling* checkout — same command tokens, different cwd — is not falsely
        flagged (which would needlessly serialize independent runs). When the real cwd is
        unavailable, fail safe to the command-line substring scope
        (``current_dir in cmd``) — never widened to global, never crashing.
        """
        real_cwd = self.cwd()
        if real_cwd is not None:
            return path_within(real_cwd, os.path.realpath(current_dir))
        return current_dir in self.cmd

    def describe(self, label: str) -> str:
        """Render ``<label> (PID <pid>): <cmd>`` with the command truncated to 100 characters."""
        return f"{label} (PID {self.pid}): {self.cmd[:100]}"


@runtime_checkable
class ConflictMatcher(Protocol):
    """A caller-injected rule deciding whether one process is a conflict.

    Return a human-readable conflict description (typically via
    :meth:`ProcessInfo.describe`) when ``proc`` is a conflict for a run rooted at
    ``current_dir``, or ``None`` when it is not. The caller-supplied matcher catalog is the
    complete policy for what counts as a conflict.
    """

    def __call__(self, proc: ProcessInfo, current_dir: str) -> str | None: ...


def _contains(cmd: str, needle: str, *, case_insensitive: bool) -> bool:
    """Substring test honouring the case-sensitivity flag, in one place (DRY)."""
    if case_insensitive:
        return needle.lower() in cmd.lower()
    return needle in cmd


@dataclass(frozen=True)
class ExecutableMatcher:
    """Flag a process whose ``argv[0]`` basename is ``executable`` and that runs *here*.

    Keying on the executable basename (not a command-line substring) means a monitoring
    shell that merely mentions the tool is not a conflict. ``subcommands`` (when non-empty)
    additionally requires at least one of the
    given tokens to appear in the command line (e.g. ``("test", "build")``). The match is
    scoped to ``current_dir`` by real cwd (:meth:`ProcessInfo.within_dir`).
    """

    executable: str
    label: str
    subcommands: tuple[str, ...] = ()

    def __call__(self, proc: ProcessInfo, current_dir: str) -> str | None:
        if proc.basename != self.executable:
            return None
        if self.subcommands and not any(sub in proc.cmd for sub in self.subcommands):
            return None
        if not proc.within_dir(current_dir):
            return None
        return proc.describe(self.label)


@dataclass(frozen=True)
class SubstringMatcher:
    """Flag a process by required/excluded command-line substrings, optionally dir-scoped.

    A process matches when every string in ``require_all`` is present, at least one string in
    ``require_any`` is present
    (when that tuple is non-empty), and no string in ``exclude_any`` is present. When
    ``require_current_dir`` is set, the command line must additionally contain
    ``current_dir``. Use :class:`ExecutableMatcher` when real-working-directory scoping is
    required. ``exclude_any`` lets a caller carve out launcher or shell processes.
    """

    label: str
    require_all: tuple[str, ...] = ()
    require_any: tuple[str, ...] = ()
    exclude_any: tuple[str, ...] = ()
    require_current_dir: bool = True
    case_insensitive: bool = False

    def __call__(self, proc: ProcessInfo, current_dir: str) -> str | None:
        cmd = proc.cmd
        ci = self.case_insensitive
        if any(not _contains(cmd, req, case_insensitive=ci) for req in self.require_all):
            return None
        if self.require_any and not any(
            _contains(cmd, req, case_insensitive=ci) for req in self.require_any
        ):
            return None
        if any(_contains(cmd, exc, case_insensitive=ci) for exc in self.exclude_any):
            return None
        if self.require_current_dir and current_dir not in cmd:
            return None
        return proc.describe(self.label)


def is_conflicting_process(
    ps_line: str,
    current_dir: str,
    *,
    matchers: Sequence[ConflictMatcher],
    self_ancestors: Collection[int],
) -> tuple[bool, str | None]:
    """Decide whether one ``ps aux`` line is a conflicting process.

    Returns ``(is_conflict, description)``: ``(False, None)`` for the header, a malformed
    row, this run's own ancestor chain, or a process no matcher claims; otherwise
    ``(True, <description>)`` from the first matcher that fires.

    ``matchers`` is the caller-injected conflict catalog (see :class:`ConflictMatcher`).
    ``self_ancestors`` is the PID set from :func:`ancestor_pids` — the launching wrapper /
    shell chain that must never flag itself.
    """
    proc = ProcessInfo.parse(ps_line)
    if proc is None:
        return False, None
    # Skip this run's own ancestry (the driver/make/shell that launched this check).
    if proc.pid_int is not None and proc.pid_int in self_ancestors:
        return False, None
    for matcher in matchers:
        description = matcher(proc, current_dir)
        if description is not None:
            return True, description
    return False, None


def iter_conflicts(
    ps_lines: Sequence[str],
    current_dir: str,
    *,
    matchers: Sequence[ConflictMatcher],
    self_ancestors: Collection[int],
) -> Iterator[str]:
    """Yield the conflict description for every conflicting line in a ``ps aux`` dump.

    Convenience over :func:`is_conflicting_process` for the common whole-scan loop; iterates
    lazily (zero intermediate list) so a caller can short-circuit or count as it goes.
    """
    for ps_line in ps_lines:
        is_conflict, description = is_conflicting_process(
            ps_line,
            current_dir,
            matchers=matchers,
            self_ancestors=self_ancestors,
        )
        if is_conflict and description is not None:
            yield description
