"""Best-effort append-only records of every command submitted to a pane.

A tool that runs commands somewhere the caller's own constraints do not reach must leave a trail
that is complete rather than convenient: REFUSED attempts are logged as prominently as successful
ones, because a run of refusals is exactly the signal worth noticing, and a log that only recorded
successes would make the allowlist's behaviour unobservable after the fact.

JSONL, one object per line, appended with one ``write`` of a line that ends in ``\\n``. Nothing here
locks. Linux ``O_APPEND`` assigns each write its end-of-file offset atomically, but this remains an
operational aid rather than durable or tamper-proof audit storage: writes are not fsynced and a
same-UID process can modify the file.
"""

from __future__ import annotations

import os
import time
from collections.abc import Callable

from herdr_run.client import bounded_control_command
from herdr_run.textfmt import display, json_text, quote

__all__ = [
    "audit_path",
    "make_private_dirs",
    "record",
    "spool_is_ignored",
    "warn_if_spool_is_tracked",
]


def audit_path(project_root: str, spool_dir: str) -> str:
    """Absolute path of the append-only audit log for a project's spool directory."""
    root = (
        spool_dir if os.path.isabs(spool_dir) else os.path.join(project_root, spool_dir)
    )
    return os.path.join(root, "audit.jsonl")


def spool_is_ignored(project_root: str, spool_dir: str) -> bool | None:
    """Is the spool path git-ignored? ``None`` when the question does not apply or cannot be answered.

    Asks git itself rather than parsing ``.gitignore``: the effective ignore state is the product of
    repo, global, and info/exclude rules, and only ``git check-ignore`` resolves all three.
    """
    import subprocess

    root = (
        spool_dir if os.path.isabs(spool_dir) else os.path.join(project_root, spool_dir)
    )
    try:
        completed = bounded_control_command(
            [
                "git",
                "-C",
                project_root,
                "check-ignore",
                "-q",
                "--",
                os.path.join(root, "probe"),
            ],
            timeout=10,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if completed.returncode == 0:
        return True
    if completed.returncode == 1:
        return False
    # rc 128: not a git work tree, or git unusable here. Not a finding, so do not claim one.
    return None


def make_private_dirs(path: str) -> None:
    """Create ``path`` and any missing parents, each with mode 0700, like a recursive ``mkdir``.

    Every directory herdr-run creates for command output is private, not only the last one:
    ``os.makedirs(mode=...)`` applies the mode to the leaf alone. An existing directory is left as
    it is. The algorithm (and so every error) follows the native standard library's recursive
    directory builder, so both editions fail on the same component with the same error.
    """
    if not path:
        return
    try:
        os.mkdir(path, 0o700)
        return
    except FileNotFoundError:
        pass
    except OSError:
        if os.path.isdir(path):
            return
        raise
    stripped = path.rstrip("/")
    if not stripped:
        raise OSError("failed to create whole tree")
    make_private_dirs(os.path.dirname(stripped))
    try:
        os.mkdir(path, 0o700)
    except OSError:
        if os.path.isdir(path):
            return
        raise


def warn_if_spool_is_tracked(
    project_root: str, spool_dir: str, *, stream: object = None
) -> bool:
    """Warn when command output would be written into a tracked part of a source tree.

    The spool holds real stdout/stderr from commands that crossed the sandbox boundary. Writing that
    where ``git add`` can pick it up is how command output ends up committed. A warning rather than a
    refusal: an un-ignored spool is a hygiene defect, not a reason to block the run.
    """
    from herdr_run import output

    if spool_is_ignored(project_root, spool_dir) is not False:
        return False
    message = (
        f"herdr-run: WARNING: spool directory {quote(spool_dir)} is NOT git-ignored in "
        f"{quote(project_root)}. Command output and the audit log will be written into a tracked "
        f"tree. Add '{display(spool_dir.rstrip('/'))}/' to .gitignore."
    )
    if stream is not None:
        print(message, file=stream)  # type: ignore[call-overload]
    else:
        output.diagnostic(message)
    return True


def record(
    path: str,
    *,
    agent: str,
    command: str,
    verdict: str,
    detail: str,
    fields: dict[str, object] | None = None,
    now: Callable[[], float] = time.time,
) -> bool:
    """Append one audit entry, returning success without ever masking the command result."""
    entry: dict[str, object] = {
        "time": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(now())),
        "agent": agent,
        "pid": os.getpid(),
        "command": command,
        "verdict": verdict,
        "detail": detail,
    }
    if fields:
        entry.update(fields)
    try:
        parent = os.path.dirname(path)
        if not parent:
            return False
        make_private_dirs(parent)
        encoded = (json_text(entry) + "\n").encode(
            "utf-8"
        )
        flags = (
            os.O_APPEND
            | os.O_CREAT
            | os.O_WRONLY
            | getattr(os, "O_CLOEXEC", 0)
            | getattr(os, "O_NOFOLLOW", 0)
        )
        descriptor = os.open(path, flags, 0o600)
        try:
            os.fchmod(descriptor, 0o600)
            # One O_APPEND write gives concurrent callers a whole-line placement boundary.
            return os.write(descriptor, encoded) == len(encoded)
        finally:
            os.close(descriptor)
    except (OSError, UnicodeError, ValueError):
        return False
