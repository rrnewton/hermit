"""Account-global coordination state for Herdr session and pane ownership.

Herdr workspace, tab, and pane identifiers belong to the operating-system account, not to one
project checkout. Locks protecting those objects therefore live below the account home recorded
by the OS account database. Caller-controlled ``HOME`` and ``XDG_*`` variables are deliberately
ignored. Messages are worded exactly as the native edition words them.
"""

from __future__ import annotations

import os
from typing import BinaryIO

from herdr_run.client import account_home
from herdr_run.errors import HerdrUnavailable
from herdr_run.textfmt import os_error_text

__all__ = ["account_state_root", "open_lock_file", "pane_lock_path", "session_lock_path"]


def _account_home() -> str:
    """The account database's home directory (a seam the tests redirect)."""
    return account_home()


def _state_root_for_home(home: str) -> str:
    if not os.path.isabs(home):
        raise HerdrUnavailable(f"the current account home is not absolute: {home}")
    return os.path.join(home, ".local", "state", "herdr-run")


def account_state_root() -> str:
    """Return the deterministic per-account root; this does not consult the environment."""
    return _state_root_for_home(_account_home())


def _ensure_private_directory(path: str) -> None:
    try:
        os.makedirs(path, exist_ok=True)
    except (OSError, ValueError) as exc:
        raise HerdrUnavailable(
            f"cannot prepare private herdr-run state directory {path}: {os_error_text(exc)}"
        ) from exc
    try:
        # Tighten a pre-existing directory as well as accounting for a permissive process umask.
        os.chmod(path, 0o700)
    except (OSError, ValueError) as exc:
        raise HerdrUnavailable(
            f"cannot set private mode on herdr-run state directory {path}: {os_error_text(exc)}"
        ) from exc


def _lock_path(*parts: str) -> str:
    root = account_state_root()
    _ensure_private_directory(root)
    directory = os.path.join(root, "locks")
    _ensure_private_directory(directory)
    for part in parts[:-1]:
        directory = os.path.join(directory, part)
        _ensure_private_directory(directory)
    return os.path.join(directory, parts[-1])


def session_lock_path() -> str:
    """Return the one account-global session-resolution lock path."""
    return _lock_path("session-resolve.lock")


def pane_lock_path(pane_id: str) -> str:
    """Return the account-global lock path for one opaque Herdr pane identifier."""
    import hashlib

    digest = hashlib.sha256(pane_id.encode("utf-8")).hexdigest()
    return _lock_path("panes", f"{digest}.lock")


def open_lock_file(path: str) -> BinaryIO:
    """Open one advisory-lock inode with private mode and without following a final symlink."""
    flags = os.O_CREAT | os.O_RDWR
    flags |= getattr(os, "O_CLOEXEC", 0)
    flags |= getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags, 0o600)
    except (OSError, ValueError) as exc:
        raise HerdrUnavailable(
            f"cannot open private herdr-run lock {path}: {os_error_text(exc)}"
        ) from exc
    try:
        os.fchmod(descriptor, 0o600)
    except (OSError, ValueError) as exc:
        os.close(descriptor)
        raise HerdrUnavailable(
            f"cannot set private mode on herdr-run lock {path}: {os_error_text(exc)}"
        ) from exc
    return os.fdopen(descriptor, "r+b")
