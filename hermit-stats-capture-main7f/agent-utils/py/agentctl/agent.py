"""Durable, serialized messaging for interactive agents hosted by Herdr.

Target identity is supplied by adapters.  This module owns every transport
property: durable FIFO files, idle/done readiness, atomic multiline submission,
working-state confirmation, at-most-once ambiguity quarantine, status, and reading.
"""

from __future__ import annotations

import fcntl
import hashlib
import json
import math
import os
import re
import stat
import tempfile
import time
from collections.abc import Callable, Iterator, Sequence
from contextlib import contextmanager
from contextvars import ContextVar
from dataclasses import dataclass

from agentctl.client import AgentPaneInfo, HerdrClient
from agentctl.errors import (
    AgentDeliveryError,
    AgentPending,
    AgentPossiblySubmitted,
    HerdrUnavailable,
    MisrouteRecovered,
    ProbableMisroute,
)
from agentctl.prompt_time import retime as _retime_prompt
from agentctl.submission import PromptNotStaged, SubmissionReceipt

__all__ = [
    "Target", "QueueResult", "resolve_target", "enqueue", "drain", "send", "status", "read",
    "QUEUE_ERROR_MAX_BYTES", "QUEUE_UPDATE_MAX_BYTES", "QUEUE_ERROR_SIDECAR_MAX_BYTES",
    "queue_artifact_reservation_bytes",
]


@dataclass(frozen=True)
class Target:
    """Identity assertions for one already-running interactive Herdr agent."""

    pane_id: str | None = None
    session_agent: str | None = None
    session_value: str | None = None
    expected_agent: str | None = None
    expected_workspace: str | None = None
    expected_cwd: str | None = None


@dataclass(frozen=True)
class AtomicWritePolicy:
    """Opt-in, same-filesystem single-slot staging for a bounded state owner."""

    directory: str
    max_bytes: int


_ACTIVE_ATOMIC_POLICY: ContextVar[AtomicWritePolicy | None] = ContextVar(
    "agentctl_atomic_write_policy", default=None)


@contextmanager
def atomic_write_policy(policy: AtomicWritePolicy | None) -> Iterator[None]:
    """Apply one optional bounded atomic-staging policy within this context."""
    if policy is not None:
        # Pin relative policies to the entry cwd once, before any lock, mkdir,
        # or parent-directory fsync derives a path from the staging directory.
        policy = AtomicWritePolicy(os.path.abspath(policy.directory), policy.max_bytes)
    token = _ACTIVE_ATOMIC_POLICY.set(policy)
    try:
        yield
    finally:
        _ACTIVE_ATOMIC_POLICY.reset(token)


@dataclass(frozen=True)
class QueueResult:
    """Structured outcome of one durable queue send or drain operation."""

    message_id: str
    delivered: tuple[str, ...]
    quarantined: tuple[str, ...]
    pending: tuple[str, ...]
    blocked: str | None = None
    outcome: str = "delivered"


class _PossiblySubmitted(AgentDeliveryError):
    """The atomic pane injection happened, but its working transition was not observed."""

    def __init__(self, message: str, *, misroute: bool = False) -> None:
        super().__init__(message)
        #: The pane failed its recipient check right after input was written.
        self.misroute = misroute


class _Misrouted(AgentDeliveryError):
    """The prompt reached the wrong program, which was told to ignore it; retry is safe."""


#: Misroutes tolerated for one message before it is quarantined instead of retried.
MAX_MISROUTES = 2


class _NotStaged(AgentDeliveryError):
    """Nothing was typed into the pane, so the prompt remains safe to retry."""


class _ArtifactTooLarge(AgentDeliveryError):
    """A configured serialized-artifact limit would be exceeded."""


_MESSAGE_ID = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,254}\Z")
_U64_MAX = (1 << 64) - 1
# Bound diagnostic text before JSON escaping. ASCII control characters can require
# six serialized bytes per UTF-8 byte, which the reservation envelopes include.
QUEUE_ERROR_MAX_BYTES = 2000
_LONGEST_JSON_FLOAT = -1.7976931348623157e308


def _json_text(document: dict[str, object], *, allow_nan: bool = True) -> str:
    return json.dumps(document, indent=2, sort_keys=True, allow_nan=allow_nan) + "\n"


_UPDATE_ENVELOPE: dict[str, object] = {
    "delivery_attempts": _U64_MAX + 1,
    "tui_delivery_attempts": _U64_MAX + 1,
    "delivery_state": "processed",
    "delivery_error": "\x00" * QUEUE_ERROR_MAX_BYTES,
    "possibly_submitted": True,
    "probable_misroute": True,
    "misroutes": [
        {"at": _LONGEST_JSON_FLOAT, "error": "\x00" * QUEUE_ERROR_MAX_BYTES}
    ] * MAX_MISROUTES,
    "delivery_blocked_at": _LONGEST_JSON_FLOAT,
    "inflight_at": _LONGEST_JSON_FLOAT,
    "delivery_failed_at": _LONGEST_JSON_FLOAT,
    "confirmed_at": _LONGEST_JSON_FLOAT,
}
# Adding these fields to a nonempty, identically formatted object grows it by
# at most this many bytes (exclude the envelope's braces and final newline).
QUEUE_UPDATE_MAX_BYTES = len(_json_text(_UPDATE_ENVELOPE).encode("utf-8")) - 3
QUEUE_ERROR_SIDECAR_MAX_BYTES = len(_json_text({
    "artifact": "x" * 255 + ".json",
    "outcome": "possibly_submitted",
    "error": "\x00" * QUEUE_ERROR_MAX_BYTES,
    "failed_at": _LONGEST_JSON_FLOAT,
}).encode("utf-8"))


def _validate_artifact_limit(max_artifact_bytes: int | None) -> None:
    if max_artifact_bytes is not None and (
        type(max_artifact_bytes) is not int or max_artifact_bytes <= 0
    ):
        raise AgentDeliveryError("max_artifact_bytes must be a positive integer or None")


def _serialized_json(
    document: dict[str, object], max_artifact_bytes: int | None, *, allow_nan: bool = True,
) -> str:
    _validate_artifact_limit(max_artifact_bytes)
    serialized = _json_text(document, allow_nan=allow_nan)
    if max_artifact_bytes is not None:
        size = len(serialized.encode("utf-8"))
        if size > max_artifact_bytes:
            raise _ArtifactTooLarge(
                f"queue artifact needs {size} bytes, exceeding max_artifact_bytes={max_artifact_bytes}"
            )
    return serialized


def _effective_artifact_limit(max_artifact_bytes: int | None) -> int | None:
    """An opted-in staging bound also bounds queue reads and diagnostics."""
    _validate_artifact_limit(max_artifact_bytes)
    policy = _ACTIVE_ATOMIC_POLICY.get()
    if policy is None:
        return max_artifact_bytes
    _validate_artifact_limit(policy.max_bytes)
    return policy.max_bytes if max_artifact_bytes is None else min(max_artifact_bytes, policy.max_bytes)


def _bounded_error(error: str, max_artifact_bytes: int | None) -> str:
    if max_artifact_bytes is None:
        return error
    return error[:QUEUE_ERROR_MAX_BYTES].encode("utf-8", errors="replace")[:QUEUE_ERROR_MAX_BYTES].decode("utf-8", errors="ignore")


def queue_artifact_reservation_bytes(text: str, *, message_id: str) -> int:
    """Upper bound on a bounded enqueue artifact after every delivery update.

    This includes JSON escaping, the longest float timestamp, and capped errors.
    Reserve twice this size for an atomic replacement's temporary copy, plus
    ``QUEUE_ERROR_SIDECAR_MAX_BYTES`` for the failure sidecar. Queue control files
    (including target.json) need a separate reservation. For existing artifacts
    with additional fields, reserve their serialized size plus
    ``QUEUE_UPDATE_MAX_BYTES`` instead; this helper covers enqueue's schema only.
    """
    if not text or _MESSAGE_ID.fullmatch(message_id) is None:
        raise AgentDeliveryError("reservation needs nonempty text and a valid message id")
    initial: dict[str, object] = {
        "id": message_id, "text": text,
        "queued_at": _LONGEST_JSON_FLOAT, "delivery_attempts": 0,
    }
    return len(_json_text(initial).encode("utf-8")) + QUEUE_UPDATE_MAX_BYTES


def _reject_json_constant(value: str) -> None:
    raise ValueError(f"non-standard JSON constant {value!r}")


def _reject_duplicate_json_keys(pairs: list[tuple[str, object]]) -> dict[str, object]:
    document: dict[str, object] = {}
    for key, value in pairs:
        if key in document:
            raise ValueError(f"duplicate JSON object key {key[:80]!r}")
        document[key] = value
    return document


def _validate_json_depth(value: object, *, maximum: int = 64) -> None:
    """Refuse pathologically nested JSON after its encoded bytes were bounded."""
    pending: list[tuple[object, int]] = [(value, 0)]
    while pending:
        current, depth = pending.pop()
        if depth > maximum:
            raise ValueError(f"JSON nesting exceeds {maximum} levels")
        if isinstance(current, float) and not math.isfinite(current):
            raise ValueError("JSON numbers must be finite")
        if isinstance(current, dict):
            pending.extend((item, depth + 1) for item in current.values())
        elif isinstance(current, (list, tuple)):
            pending.extend((item, depth + 1) for item in current)


def _real(path: str) -> str:
    return os.path.realpath(os.path.abspath(path))


def _validate(client: HerdrClient, info: AgentPaneInfo, target: Target) -> None:
    failures: list[str] = []
    if target.expected_agent is not None and info.agent != target.expected_agent:
        failures.append(f"agent is {info.agent!r}, expected {target.expected_agent!r}")
    if target.session_agent is not None and info.session_agent != target.session_agent:
        failures.append(f"session agent is {info.session_agent!r}, expected {target.session_agent!r}")
    if target.session_value is not None and info.session_value != target.session_value:
        failures.append(f"session is {info.session_value!r}, expected {target.session_value!r}")
    if target.expected_workspace is not None:
        label = client.workspace_label(info.workspace_id)
        if label != target.expected_workspace:
            failures.append(f"workspace is {label!r}, expected {target.expected_workspace!r}")
    if target.expected_cwd is not None and _real(info.cwd) != _real(target.expected_cwd):
        failures.append(f"cwd is {info.cwd!r}, expected {_real(target.expected_cwd)!r}")
    if failures:
        raise AgentDeliveryError(f"refusing pane {info.pane_id}: " + "; ".join(failures))


def resolve_target(client: HerdrClient, target: Target) -> AgentPaneInfo:
    """Resolve by stable session when supplied, then revalidate every asserted field."""
    if not (target.pane_id or target.session_value):
        raise AgentDeliveryError("target needs --pane or a stable session value")
    asserted_pane_id = target.pane_id
    pane_id = asserted_pane_id
    if target.session_value is not None:
        matches: list[str] = []
        for pane in client.panes():
            info = client.pane_info(pane.pane_id)
            if info.session_value == target.session_value and (
                target.session_agent is None or info.session_agent == target.session_agent
            ):
                matches.append(pane.pane_id)
        if len(matches) != 1:
            raise AgentDeliveryError(
                f"expected exactly one live pane for session {target.session_value!r}, found {len(matches)}"
            )
        pane_id = matches[0]
        if asserted_pane_id is not None and asserted_pane_id != pane_id:
            raise AgentDeliveryError(
                f"refusing session target pane {pane_id!r}: expected exact pane {asserted_pane_id!r}"
            )
    if pane_id is None:
        raise AgentDeliveryError("target needs --pane or a stable session value")
    info = client.pane_info(pane_id)
    _validate(client, info, target)
    return info


def _fsync_dir(path: str) -> None:
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def _dirs(root: str) -> tuple[str, str, str, str]:
    return tuple(os.path.join(root, name) for name in ("inbox", "inflight", "processed", "failed"))  # type: ignore[return-value]


def _validate_private_directory(path: str, purpose: str, *, tighten: bool = False) -> None:
    """Require queue-owned directories to be real, private, and owned by this account."""

    try:
        metadata = os.stat(path, follow_symlinks=False)
    except OSError as exc:
        raise AgentDeliveryError(f"cannot inspect {purpose} {path}: {exc}") from exc
    if not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != os.getuid():
        raise AgentDeliveryError(f"unsafe {purpose}: {path}")
    if stat.S_IMODE(metadata.st_mode) & 0o077:
        if not tighten:
            raise AgentDeliveryError(f"{purpose} is not private: {path}")
        try:
            os.chmod(path, 0o700, follow_symlinks=False)
        except OSError as exc:
            raise AgentDeliveryError(f"cannot make {purpose} private {path}: {exc}") from exc


def _open_private_lock(path: str, purpose: str) -> int:
    """Open a same-user private lock without following a planted symlink."""

    flags = os.O_CREAT | os.O_RDWR | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
    descriptor = -1
    keep_open = False
    try:
        descriptor = os.open(path, flags, 0o600)
        metadata = os.fstat(descriptor)
        if (
            not stat.S_ISREG(metadata.st_mode)
            or metadata.st_uid != os.getuid()
            or stat.S_IMODE(metadata.st_mode) & 0o077
        ):
            raise AgentDeliveryError(f"unsafe {purpose}: {path}")
        keep_open = True
        return descriptor
    except OSError as exc:
        raise AgentDeliveryError(f"cannot open {purpose} {path}: {exc}") from exc
    finally:
        if descriptor >= 0 and not keep_open:
            os.close(descriptor)


def _read_queue_json(
    path: str, purpose: str, *, require_private: bool, max_artifact_bytes: int | None = None,
) -> object:
    """Read one owned regular queue artifact without following a symlink."""

    if max_artifact_bytes is not None:
        return _read_bounded_queue_json(
            path, purpose, require_private=require_private,
            max_artifact_bytes=max_artifact_bytes,
        )[0]

    flags = (
        os.O_RDONLY
        | getattr(os, "O_CLOEXEC", 0)
        | getattr(os, "O_NOFOLLOW", 0)
        | getattr(os, "O_NONBLOCK", 0)
    )
    descriptor = -1
    try:
        descriptor = os.open(path, flags)
        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid():
            raise AgentDeliveryError(f"unsafe {purpose}: {path}")
        if require_private and stat.S_IMODE(metadata.st_mode) & 0o077:
            raise AgentDeliveryError(f"{purpose} is not private: {path}")
        # Preserve the generic queue's historical unlimited mode. Chat and
        # other bounded callers take the strict helper above.
        with os.fdopen(descriptor, encoding="utf-8") as handle:
            descriptor = -1
            return json.load(handle, parse_constant=_reject_json_constant)
    except AgentDeliveryError:
        raise
    except (OSError, UnicodeError, json.JSONDecodeError, ValueError, RecursionError) as exc:
        raise AgentDeliveryError(f"cannot read {purpose} {path}: {exc}") from exc
    finally:
        if descriptor >= 0:
            os.close(descriptor)


def _read_bounded_queue_json(
    path: str, purpose: str, *, require_private: bool, max_artifact_bytes: int,
) -> tuple[object, int]:
    """Read and decode exactly one stable, bounded regular file descriptor."""
    _validate_artifact_limit(max_artifact_bytes)
    flags = (
        os.O_RDONLY
        | getattr(os, "O_CLOEXEC", 0)
        | getattr(os, "O_NOFOLLOW", 0)
        | getattr(os, "O_NONBLOCK", 0)
    )
    descriptor = -1
    try:
        try:
            descriptor = os.open(path, flags)
        except OSError as exc:
            raise AgentDeliveryError(f"cannot open {purpose} {path}: {exc}") from exc
        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid():
            raise AgentDeliveryError(f"unsafe {purpose}: {path}")
        if require_private and stat.S_IMODE(metadata.st_mode) & 0o077:
            raise AgentDeliveryError(f"{purpose} is not private: {path}")
        if metadata.st_nlink != 1:
            raise AgentDeliveryError(f"{purpose} must not be hard-linked: {path}")
        if metadata.st_size > max_artifact_bytes:
            raise AgentDeliveryError(
                f"{purpose} exceeds max_artifact_bytes={max_artifact_bytes}: {path}")
        encoded = bytearray()
        while True:
            remaining = max_artifact_bytes + 1 - len(encoded)
            if remaining <= 0:
                raise AgentDeliveryError(
                    f"{purpose} exceeds max_artifact_bytes={max_artifact_bytes}: {path}")
            block = os.read(descriptor, min(64 << 10, remaining))
            if not block:
                break
            encoded.extend(block)
            if len(encoded) > max_artifact_bytes:
                raise AgentDeliveryError(
                    f"{purpose} exceeds max_artifact_bytes={max_artifact_bytes}: {path}")
        final_metadata = os.fstat(descriptor)
        if (
            final_metadata.st_dev != metadata.st_dev
            or final_metadata.st_ino != metadata.st_ino
            or final_metadata.st_size != len(encoded)
            or final_metadata.st_mode != metadata.st_mode
            or final_metadata.st_uid != metadata.st_uid
            or final_metadata.st_mtime_ns != metadata.st_mtime_ns
            or final_metadata.st_nlink != 1
            or final_metadata.st_ctime_ns != metadata.st_ctime_ns
        ):
            raise AgentDeliveryError(f"{purpose} changed while it was read: {path}")
        decoded = json.loads(
            bytes(encoded).decode("utf-8"),
            object_pairs_hook=_reject_duplicate_json_keys,
            parse_constant=_reject_json_constant,
        )
        _validate_json_depth(decoded)
        return decoded, len(encoded)
    except AgentDeliveryError:
        raise
    except (OSError, UnicodeError, json.JSONDecodeError, ValueError, RecursionError) as exc:
        raise AgentDeliveryError(f"cannot read {purpose} {path}: {exc}") from exc
    finally:
        if descriptor >= 0:
            os.close(descriptor)


def _prepare(root: str) -> tuple[str, str, str, str]:
    root_existed = os.path.lexists(root)
    os.makedirs(root, mode=0o700, exist_ok=True)
    _validate_private_directory(root, "queue directory", tighten=True)
    if not root_existed:
        _fsync_dir(os.path.dirname(os.path.abspath(root)))
    paths = _dirs(root)
    for path in paths:
        os.makedirs(path, mode=0o700, exist_ok=True)
        _validate_private_directory(path, "queue state directory", tighten=True)
    _fsync_dir(root)
    return paths


def _queue_directory_identity(root: str) -> tuple[int, int]:
    """Remember the private queue generation before waiting on one of its locks."""
    try:
        metadata = os.stat(root, follow_symlinks=False)
    except OSError as exc:
        raise AgentDeliveryError(f"cannot inspect queue directory {root}: {exc}") from exc
    if not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != os.getuid():
        raise AgentDeliveryError(f"unsafe queue directory: {root}")
    if stat.S_IMODE(metadata.st_mode) & 0o077:
        raise AgentDeliveryError(f"queue directory is not private: {root}")
    return metadata.st_dev, metadata.st_ino


def _verify_queue_lock(
    root: str, directory_identity: tuple[int, int], path: str, descriptor: int, purpose: str,
) -> None:
    """A lock on an archived or replaced inode cannot protect the reused queue path."""
    if _queue_directory_identity(root) != directory_identity:
        raise AgentDeliveryError(f"queue directory changed while waiting for {purpose}: {root}")
    try:
        current = os.stat(path, follow_symlinks=False)
        opened = os.fstat(descriptor)
    except OSError as exc:
        raise AgentDeliveryError(f"cannot inspect {purpose} after locking {path}: {exc}") from exc
    for metadata in (current, opened):
        if (not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid()
                or stat.S_IMODE(metadata.st_mode) & 0o077):
            raise AgentDeliveryError(f"unsafe {purpose}: {path}")
    if (current.st_dev, current.st_ino) != (opened.st_dev, opened.st_ino):
        raise AgentDeliveryError(f"{purpose} changed while waiting: {path}")


def _atomic_policy_directory(policy: AtomicWritePolicy) -> int:
    _validate_artifact_limit(policy.max_bytes)
    parent = os.path.dirname(policy.directory)
    try:
        os.mkdir(policy.directory, 0o700)
    except FileExistsError:
        pass
    else:
        _fsync_dir(parent)
    descriptor = os.open(policy.directory, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        metadata = os.fstat(descriptor)
        if metadata.st_uid != os.getuid() or stat.S_IMODE(metadata.st_mode) != 0o700:
            raise AgentDeliveryError("atomic staging directory must be owned and mode 0700")
    except BaseException:
        os.close(descriptor)
        raise
    return descriptor


_MAX_ATOMIC_INTENT_BYTES = 8192
_ATOMIC_PAYLOAD = "staged.json"
_ATOMIC_PUBLISH = "publish.json"
_ATOMIC_INTENT = "intent.json"
_ATOMIC_REFUSAL_PREFIX = b"!atomic-cleanup-refusal-v1\n"
_ATOMIC_CLEANUP_PHASE = b"\n!atomic-cleanup-phase-v1\n"
_ATOMIC_CLEANUP_COMMIT = b"\n!atomic-cleanup-commit-v1\n"


def _atomic_cleanup_state(encoded: bytes) -> tuple[bytes, str]:
    """Separate original intent from bounded, content-free cleanup phases."""
    if encoded.startswith(_ATOMIC_REFUSAL_PREFIX[:1]):
        raise AgentDeliveryError(
            f"atomic cleanup interference; evidence retained (sha256={hashlib.sha256(encoded).hexdigest()})")
    index = encoded.find(_ATOMIC_CLEANUP_PHASE)
    if index >= 0:
        suffix = encoded[index + len(_ATOMIC_CLEANUP_PHASE):]
        if suffix == _ATOMIC_CLEANUP_COMMIT:
            return encoded[:index], "commit"
        if _ATOMIC_CLEANUP_COMMIT.startswith(suffix):
            return encoded[:index], "prepare"
        raise AgentDeliveryError("invalid atomic cleanup phase suffix; evidence retained")
    if b"\n!" in encoded:
        raise AgentDeliveryError(
            f"atomic cleanup interference; evidence retained (sha256={hashlib.sha256(encoded).hexdigest()})")
    return encoded, "none"


def _prepare_atomic_cleanup(
    directory: int, intent: tuple[int, os.stat_result] | None,
) -> tuple[int, os.stat_result]:
    """Durably mark cleanup before deleting anything; preserve original bytes."""
    original = b"" if intent is None else os.pread(intent[0], _MAX_ATOMIC_INTENT_BYTES, 0)
    base, state = _atomic_cleanup_state(original)
    original_size = len(original)
    if state == "commit":
        raise AgentDeliveryError("atomic cleanup is already committed")
    if len(base) + len(_ATOMIC_CLEANUP_PHASE) + len(_ATOMIC_CLEANUP_COMMIT) > _MAX_ATOMIC_INTENT_BYTES:
        raise AgentDeliveryError("atomic intent has no bounded cleanup-phase headroom")
    flags = os.O_RDWR | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC
    descriptor = os.open(_ATOMIC_INTENT, flags | (os.O_CREAT | os.O_EXCL if intent is None else 0),
                         0o600, dir_fd=directory)
    try:
        metadata = os.fstat(descriptor)
        if intent is not None:
            _revalidate_atomic_entries((descriptor, intent[1]))
        elif (not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid()
                or stat.S_IMODE(metadata.st_mode) != 0o600 or metadata.st_nlink != 1):
            raise AgentDeliveryError("atomic cleanup phase is not private owned evidence")
        os.lseek(descriptor, original_size, os.SEEK_SET)
        try:
            offset = 0
            while state != "prepare" and offset < len(_ATOMIC_CLEANUP_PHASE):
                written = os.write(descriptor, _ATOMIC_CLEANUP_PHASE[offset:])
                if written <= 0:
                    raise OSError("atomic cleanup phase write made no progress")
                offset += written
            os.fsync(descriptor)
            os.fsync(directory)
            prepared = os.fstat(descriptor)
            if ((prepared.st_dev, prepared.st_ino, prepared.st_mode, prepared.st_uid, prepared.st_nlink)
                    != (metadata.st_dev, metadata.st_ino, metadata.st_mode, metadata.st_uid, 1)
                    or prepared.st_size != original_size + (0 if state == "prepare" else len(_ATOMIC_CLEANUP_PHASE))):
                raise AgentDeliveryError("atomic cleanup phase changed while it was prepared")
        except BaseException:
            # Destructive cleanup has not begun. Restore the exact original
            # intent without ever replacing/removing any payload or final link.
            if intent is None:
                os.unlink(_ATOMIC_INTENT, dir_fd=directory)
            else:
                os.ftruncate(descriptor, original_size)
                os.fsync(descriptor)
            os.fsync(directory)
            raise
        return descriptor, os.fstat(descriptor)
    except BaseException:
        os.close(descriptor)
        raise


def _commit_atomic_cleanup(directory: int, phase: tuple[int, os.stat_result]) -> None:
    """Certify completed post-unlink checks, never a prediction of them."""
    _revalidate_atomic_entries(phase)
    encoded = os.pread(phase[0], _MAX_ATOMIC_INTENT_BYTES + 1, 0)
    _revalidate_atomic_entries(phase)
    if len(encoded) != phase[1].st_size:
        raise AgentDeliveryError("atomic cleanup phase size changed before commit")
    original, state = _atomic_cleanup_state(encoded)
    if state != "prepare":
        raise AgentDeliveryError("atomic cleanup has no prepared phase")
    # A partial COMMIT from a prior attempt is not authority; overwrite just
    # that suffix after the current attempt repeated every semantic check.
    offset = len(original) + len(_ATOMIC_CLEANUP_PHASE)
    os.lseek(phase[0], offset, os.SEEK_SET)
    written = 0
    while written < len(_ATOMIC_CLEANUP_COMMIT):
        count = os.write(phase[0], _ATOMIC_CLEANUP_COMMIT[written:])
        if count <= 0:
            raise OSError("atomic cleanup commit write made no progress")
        written += count
    os.ftruncate(phase[0], offset + len(_ATOMIC_CLEANUP_COMMIT))
    committed = os.fstat(phase[0])
    if ((committed.st_dev, committed.st_ino, committed.st_mode, committed.st_uid, committed.st_nlink)
            != (phase[1].st_dev, phase[1].st_ino, phase[1].st_mode, phase[1].st_uid, 1)
            or committed.st_size != offset + len(_ATOMIC_CLEANUP_COMMIT)):
        raise AgentDeliveryError("atomic cleanup certificate changed while it was written")
    os.fsync(phase[0])
    os.fsync(directory)
    exact = original + _ATOMIC_CLEANUP_PHASE + _ATOMIC_CLEANUP_COMMIT
    if os.pread(phase[0], _MAX_ATOMIC_INTENT_BYTES + 1, 0) != exact:
        raise AgentDeliveryError("atomic cleanup certificate contents changed before commit")
    _revalidate_atomic_entries((phase[0], committed))


def _atomic_entry(directory: int, name: str, limit: int) -> tuple[int, os.stat_result] | None:
    try:
        descriptor = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
                             dir_fd=directory)
    except FileNotFoundError:
        return None
    try:
        metadata = os.fstat(descriptor)
        if (not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid()
                or stat.S_IMODE(metadata.st_mode) != 0o600):
            raise AgentDeliveryError("atomic staging slot must be an owned mode 0600 regular file")
        if metadata.st_size > limit:
            raise AgentDeliveryError(f"atomic staging slot exceeds {limit} bytes")
        return descriptor, metadata
    except BaseException:
        os.close(descriptor)
        raise


def _atomic_relative_destination(policy: AtomicWritePolicy, path: str) -> str:
    relative = os.path.relpath(os.path.abspath(path), os.path.dirname(policy.directory))
    _validate_atomic_destination(policy, relative)
    return relative


def _validate_atomic_destination(policy: AtomicWritePolicy, relative: str) -> None:
    staging_name = os.path.basename(policy.directory)
    if (not relative or os.path.isabs(relative)
            or any(component in ("", ".", "..") for component in relative.split("/"))):
        raise AgentDeliveryError("atomic destination must remain inside its policy root")
    if (relative in (staging_name, staging_name + ".lock")
            or (relative.startswith(staging_name + "/")
                and relative != staging_name + "/audited-v1.json")):
        raise AgentDeliveryError("atomic destination overlaps staging or lock authority")


def _atomic_destination_directory(policy: AtomicWritePolicy, relative: str) -> int:
    _validate_atomic_destination(policy, relative)
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
    descriptor = os.open(os.path.dirname(policy.directory), flags)
    try:
        for component in relative.split("/")[:-1]:
            child = os.open(component, flags, dir_fd=descriptor)
            os.close(descriptor)
            descriptor = child
        return descriptor
    except BaseException:
        os.close(descriptor)
        raise


def _clear_atomic_evidence(
    directory: int, *, payload: tuple[int, os.stat_result] | None,
    publish: tuple[int, os.stat_result] | None, intent: tuple[int, os.stat_result] | None,
    final: tuple[int, str, tuple[int, os.stat_result]] | None = None,
) -> None:
    """Check held inodes on both sides of every destructive cleanup boundary.

    This detects interference, not exclusion of an arbitrary same-UID actor:
    unlink changes ctime itself, and Linux offers no unlink-if-link-count syscall.
    Transient aliasing inside that syscall or after our final check cannot be
    ruled out. Cooperating writers are excluded by the atomic lock.
    """
    _revalidate_atomic_entries(payload, publish, intent, None if final is None else final[2])
    saved = b"" if intent is None else os.pread(intent[0], _MAX_ATOMIC_INTENT_BYTES, 0)
    phase = _prepare_atomic_cleanup(directory, intent)
    internal = {_ATOMIC_PAYLOAD: payload, _ATOMIC_PUBLISH: publish, _ATOMIC_INTENT: phase}
    tracked = [entry for entry in (None if final is None else final[2], payload, publish, phase)
               if entry is not None]

    def verify(unlinked: os.stat_result | None = None) -> None:
        checked: list[tuple[int, os.stat_result]] = []
        for descriptor, expected in tracked:
            current = os.fstat(descriptor)
            changed = unlinked is not None and (expected.st_dev, expected.st_ino) == (
                unlinked.st_dev, unlinked.st_ino)
            if (_atomic_stable_metadata(current) != _atomic_stable_metadata(expected)
                    or current.st_nlink != expected.st_nlink - int(changed)
                    or (not changed and current.st_ctime_ns != expected.st_ctime_ns)):
                purpose = "destination" if final is not None and descriptor == final[2][0] else "staging evidence"
                raise AgentDeliveryError(f"atomic {purpose} changed during publication or recovery cleanup")
            if final is not None and descriptor == final[2][0]:
                _verify_atomic_final(final[0], final[1], descriptor, current, links=current.st_nlink)
            checked.append((descriptor, current))
        tracked[:] = checked

    def unlink(name: str) -> None:
        entry = internal[name]
        if entry is None:
            return
        verify()
        before = os.fstat(entry[0])
        os.unlink(name, dir_fd=directory)
        verify(before)

    try:
        try:
            unlink(_ATOMIC_PUBLISH)
            unlink(_ATOMIC_PAYLOAD)
            os.fsync(directory)
            verify()
            _commit_atomic_cleanup(directory, phase)
        except BaseException as exc:
            # The durable phase is sufficient even if richer diagnostics fail
            # before their first byte. No destructive cleanup can precede it.
            if isinstance(exc, AgentDeliveryError):
                _retain_atomic_cleanup_refusal(directory, phase, tracked, saved)
            raise
        # Commit point: all data cleanup, directory fsync and semantic checks
        # succeeded with the phase still durable. Do not perform another
        # fallible semantic check after removing the last recovery evidence.
        # An unlink/fsync error may leave phase-or-absence; both are safe.
        os.unlink(_ATOMIC_INTENT, dir_fd=directory)
        os.fsync(directory)
    finally:
        os.close(phase[0])


def _retain_atomic_cleanup_refusal(
    directory: int, intent: tuple[int, os.stat_result] | None,
    entries: list[tuple[int, os.stat_result]], saved: bytes,
) -> None:
    """Keep a bounded, explicitly terminal record, even after intent unlink."""
    evidence = [{"device": metadata.st_dev, "inode": metadata.st_ino, "links": metadata.st_nlink}
                for _, metadata in entries]
    record: dict[str, object] = {"version": 1, "type": "atomic-cleanup-refusal",
                                 "reason": "inode topology changed during cleanup",
                                 "intent_sha256": hashlib.sha256(saved).hexdigest(), "evidence": evidence}
    # A refusal starts outside the JSON alphabet. Even a one-byte partial write
    # cannot be mistaken for an incomplete, safely unpublished JSON intent.
    encoded = _ATOMIC_REFUSAL_PREFIX + _serialized_json(
        record, _MAX_ATOMIC_INTENT_BYTES - len(_ATOMIC_REFUSAL_PREFIX), allow_nan=False).encode("utf-8")
    flags = os.O_WRONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK
    try:
        descriptor = os.open(_ATOMIC_INTENT, flags | os.O_CREAT | os.O_EXCL, 0o600, dir_fd=directory)
    except FileExistsError:
        descriptor = os.open(_ATOMIC_INTENT, flags, dir_fd=directory)
        metadata = os.fstat(descriptor)
        if (intent is None or (metadata.st_dev, metadata.st_ino) != (intent[1].st_dev, intent[1].st_ino)
                or not stat.S_ISREG(metadata.st_mode) or stat.S_IMODE(metadata.st_mode) != 0o600
                or metadata.st_uid != os.getuid() or metadata.st_nlink != 1
                or metadata.st_size > _MAX_ATOMIC_INTENT_BYTES):
            os.close(descriptor)
            os.fsync(directory)
            raise AgentDeliveryError("atomic cleanup refusal could not replace changed intent evidence")
    try:
        os.fchmod(descriptor, 0o600)
        offset = 0
        while offset < len(encoded):
            written = os.write(descriptor, encoded[offset:])
            if written <= 0:
                raise OSError("atomic cleanup refusal write made no progress")
            offset += written
        # Write the non-JSON prefix before truncation: an interrupted overwrite
        # must not turn a refusal into an empty, recoverable partial intent.
        os.ftruncate(descriptor, len(encoded))
        os.fsync(descriptor)
        os.fsync(directory)
    finally:
        os.close(descriptor)


def _discard_unpublished_atomic(
    directory: int, policy: AtomicWritePolicy,
    payload_identity: os.stat_result, intent_identity: os.stat_result,
) -> None:
    """Discard a definite EEXIST attempt without inspecting its destination."""
    entries: list[tuple[int, os.stat_result]] = []
    try:
        for name, expected, limit in (
            (_ATOMIC_PAYLOAD, payload_identity, policy.max_bytes),
            (_ATOMIC_INTENT, intent_identity, _MAX_ATOMIC_INTENT_BYTES),
        ):
            entry = _atomic_entry(directory, name, limit)
            if entry is None:
                raise AgentDeliveryError("unpublished atomic evidence is missing")
            entries.append(entry)
            actual = entry[1]
            if (actual.st_dev != expected.st_dev or actual.st_ino != expected.st_ino
                    or actual.st_size != expected.st_size or actual.st_nlink != 1
                    or actual.st_mtime_ns != expected.st_mtime_ns
                    or actual.st_ctime_ns != expected.st_ctime_ns):
                raise AgentDeliveryError("unpublished atomic evidence changed")
        _revalidate_atomic_entries(*entries)
        _clear_atomic_evidence(directory, payload=entries[0], publish=None, intent=entries[1])
    finally:
        for entry in entries:
            os.close(entry[0])


def _revalidate_atomic_entries(*entries: tuple[int, os.stat_result] | None) -> None:
    """Recheck held inode authority after intent reads or destination fsync."""
    for entry in entries:
        if entry is None:
            continue
        current, expected = os.fstat(entry[0]), entry[1]
        if (current.st_dev != expected.st_dev or current.st_ino != expected.st_ino
                or current.st_mode != expected.st_mode or current.st_uid != expected.st_uid
                or current.st_size != expected.st_size or current.st_nlink != expected.st_nlink
                or current.st_mtime_ns != expected.st_mtime_ns or current.st_ctime_ns != expected.st_ctime_ns):
            raise AgentDeliveryError("atomic staging evidence changed during recovery")


def _atomic_stable_metadata(metadata: os.stat_result) -> tuple[int, ...]:
    return (metadata.st_dev, metadata.st_ino, metadata.st_mode, metadata.st_uid,
            metadata.st_size, metadata.st_mtime_ns)


def _verify_atomic_final(
    target: int, name: str, descriptor: int, expected: os.stat_result, *,
    links: int, stable_ctime: bool = True,
) -> os.stat_result:
    current = os.fstat(descriptor)
    named = os.stat(name, dir_fd=target, follow_symlinks=False)
    if (_atomic_stable_metadata(current) != _atomic_stable_metadata(expected)
            or current.st_nlink != links
            or (stable_ctime and current.st_ctime_ns != expected.st_ctime_ns)
            or _atomic_stable_metadata(named) != _atomic_stable_metadata(current)
            or named.st_nlink != current.st_nlink or named.st_ctime_ns != current.st_ctime_ns):
        raise AgentDeliveryError("atomic destination changed during publication or recovery")
    return current


def _recover_atomic_slot(directory: int, policy: AtomicWritePolicy) -> None:
    payload = publish = intent = final_entry = None
    target = -1
    try:
        payload = _atomic_entry(directory, _ATOMIC_PAYLOAD, policy.max_bytes)
        publish = _atomic_entry(directory, _ATOMIC_PUBLISH, policy.max_bytes)
        intent = _atomic_entry(directory, _ATOMIC_INTENT, _MAX_ATOMIC_INTENT_BYTES)
        if intent is None:
            if publish is not None or (payload is not None and payload[1].st_nlink != 1):
                raise AgentDeliveryError("atomic staging hardlink has no durable destination intent")
            if payload is not None:
                # No publication was authorized before the durable intent.
                # Seal intent absence before removing the last payload name;
                # this also completes a retried partial-intent cleanup.
                os.fsync(directory)
                _revalidate_atomic_entries(payload)
                _clear_atomic_evidence(directory, payload=payload, publish=None, intent=None)
            return
        if intent[1].st_nlink != 1:
            raise AgentDeliveryError("atomic destination intent must not be hard-linked")
        encoded = bytearray()
        while block := os.read(intent[0], min(8192, _MAX_ATOMIC_INTENT_BYTES + 1 - len(encoded))):
            encoded.extend(block)
            if len(encoded) > _MAX_ATOMIC_INTENT_BYTES:
                raise AgentDeliveryError("atomic destination intent exceeds its byte limit")
        final_intent = os.fstat(intent[0])
        if (len(encoded) != intent[1].st_size or final_intent.st_size != len(encoded)
                or final_intent.st_mtime_ns != intent[1].st_mtime_ns
                or final_intent.st_ctime_ns != intent[1].st_ctime_ns
                or final_intent.st_mode != intent[1].st_mode
                or final_intent.st_uid != intent[1].st_uid or final_intent.st_nlink != 1):
            raise AgentDeliveryError("atomic destination intent changed while it was read")
        original_intent, cleanup_state = _atomic_cleanup_state(bytes(encoded))
        if cleanup_state == "commit":
            if payload is not None or publish is not None:
                raise AgentDeliveryError("committed atomic cleanup still has internal payload links")
            _revalidate_atomic_entries(intent)
            os.unlink(_ATOMIC_INTENT, dir_fd=directory)
            os.fsync(directory)
            return
        try:
            document: object = json.loads(original_intent, object_pairs_hook=_reject_duplicate_json_keys,
                                          parse_constant=_reject_json_constant)
        except (ValueError, UnicodeError, RecursionError) as exc:
            _revalidate_atomic_entries(payload, publish)
            if payload is not None and payload[1].st_nlink == 1 and publish is None:
                # A partially written intent cannot authorize publication, and
                # this exact single-link payload has no published final name.
                # Establish durable cleanup authority even for a partial
                # original intent, before removing the last payload name.
                _clear_atomic_evidence(directory, payload=payload, publish=None, intent=intent)
                return
            if cleanup_state == "prepare":
                raise AgentDeliveryError("atomic cleanup interference; evidence retained: no trusted anchor") from exc
            raise AgentDeliveryError("invalid atomic destination intent; recovery evidence retained") from exc
        fields = {"version", "destination", "operation", "device", "inode", "directory_device", "directory_inode"}
        if not isinstance(document, dict) or set(document) != fields:
            raise AgentDeliveryError("invalid atomic destination intent fields")
        relative = document.get("destination")
        if (document.get("version") != 1 or type(document.get("version")) is not int
                or document.get("operation") not in ("create", "replace") or not isinstance(relative, str)
                or any(type(document.get(key)) is not int or document[key] < 0 for key in (
                    "device", "inode", "directory_device", "directory_inode"))):
            raise AgentDeliveryError("invalid atomic destination intent authority")
        target = _atomic_destination_directory(policy, relative)
        target_info = os.fstat(target)
        if (target_info.st_dev, target_info.st_ino) != (document["directory_device"], document["directory_inode"]):
            raise AgentDeliveryError("atomic destination directory identity changed")
        identity = (document["device"], document["inode"])
        for entry in (payload, publish):
            if entry is not None and (entry[1].st_dev, entry[1].st_ino) != identity:
                raise AgentDeliveryError("atomic staging payload identity does not match intent")
        try:
            final = os.stat(relative.split("/")[-1], dir_fd=target, follow_symlinks=False)
        except FileNotFoundError:
            final = None
        matched = final is not None and (final.st_dev, final.st_ino) == identity
        expected_links = int(payload is not None) + int(publish is not None) + int(matched)
        if any(entry is not None and entry[1].st_nlink != expected_links for entry in (payload, publish)):
            raise AgentDeliveryError("atomic staging payload has unrelated hardlinks")
        if matched:
            assert final is not None
            if (not stat.S_ISREG(final.st_mode) or final.st_uid != os.getuid()
                    or stat.S_IMODE(final.st_mode) != 0o600):
                raise AgentDeliveryError("atomic destination is not an owned mode 0600 regular file")
            if final.st_nlink != expected_links:
                raise AgentDeliveryError("atomic destination has unrelated hardlinks")
            name = relative.split("/")[-1]
            final_entry = _atomic_entry(target, name, policy.max_bytes)
            if final_entry is None:
                raise AgentDeliveryError("atomic destination disappeared during recovery")
            _verify_atomic_final(target, name, final_entry[0], final, links=expected_links)
            # Replaying a visible final is not a durability acknowledgment.
            # This fsync must succeed before readers/adoption can proceed.
            os.fsync(target)
            _verify_atomic_final(target, name, final_entry[0], final_entry[1], links=expected_links)
        # An unrelated old destination proves this payload was not published.
        # Its type/mode/link count is not ours to validate or alter; only the
        # intent-owned internal names are discarded in that case.
        # An intent-only unmatched state from older writers has no surviving
        # inode authority to prove that cleanup did not leak an external alias.
        # Do not erase it merely because current writers use a typed phase.
        if not matched and payload is None and publish is None:
            raise AgentDeliveryError("atomic cleanup interference; evidence retained: no trusted payload links")
        # A publish-only inode is also owned evidence: without a successful
        # directory fsync, crash persistence need not retain unlink order.
        _revalidate_atomic_entries(payload, publish, intent)
        _clear_atomic_evidence(
            directory, payload=payload, publish=publish, intent=intent,
            final=None if final_entry is None else (target, relative.split("/")[-1], final_entry))
    finally:
        if target >= 0:
            os.close(target)
        for entry in (payload, publish, intent, final_entry):
            if entry is not None:
                os.close(entry[0])


@contextmanager
def atomic_write_recovery(policy: AtomicWritePolicy) -> Iterator[None]:
    """Recover only the fixed owned slot and exclude writers until exit.

    A crash can leave its final name linked to this slot. Fsyncing the intended
    destination before removing evidence restores its single-link invariant without
    relaxing readers' refusal of unrelated hardlinks. No directory is scanned.
    Callers holding another writer lock acquire that lock before this one.

    Detected cleanup interference leaves a bounded, versioned refusal record
    in the intent slot, including when that slot has to be reconstructed. The
    record contains inode identities and a digest, not payload text, and is
    never automatically cleared. The checks do not exclude an uncooperative
    same-UID actor: transient link changes within our own unlink's ctime update,
    or changes after the final check, cannot be atomically ruled out on Linux.
    A typed cleanup phase is fsynced before any internal payload unlink. A
    PREPARE recovery needs a surviving trusted payload, publication or final
    anchor. The no-anchor window after last unlink but before durable COMMIT
    is irreducibly ambiguous and requires offline inspection. COMMIT follows
    post-unlink checks and cleanup fsync, and permits intent-only recovery.
    Phase unlink is the commit point, after all semantic checks; only a final
    directory fsync follows it. Same-UID interference after the last check is
    outside this protocol, not a condition that can safely be detected later.
    """
    policy = AtomicWritePolicy(os.path.abspath(policy.directory), policy.max_bytes)
    lock = _open_private_lock(policy.directory + ".lock", "atomic staging lock")
    directory = -1
    try:
        fcntl.flock(lock, fcntl.LOCK_EX)
        directory = _atomic_policy_directory(policy)
        _recover_atomic_slot(directory, policy)
        yield
    finally:
        if directory >= 0:
            os.close(directory)
        os.close(lock)


def _atomic_json_staged(
    path: str, document: dict[str, object], policy: AtomicWritePolicy, *,
    create: bool, max_artifact_bytes: int | None,
) -> None:
    relative = _atomic_relative_destination(policy, path)
    lock = _open_private_lock(policy.directory + ".lock", "atomic staging lock")
    directory = target = staged = intent = -1
    final_entry = payload_entry = intent_entry = None
    try:
        fcntl.flock(lock, fcntl.LOCK_EX)
        directory = _atomic_policy_directory(policy)
        _recover_atomic_slot(directory, policy)
        limit = policy.max_bytes if max_artifact_bytes is None else min(policy.max_bytes, max_artifact_bytes)
        serialized = _serialized_json(document, limit, allow_nan=not create).encode("utf-8")
        target = _atomic_destination_directory(policy, relative)
        target_metadata = os.fstat(target)
        if target_metadata.st_dev != os.fstat(directory).st_dev:
            raise AgentDeliveryError("atomic staging and destination must share a filesystem")
        staged = os.open("staged.json", os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC,
                         0o600, dir_fd=directory)
        os.fchmod(staged, 0o600)
        offset = 0
        while offset < len(serialized):
            written = os.write(staged, serialized[offset:])
            if written <= 0:
                raise OSError("atomic staging write made no progress")
            offset += written
        os.fsync(staged)
        metadata = os.fstat(staged)
        os.close(staged)
        staged = -1
        intent_document: dict[str, object] = {
            "version": 1, "destination": relative, "operation": "create" if create else "replace",
            "device": metadata.st_dev, "inode": metadata.st_ino,
            "directory_device": target_metadata.st_dev, "directory_inode": target_metadata.st_ino,
        }
        encoded_intent = _serialized_json(
            intent_document, _MAX_ATOMIC_INTENT_BYTES - len(_ATOMIC_CLEANUP_PHASE) - len(_ATOMIC_CLEANUP_COMMIT),
            allow_nan=False).encode("utf-8")
        intent = os.open(_ATOMIC_INTENT, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC,
                         0o600, dir_fd=directory)
        os.fchmod(intent, 0o600)
        offset = 0
        while offset < len(encoded_intent):
            written = os.write(intent, encoded_intent[offset:])
            if written <= 0:
                raise OSError("atomic intent write made no progress")
            offset += written
        os.fsync(intent)
        intent_metadata = os.fstat(intent)
        os.close(intent)
        intent = -1
        os.fsync(directory)
        name = os.path.basename(path)
        if create:
            try:
                os.link("staged.json", name, src_dir_fd=directory, dst_dir_fd=target, follow_symlinks=False)
            except FileExistsError:
                # EEXIST proves this syscall installed no final link. Do not
                # retain rejected submission text in the staging slot. Other
                # failures may follow a successful link and retain recovery
                # evidence, as do destination-fsync failures below.
                _discard_unpublished_atomic(directory, policy, metadata, intent_metadata)
                raise
        else:
            os.link(_ATOMIC_PAYLOAD, _ATOMIC_PUBLISH, src_dir_fd=directory, dst_dir_fd=directory,
                    follow_symlinks=False)
            os.replace(_ATOMIC_PUBLISH, name, src_dir_fd=directory, dst_dir_fd=target)
        final_entry = _atomic_entry(target, name, limit)
        if final_entry is None:
            raise AgentDeliveryError("atomic destination disappeared during publication")
        _verify_atomic_final(target, name, final_entry[0], metadata, links=2, stable_ctime=False)
        os.fsync(target)
        _verify_atomic_final(target, name, final_entry[0], final_entry[1], links=2)
        payload_entry = _atomic_entry(directory, _ATOMIC_PAYLOAD, limit)
        intent_entry = _atomic_entry(directory, _ATOMIC_INTENT, _MAX_ATOMIC_INTENT_BYTES)
        if payload_entry is None or intent_entry is None:
            raise AgentDeliveryError("atomic publication evidence disappeared before cleanup")
        _verify_atomic_final(directory, _ATOMIC_PAYLOAD, payload_entry[0], final_entry[1], links=2)
        _revalidate_atomic_entries((intent_entry[0], intent_metadata))
        _clear_atomic_evidence(
            directory, payload=payload_entry, publish=None, intent=intent_entry,
            final=(target, name, final_entry))
    finally:
        for entry in (final_entry, payload_entry, intent_entry):
            if entry is not None:
                os.close(entry[0])
        for descriptor in (staged, intent, target, directory, lock):
            if descriptor >= 0:
                os.close(descriptor)


def _atomic_json(
    path: str, document: dict[str, object], *, max_artifact_bytes: int | None = None,
) -> None:
    policy = _ACTIVE_ATOMIC_POLICY.get()
    if policy is not None:
        _atomic_json_staged(path, document, policy, create=False, max_artifact_bytes=max_artifact_bytes)
        return
    serialized = None if max_artifact_bytes is None else _serialized_json(document, max_artifact_bytes)
    parent = os.path.dirname(path)
    temporary = ""
    try:
        with tempfile.NamedTemporaryFile("w", encoding="utf-8", dir=parent, prefix=".message.", delete=False) as handle:
            temporary = handle.name
            os.fchmod(handle.fileno(), 0o600)
            if serialized is None:
                json.dump(document, handle, indent=2, sort_keys=True)
                handle.write("\n")
            else:
                handle.write(serialized)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, path)
        _fsync_dir(parent)
    finally:
        if temporary:
            try:
                os.unlink(temporary)
            except FileNotFoundError:
                pass


def _atomic_json_create(
    path: str, document: dict[str, object], *, max_artifact_bytes: int | None = None,
) -> None:
    """Durably create one JSON artifact without replacing an existing name."""

    policy = _ACTIVE_ATOMIC_POLICY.get()
    if policy is not None:
        _atomic_json_staged(path, document, policy, create=True, max_artifact_bytes=max_artifact_bytes)
        return

    serialized = None if max_artifact_bytes is None else _serialized_json(document, max_artifact_bytes, allow_nan=False)
    parent = os.path.dirname(path)
    temporary = ""
    try:
        with tempfile.NamedTemporaryFile(
            "w", encoding="utf-8", dir=parent, prefix=".message.", delete=False
        ) as handle:
            temporary = handle.name
            os.fchmod(handle.fileno(), 0o600)
            if serialized is None:
                json.dump(document, handle, indent=2, sort_keys=True, allow_nan=False)
                handle.write("\n")
            else:
                handle.write(serialized)
            handle.flush()
            os.fsync(handle.fileno())
        os.link(temporary, path, follow_symlinks=False)
        os.unlink(temporary)
        temporary = ""
        _fsync_dir(parent)
    finally:
        if temporary:
            try:
                os.unlink(temporary)
            except FileNotFoundError:
                pass


def enqueue(
    root: str, text: str, *, message_id: str | None = None, max_artifact_bytes: int | None = None,
    atomic_policy: AtomicWritePolicy | None = None,
) -> str:
    """Persist a prompt, optionally limiting each serialized artifact's UTF-8 bytes.

    The limit is checked before creating a temporary file. None leaves sizes and
    diagnostics unrestricted. Use ``queue_artifact_reservation_bytes`` to allow
    for subsequent delivery updates as well as this initial write.
    """
    with atomic_write_policy(atomic_policy):
        return _enqueue(
            root, text, message_id=message_id, serialize=True, max_artifact_bytes=max_artifact_bytes,
        )


def _enqueue(
    root: str,
    text: str,
    *,
    message_id: str | None,
    serialize: bool,
    max_artifact_bytes: int | None = None,
) -> str:
    max_artifact_bytes = _effective_artifact_limit(max_artifact_bytes)
    if not text:
        raise AgentDeliveryError("message must not be empty")
    inbox, inflight, processed, failed = _prepare(root)
    identifier = f"{time.time_ns():020d}-{os.getpid()}" if message_id is None else message_id
    if _MESSAGE_ID.fullmatch(identifier) is None:
        raise AgentDeliveryError(
            "message id must be 1-255 ASCII letters, digits, dots, underscores, or hyphens "
            "and must start with a letter or digit"
        )
    filename = f"{identifier}.json"
    path = os.path.join(inbox, filename)
    descriptor = -1
    try:
        if serialize:
            directory_identity = _queue_directory_identity(root)
            lock_path = os.path.join(root, ".delivery.lock")
            descriptor = _open_private_lock(lock_path, "queue delivery lock")
            fcntl.flock(descriptor, fcntl.LOCK_EX)
            _verify_queue_lock(root, directory_identity, lock_path, descriptor, "queue delivery lock")
        if any(os.path.lexists(os.path.join(directory, filename)) for directory in (inbox, inflight, processed, failed)):
            raise AgentDeliveryError(f"message id already exists: {identifier}")
        try:
            _atomic_json_create(
                path,
                {
                    "id": identifier,
                    "text": text,
                    "queued_at": time.time(),
                    "delivery_attempts": 0,
                },
                max_artifact_bytes=max_artifact_bytes,
            )
        except FileExistsError as exc:
            raise AgentDeliveryError(f"message id already exists: {identifier}") from exc
    finally:
        if descriptor >= 0:
            os.close(descriptor)
    return identifier


def _binding(target: Target) -> dict[str, object]:
    """Identity authority for a queue: stable session when present, otherwise exact pane."""
    identity: dict[str, object]
    if target.session_value is not None:
        identity = {"kind": "session", "agent": target.session_agent, "value": target.session_value}
    else:
        identity = {"kind": "pane", "pane_id": target.pane_id}
    identity.update(
        {
            "expected_agent": target.expected_agent,
            "expected_workspace": target.expected_workspace,
            "expected_cwd": None if target.expected_cwd is None else _real(target.expected_cwd),
        }
    )
    return identity


def _same_binding_identity(left: object, right: object) -> bool:
    """Compare every durable delivery-identity assertion exactly."""
    return isinstance(left, dict) and isinstance(right, dict) and left == right


def _binding_identity_matches(
    actual: object, expected: object, *, allow_legacy_workspace_binding: bool,
) -> bool:
    """Accept an old managed label only for a deliberately unpinned managed target."""
    if _same_binding_identity(actual, expected):
        return True
    if (not allow_legacy_workspace_binding or not isinstance(actual, dict)
            or not isinstance(expected, dict)
            or expected.get("expected_workspace") is not None
            or not isinstance(actual.get("expected_workspace"), str)):
        return False
    compatible = dict(actual)
    compatible["expected_workspace"] = None
    return compatible == expected


def _target_lock_name(pane_id: str) -> str:
    """Return the lock file name for one resolved live pane, independent of its directory."""
    identity: dict[str, object] = {"kind": "pane", "pane_id": pane_id}
    # ``ensure_ascii=False`` pins the command's UTF-8 lock encoding for every resolved pane id.
    encoded = json.dumps(identity, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()
    return f"{hashlib.sha256(encoded).hexdigest()}.lock"


def _host_target_lock_root() -> str:
    """Return the directory every agentctl process for this user serializes pane delivery in.

    It is account state, resolved from the account database rather than ``$HOME`` or
    ``$TMPDIR``, so callers with different environments still meet on one file. It must not be
    under ``/tmp`` or ``/var/tmp``: the host's daily cleaner deletes files there by modification
    time, and a lock file is never written, so once the file was four days old the cleaner could
    delete it while it was held and a second sender would lock a fresh file at the same path.
    """
    return _target_lock_root_under(HerdrClient._account_home())


def _target_lock_root_under(home: str) -> str:
    """Return the account-state lock root under ``home``, which must be absolute.

    A relative home would resolve against each caller's working directory, so senders started
    in different directories would lock different files.
    """
    if not os.path.isabs(home):
        raise AgentDeliveryError(f"account home directory is not an absolute path: {home}")
    return os.path.join(home, ".local", "state", "agentctl", "target-locks")


def _legacy_host_target_lock_root() -> str:
    """Return the root earlier editions lock in. New editions lock it second."""
    return os.path.join("/tmp", f"herdr-agent-target-locks-{os.getuid()}")


def _host_target_lock_roots() -> tuple[str, str]:
    """Return the roots every agentctl process on the host locks in, in acquisition order."""
    return _host_target_lock_root(), _legacy_host_target_lock_root()


def _target_lock_paths(name: str) -> tuple[str, str]:
    """Return the lock files for one target name in acquisition order: account state, then legacy."""
    paths = []
    for lock_root, purpose in zip(
        _host_target_lock_roots(),
        ("host-wide target lock directory", "legacy host-wide target lock directory"),
    ):
        os.makedirs(lock_root, mode=0o700, exist_ok=True)
        _validate_private_directory(lock_root, purpose)
        paths.append(os.path.join(lock_root, _target_lock_name(name)))
    return paths[0], paths[1]


class _TargetLock:
    """One held host-wide target lock; closing it releases every file it holds.

    It holds the file under the account-state root and, while editions that know only the
    legacy ``/tmp`` root may still run, the legacy file as well, so a new and an old process
    still exclude each other. Every new process takes both in that fixed order and an old
    process takes only the legacy file, so the two orders cannot deadlock.
    """

    def __init__(self, descriptors: Sequence[int]) -> None:
        self._descriptors = list(descriptors)

    def close(self) -> None:
        while self._descriptors:
            os.close(self._descriptors.pop())

    def __enter__(self) -> "_TargetLock":
        return self

    def __exit__(self, *_exc: object) -> None:
        self.close()


def _flock_exclusive(descriptor: int, _path: str) -> None:
    fcntl.flock(descriptor, fcntl.LOCK_EX)


def _lock_target(
    name: str, purpose: str, wait: Callable[[int, str], None] = _flock_exclusive
) -> _TargetLock:
    """Open and lock every file of one target lock in order, waiting on each with ``wait``."""
    lock = _TargetLock(())
    try:
        paths = _target_lock_paths(name)
        for path in paths:
            _lock_current_file(path, purpose, lock._descriptors, wait)
        _refresh_lock_files_when_due(
            os.path.join(os.path.dirname(paths[0]), _LEGACY_REFRESH_MARKER),
            os.path.dirname(paths[1]),
        )
    except BaseException:
        lock.close()
        raise
    return lock


def _lock_target_within(name: str, purpose: str, seconds: float) -> _TargetLock:
    """Lock one target like ``_lock_target``, giving up after ``seconds`` instead of waiting.

    Used where the caller already holds another target lock, so it can never deadlock.
    """
    deadline = time.monotonic() + seconds

    def wait(descriptor: int, _path: str) -> None:
        while True:
            try:
                fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
                return
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    raise AgentDeliveryError(
                        f"{purpose} for {name} is held by another sender"
                    ) from None
                time.sleep(0.05)

    return _lock_target(name, purpose, wait)


def _lock_current_file(
    path: str,
    purpose: str,
    held: list[int],
    wait: Callable[[int, str], None] = _flock_exclusive,
) -> None:
    """Lock the file at ``path``, retrying until it is still the file the path names.

    The descriptor is appended to ``held`` as soon as it is opened, before it is locked, so the
    caller owns it, and releases it on any failure, from the moment it can hold a lock.

    The host's daily cleaner deletes ``/tmp`` files by modification time, so it can unlink a
    lock file while a sender waits on it. A sender that then took the lock would hold a file no
    other process can open, while the next sender locked a fresh file at the same path; the
    check after locking sends it back to contend on the file that is there. The file's
    modification time is then set to now.
    """
    while True:
        held.append(_open_private_lock(path, purpose))
        descriptor = held[-1]
        wait(descriptor, path)
        locked = os.fstat(descriptor)
        try:
            current = os.stat(path, follow_symlinks=False)
        except FileNotFoundError:
            current = None
        if current is not None and (current.st_dev, current.st_ino) == (
            locked.st_dev,
            locked.st_ino,
        ):
            os.utime(descriptor)
            return
        os.close(held.pop())


# The account-state file whose modification time records the last refresh of the /tmp lock
# files. It lives outside /tmp, so the cleaner never removes it.
_LEGACY_REFRESH_MARKER = "legacy-locks-refreshed"
# How often senders refresh the /tmp lock files: far inside the cleaner's four days, and rare
# enough that a steady stream of deliveries does not scan a directory each time.
_LEGACY_REFRESH_SECONDS = 60 * 60


def _refresh_lock_files_when_due(marker: str, directory: str) -> None:
    """Refresh every lock file in ``directory`` unless ``marker`` shows a refresh within the
    last ``_LEGACY_REFRESH_SECONDS``. A missing marker, or one dated in the future, is due."""
    now = time.time()
    try:
        age: float | None = now - os.stat(marker).st_mtime
    except OSError:
        age = None
    if age is not None and 0 <= age < _LEGACY_REFRESH_SECONDS:
        return
    # A marker that cannot be written only means the next sender refreshes again.
    try:
        descriptor = os.open(marker, os.O_WRONLY | os.O_CREAT | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
        try:
            os.utime(descriptor, (now, now))
        finally:
            os.close(descriptor)
    except OSError:
        pass
    _refresh_lock_files(directory)


def _refresh_lock_files(directory: str) -> None:
    """Set the modification time of every lock file in ``directory`` to now.

    Refreshing only the file a sender locks leaves two gaps. The cleaner examines a file and
    deletes it a moment later, so a file that was four days old when examined can be deleted
    just after a sender refreshed and locked it. And a target no one sends to for four days
    ages out while a process that takes only the ``/tmp`` file may still open it. Refreshing
    the whole directory at least hourly means no file there reaches four days while any
    process that takes both files delivers on the host. Failures are ignored: a file that another
    sender is creating or the cleaner is deleting needs nothing from this sender, whose own
    lock is already valid.
    """
    try:
        entries = list(os.scandir(directory))
    except OSError:
        return
    for entry in entries:
        if entry.name.endswith(".lock"):
            try:
                os.utime(entry.path, follow_symlinks=False)
            except OSError:
                pass


def _lock_resolved_target(
    client: HerdrClient, target: Target
) -> tuple[_TargetLock, str, AgentPaneInfo]:
    """Lock the initially resolved pane and prove the target did not move while waiting."""

    initial = resolve_target(client, target)
    lock = _lock_target(initial.pane_id, "host-wide target lock")
    keep_open = False
    try:
        confirmed = resolve_target(client, target)
        if confirmed.pane_id != initial.pane_id:
            raise AgentDeliveryError(
                f"target moved from pane {initial.pane_id!r} to {confirmed.pane_id!r} "
                "while waiting for its host-wide lock"
            )
        keep_open = True
        return lock, initial.pane_id, confirmed
    finally:
        if not keep_open:
            lock.close()


def _bind_queue(
    root: str, target: Target, *, max_artifact_bytes: int | None = None,
    allow_legacy_workspace_binding: bool = False,
) -> tuple[int, int]:
    """Create or verify the durable queue-to-target binding under its own lock."""
    if not (target.pane_id or target.session_value):
        raise AgentDeliveryError("target needs --pane or a stable session value")
    max_artifact_bytes = _effective_artifact_limit(max_artifact_bytes)
    _prepare(root)
    directory_identity = _queue_directory_identity(root)
    lock_path = os.path.join(root, ".binding.lock")
    binding_path = os.path.join(root, "target.json")
    descriptor = _open_private_lock(lock_path, "queue binding lock")
    expected = _binding(target)
    try:
        fcntl.flock(descriptor, fcntl.LOCK_EX)
        _verify_queue_lock(root, directory_identity, lock_path, descriptor, "queue binding lock")
        policy = _ACTIVE_ATOMIC_POLICY.get()
        if policy is not None:
            # Bootstrap binding is read before the delivery lock/recovery
            # phase. Finish any interrupted publication under binding ->
            # atomic before deciding whether to create or read target.json.
            with atomic_write_recovery(policy):
                pass
        if os.path.lexists(binding_path):
            actual = _read_queue_json(
                binding_path, "queue target binding", require_private=True,
                max_artifact_bytes=max_artifact_bytes,
            )
            if not _binding_identity_matches(
                actual, expected,
                allow_legacy_workspace_binding=allow_legacy_workspace_binding,
            ):
                raise AgentDeliveryError(
                    _bounded_error(
                        f"queue {root} is bound to {actual!r}, refusing different target {expected!r}",
                        max_artifact_bytes,
                    )
                )
        else:
            _atomic_json(binding_path, expected, max_artifact_bytes=max_artifact_bytes)
    finally:
        os.close(descriptor)
    return directory_identity


def rebind_queue(root: str, previous: Target, replacement: Target) -> None:
    """Replace an exact queue binding while excluding concurrent delivery.

    Accepting an already-installed replacement makes recovery idempotent when
    the binding commit succeeded but the surrounding agent-record commit did
    not.
    """
    for target in (previous, replacement):
        if not (target.pane_id or target.session_value):
            raise AgentDeliveryError("target needs --pane or a stable session value")
    _prepare(root)
    directory_identity = _queue_directory_identity(root)
    delivery_path = os.path.join(root, ".delivery.lock")
    binding_lock_path = os.path.join(root, ".binding.lock")
    binding_path = os.path.join(root, "target.json")
    delivery_descriptor = _open_private_lock(delivery_path, "queue delivery lock")
    binding_descriptor = -1
    try:
        fcntl.flock(delivery_descriptor, fcntl.LOCK_EX)
        _verify_queue_lock(root, directory_identity, delivery_path, delivery_descriptor, "queue delivery lock")
        binding_descriptor = _open_private_lock(
            binding_lock_path, "queue binding lock"
        )
        fcntl.flock(binding_descriptor, fcntl.LOCK_EX)
        _verify_queue_lock(root, directory_identity, binding_lock_path, binding_descriptor, "queue binding lock")
        old = _binding(previous)
        new = _binding(replacement)
        if os.path.lexists(binding_path):
            actual = _read_queue_json(
                binding_path, "queue target binding", require_private=True,
            )
            if (not _same_binding_identity(actual, old)
                    and not _same_binding_identity(actual, new)):
                raise AgentDeliveryError(
                    f"queue {root} is bound to {actual!r}, refusing move "
                    f"from {old!r} to {new!r}"
                )
            if actual != new:
                _atomic_json(binding_path, new)
        else:
            _atomic_json(binding_path, new)
    finally:
        if binding_descriptor >= 0:
            os.close(binding_descriptor)
        os.close(delivery_descriptor)


def rebind_queue_after(
    root: str,
    previous: Target,
    operation: Callable[[], tuple[Target, object]],
    *, already_replacement: Target | None = None,
    allow_legacy_workspace_binding: bool = False,
) -> object:
    """Run an irreversible move only after proving its queue can be rebound.

    The queue delivery and binding locks remain held across ``operation``.  This
    gives move the same delivery-lock-before-target-lock order as drain and
    prevents a foreign binding from being discovered after Herdr changed state.
    """
    if not (previous.pane_id or previous.session_value):
        raise AgentDeliveryError("target needs --pane or a stable session value")
    _prepare(root)
    directory_identity = _queue_directory_identity(root)
    delivery_path = os.path.join(root, ".delivery.lock")
    binding_lock_path = os.path.join(root, ".binding.lock")
    binding_path = os.path.join(root, "target.json")
    delivery_descriptor = _open_private_lock(delivery_path, "queue delivery lock")
    binding_descriptor = -1
    try:
        fcntl.flock(delivery_descriptor, fcntl.LOCK_EX)
        _verify_queue_lock(root, directory_identity, delivery_path, delivery_descriptor, "queue delivery lock")
        binding_descriptor = _open_private_lock(binding_lock_path, "queue binding lock")
        fcntl.flock(binding_descriptor, fcntl.LOCK_EX)
        _verify_queue_lock(root, directory_identity, binding_lock_path, binding_descriptor, "queue binding lock")
        old = _binding(previous)
        if os.path.lexists(binding_path):
            actual = _read_queue_json(
                binding_path, "queue target binding", require_private=True,
            )
            replacement_allowed = (
                already_replacement is not None
                and _same_binding_identity(actual, _binding(already_replacement))
            )
            if (not _binding_identity_matches(
                    actual, old,
                    allow_legacy_workspace_binding=allow_legacy_workspace_binding,
                ) and not replacement_allowed):
                raise AgentDeliveryError(
                    f"queue {root} is bound to {actual!r}, refusing move from {old!r}"
                )
        replacement, result = operation()
        if not (replacement.pane_id or replacement.session_value):
            raise AgentDeliveryError("target needs --pane or a stable session value")
        new = _binding(replacement)
        _atomic_json(binding_path, new)
        return result
    finally:
        if binding_descriptor >= 0:
            os.close(binding_descriptor)
        os.close(delivery_descriptor)


def _validate_existing_binding(
    root: str, target: Target, *, allow_legacy_workspace_binding: bool = False,
) -> None:
    """Read-only binding validation for observational commands such as status."""

    if not os.path.lexists(root):
        return
    _validate_private_directory(root, "queue directory")
    binding_path = os.path.join(root, "target.json")
    if not os.path.lexists(binding_path):
        return
    expected = _binding(target)
    actual = _read_queue_json(binding_path, "queue target binding", require_private=True)
    if not _binding_identity_matches(
        actual, expected,
        allow_legacy_workspace_binding=allow_legacy_workspace_binding,
    ):
        raise AgentDeliveryError(
            f"queue {root} is bound to {actual!r}, refusing different target {expected!r}"
        )


def _validate_existing_queue(root: str) -> None:
    """Validate every existing queue directory without creating or tightening it."""

    if not os.path.lexists(root):
        return
    _validate_private_directory(root, "queue directory")
    for path in _dirs(root):
        if os.path.lexists(path):
            _validate_private_directory(path, "queue state directory")


def _check_artifact_size(path: str, max_artifact_bytes: int | None) -> None:
    if max_artifact_bytes is not None and os.stat(path, follow_symlinks=False).st_size > max_artifact_bytes:
        raise _ArtifactTooLarge(f"queue artifact exceeds max_artifact_bytes={max_artifact_bytes}: {path}")


def _transition(source: str, destination: str, *, max_artifact_bytes: int | None = None) -> None:
    """Durably rename an artifact and sync both directory entries."""
    source_parent = os.path.dirname(source)
    destination_parent = os.path.dirname(destination)
    _check_artifact_size(source, max_artifact_bytes)
    if os.path.lexists(destination):
        raise AgentDeliveryError(f"refusing to replace durable queue artifact {destination}")
    os.replace(source, destination)
    _fsync_dir(destination_parent)
    if source_parent != destination_parent:
        _fsync_dir(source_parent)


def _failure_document(
    failed_path: str, *, outcome: str, error: str, max_artifact_bytes: int | None,
) -> dict[str, object]:
    return {
        "artifact": os.path.basename(failed_path), "outcome": outcome,
        "error": _bounded_error(error, max_artifact_bytes), "failed_at": time.time(),
    }


def _failed_metadata(
    failed_path: str, *, outcome: str, error: str, max_artifact_bytes: int | None = None,
) -> None:
    _atomic_json(
        failed_path + ".error",
        _failure_document(failed_path, outcome=outcome, error=error, max_artifact_bytes=max_artifact_bytes),
        max_artifact_bytes=max_artifact_bytes,
    )


def _quarantine_raw(
    path: str, failed: str, *, outcome: str, error: str, max_artifact_bytes: int | None = None,
) -> str:
    """Preserve malformed bytes exactly, add separate durable metadata, and advance FIFO."""
    basename = os.path.basename(path)
    destination = os.path.join(failed, basename)
    metadata = _failure_document(
        destination, outcome=outcome, error=error, max_artifact_bytes=max_artifact_bytes,
    )
    if max_artifact_bytes is not None:
        _serialized_json(metadata, max_artifact_bytes)
    _transition(path, destination, max_artifact_bytes=max_artifact_bytes)
    _atomic_json(destination + ".error", metadata, max_artifact_bytes=max_artifact_bytes)
    return basename[:-5] if basename.endswith(".json") else basename


def _recover_inflight(inflight: str, failed: str, *, max_artifact_bytes: int | None = None) -> list[str]:
    """Never resubmit a prompt whose process died after durable injection intent."""
    recovered: list[str] = []
    for name in sorted(entry for entry in os.listdir(inflight) if entry.endswith(".json")):
        source = os.path.join(inflight, name)
        _quarantine_raw(
            source, failed,
            outcome="possibly_submitted",
            error="recovered an inflight prompt after process restart; refusing automatic resubmission",
            max_artifact_bytes=max_artifact_bytes,
        )
        recovered.append(name[:-5])
    return recovered


def _load(path: str, *, max_artifact_bytes: int | None = None) -> dict[str, object]:
    raw = _read_queue_json(
        path, "queued message", require_private=False,
        max_artifact_bytes=max_artifact_bytes,
    )
    if (
        not isinstance(raw, dict)
        or not isinstance(raw.get("text"), str)
        or not raw["text"]
    ):
        raise AgentDeliveryError(
            f"queued message {path} must have a nonempty string text field"
        )
    if "id" in raw and not isinstance(raw["id"], str):
        raise AgentDeliveryError(f"queued message {path} has a non-string id field")
    return {str(key): value for key, value in raw.items()}


def _queue_order(path: str, max_artifact_bytes: int | None) -> tuple[bool, float, str]:
    """Sort key that makes ``drain`` take the inbox in the order its prompts were queued.

    Entries go by ``queued_at``, then by file name. A chat request's file is named
    for its key, a hash, so name order alone is not the order of arrival. An entry
    without a finite numeric ``queued_at`` goes first, by name: one in the subagent
    message shape that ``agentctl.foreign`` writes, which numbers its file names in
    sequence and records ``queued_at`` as text, if at all, or one that cannot be
    read, which the drain loop then quarantines, or refuses when it exceeds
    ``max_artifact_bytes``, as before.
    """
    try:
        document = _read_queue_json(
            path, "queued message", require_private=False,
            max_artifact_bytes=max_artifact_bytes,
        )
    except AgentDeliveryError:
        return (False, 0.0, path)
    queued_at = document.get("queued_at") if isinstance(document, dict) else None
    if isinstance(queued_at, bool) or not isinstance(queued_at, (int, float)):
        return (False, 0.0, path)
    try:
        seconds = float(queued_at)
    except OverflowError:
        return (False, 0.0, path)
    if not math.isfinite(seconds):
        return (False, 0.0, path)
    return (True, seconds, path)


def _typed_text(document: dict[str, object]) -> str:
    """The text a drain types: the stored text, except that a chat request prompt the Rust bridge
    queued with its message's create time gets its opening decided now (see prompt_time)."""
    return _retime_prompt(str(document["text"]), document.get("sent_at"), document.get("opening"))


def _delivery_attempts(document: dict[str, object], path: str) -> int:
    """Read the current or legacy attempt count as one strict unsigned 64-bit integer."""

    key = "delivery_attempts" if "delivery_attempts" in document else "tui_delivery_attempts"
    if key not in document:
        return 0
    value = document[key]
    if type(value) is not int or not 0 <= value <= _U64_MAX:
        raise AgentDeliveryError(
            f"queued message {path} has an invalid nonnegative integer {key} field"
        )
    return value


def _wait_ready(
    client: HerdrClient,
    target: Target,
    timeout: float,
    *,
    sleep: Callable[[float], None],
    monotonic: Callable[[], float],
    locked_pane_id: str,
    initial_info: AgentPaneInfo | None = None,
) -> AgentPaneInfo:
    deadline = monotonic() + timeout
    while True:
        info = initial_info if initial_info is not None else resolve_target(client, target)
        initial_info = None
        if info.pane_id != locked_pane_id:
            raise AgentDeliveryError(
                f"target moved from locked pane {locked_pane_id!r} to {info.pane_id!r}"
            )
        if info.status in ("idle", "done"):
            return info
        codex_ready = getattr(client, "codex_idle_ready", None)
        if (info.agent == "codex" and info.status == "unknown"
                and callable(codex_ready) and codex_ready(info)):
            return info
        if info.status == "blocked":
            raise AgentDeliveryError(f"pane {info.pane_id} is blocked; resolve its visible prompt")
        if monotonic() >= deadline:
            raise AgentDeliveryError(
                f"pane {info.pane_id} did not become idle/done within {timeout:g}s; last status={info.status}"
            )
        sleep(min(0.25, max(0.0, deadline - monotonic())))


def _deliver_one(
    client: HerdrClient,
    info: AgentPaneInfo,
    text: str,
    *,
    working_timeout: float,
) -> None:
    try:
        receipt = client.prompt_agent(info.pane_id, text)
    except PromptNotStaged as exc:
        raise _NotStaged(str(exc)) from exc
    except MisrouteRecovered as exc:
        raise _Misrouted(str(exc)) from exc
    except ProbableMisroute as exc:
        raise _PossiblySubmitted(
            f"pane {info.pane_id}: PROBABLE MISROUTE, quarantined: {exc}", misroute=True,
        ) from exc
    except Exception as exc:
        # The terminal server may have accepted the atomic text+Enter before the client lost its
        # response. Once agent.prompt is entered, failure is ambiguous and must never be retried.
        raise _PossiblySubmitted(
            f"pane {info.pane_id} agent-prompt outcome is unknown; prompt may have been submitted: {exc}"
        ) from exc
    if isinstance(receipt, SubmissionReceipt):
        # The screen already proved the prompt left the composer. A lifecycle
        # transition adds nothing and is absent when the agent queues the prompt.
        return
    try:
        client.wait_agent_status(info.pane_id, "working", max(1, int(working_timeout * 1000)))
    except MisrouteRecovered as exc:
        # A confirmation key (such as a goal-replacement Enter) reached the wrong program.
        raise _Misrouted(str(exc)) from exc
    except ProbableMisroute as exc:
        raise _PossiblySubmitted(
            f"pane {info.pane_id}: PROBABLE MISROUTE, quarantined: {exc}", misroute=True,
        ) from exc
    except (HerdrUnavailable, AgentDeliveryError) as exc:
        raise _PossiblySubmitted(
            f"pane {info.pane_id} did not confirm idle/done -> working submission: {exc}"
        ) from exc


def drain(
    client: HerdrClient,
    target: Target,
    root: str,
    *,
    ready_timeout: float = 900.0,
    working_timeout: float = 30.0,
    max_attempts: int = 3,
    max_artifact_bytes: int | None = None,
    sleep: Callable[[float], None] = time.sleep,
    monotonic: Callable[[], float] = time.monotonic,
    atomic_policy: AtomicWritePolicy | None = None,
    allow_legacy_workspace_binding: bool = False,
) -> QueueResult:
    """Drain a queue, optionally sharing a bounded atomic staging domain."""
    with atomic_write_policy(atomic_policy):
        return _drain(client, target, root, ready_timeout=ready_timeout,
                      working_timeout=working_timeout, max_attempts=max_attempts,
                      max_artifact_bytes=max_artifact_bytes, sleep=sleep,
                      monotonic=monotonic,
                      allow_legacy_workspace_binding=allow_legacy_workspace_binding)


def _drain(
    client: HerdrClient, target: Target, root: str, *, ready_timeout: float,
    working_timeout: float, max_attempts: int, max_artifact_bytes: int | None,
    sleep: Callable[[float], None], monotonic: Callable[[], float],
    allow_legacy_workspace_binding: bool,
) -> QueueResult:
    """Serialize and drain a FIFO; poison prompts are retained in ``failed``.

    An optional byte limit applies to message, target, and error-sidecar JSON
    writes before temporary files are created. Bounded diagnostics retain at most
    ``QUEUE_ERROR_MAX_BYTES`` UTF-8 bytes. Oversized existing artifacts are left
    intact; a failed post-submission update retains the durable inflight barrier.
    """
    max_artifact_bytes = _effective_artifact_limit(max_artifact_bytes)
    directory_identity = _bind_queue(
        root, target, max_artifact_bytes=max_artifact_bytes,
        allow_legacy_workspace_binding=allow_legacy_workspace_binding,
    )
    inbox, inflight, processed, failed = _dirs(root)
    lock_path = os.path.join(root, ".delivery.lock")
    delivered: list[str] = []
    quarantined: list[str] = []
    blocked: str | None = None
    descriptor = _open_private_lock(lock_path, "queue delivery lock")
    target_lock: _TargetLock | None = None
    locked_pane_id = ""
    initial_info: AgentPaneInfo | None = None
    try:
        fcntl.flock(descriptor, fcntl.LOCK_EX)
        _verify_queue_lock(root, directory_identity, lock_path, descriptor, "queue delivery lock")
        policy = _ACTIVE_ATOMIC_POLICY.get()
        if policy is not None:
            # enqueue's create-only final may still share the fixed slot after
            # a crash. Recover under delivery -> atomic before bounded reads
            # can mistake that committed prompt for an unsafe hardlink.
            with atomic_write_recovery(policy):
                pass
        quarantined.extend(_recover_inflight(inflight, failed, max_artifact_bytes=max_artifact_bytes))
        for path in sorted(
            (os.path.join(inbox, name) for name in os.listdir(inbox) if name.endswith(".json")),
            key=lambda path: _queue_order(path, max_artifact_bytes),
        ):
            _check_artifact_size(path, max_artifact_bytes)
            try:
                document = _load(path, max_artifact_bytes=max_artifact_bytes)
                attempts = _delivery_attempts(document, path)
            except AgentDeliveryError as exc:
                quarantined.append(
                    _quarantine_raw(
                        path, failed, outcome="invalid_message", error=str(exc),
                        max_artifact_bytes=max_artifact_bytes,
                    )
                )
                continue
            recorded_error = document.get("delivery_error")
            if isinstance(recorded_error, str):
                document["delivery_error"] = _bounded_error(recorded_error, max_artifact_bytes)
            identifier = str(document.get("id", os.path.basename(path)[:-5]))
            if attempts >= max_attempts:
                blocked = _bounded_error(
                    f"message {identifier} reached the maximum delivery-attempt count "
                    f"({attempts} >= {max_attempts}); retained pending",
                    max_artifact_bytes,
                )
                if (document.get("delivery_state") != "pending"
                        or document.get("delivery_error") != blocked):
                    document["delivery_state"] = "pending"
                    document["delivery_error"] = blocked
                    document["delivery_blocked_at"] = time.time()
                    _atomic_json(path, document, max_artifact_bytes=max_artifact_bytes)
                break
            while attempts < max_attempts:
                # Readiness is entirely pre-injection. Keep the artifact in inbox while the pane
                # is busy so a process death during an ordinary wait remains safely retryable.
                try:
                    if target_lock is None:
                        # Resolve before choosing the lock so exact-pane and stable-session
                        # callers serialize on the same live pane. Re-resolve after acquisition
                        # and on every readiness poll so a moving session cannot escape the lock.
                        target_lock, locked_pane_id, initial_info = _lock_resolved_target(
                            client, target
                        )
                    info = _wait_ready(
                        client,
                        target,
                        ready_timeout,
                        sleep=sleep,
                        monotonic=monotonic,
                        locked_pane_id=locked_pane_id,
                        initial_info=initial_info,
                    )
                    initial_info = None
                except (AgentDeliveryError, HerdrUnavailable) as exc:
                    blocked = _bounded_error(str(exc), max_artifact_bytes)
                    if (document.get("delivery_state") != "pending"
                            or document.get("delivery_error") != blocked):
                        document["delivery_state"] = "pending"
                        document["delivery_error"] = blocked
                        document["delivery_blocked_at"] = time.time()
                        _atomic_json(path, document, max_artifact_bytes=max_artifact_bytes)
                    break
                # Rendered once before the barrier below, so a failure in it leaves the prompt
                # pending, and again right before typing, after the durable writes, so the delay
                # it states is the delay at typing.
                _typed_text(document)
                # ``inflight`` is a durable at-most-once barrier. Once this rename commits, a
                # crash is treated as possibly submitted. Readiness was already proven above;
                # this transition occurs immediately before pane.run.
                inflight_path = os.path.join(inflight, os.path.basename(path))
                # Rename first: a crash at every later instruction leaves the artifact in the
                # restart-quarantined directory. Updating the inbox file before this rename
                # would leave a small but real restart-resubmission window.
                document["possibly_submitted"] = True
                document["delivery_state"] = "inflight"
                document["inflight_at"] = time.time()
                if max_artifact_bytes is not None:
                    _serialized_json(document, max_artifact_bytes)
                _transition(path, inflight_path, max_artifact_bytes=max_artifact_bytes)
                _atomic_json(inflight_path, document, max_artifact_bytes=max_artifact_bytes)
                retained_path = inflight_path
                try:
                    try:
                        _deliver_one(
                            client, info, _typed_text(document), working_timeout=working_timeout,
                        )
                    except _NotStaged as exc:
                        # Nothing reached the composer: undo the at-most-once barrier so the
                        # prompt stays pending. A crash before the rename still quarantines it.
                        blocked = _bounded_error(str(exc), max_artifact_bytes)
                        document.pop("possibly_submitted", None)
                        document.pop("inflight_at", None)
                        document["delivery_state"] = "pending"
                        document["delivery_error"] = blocked
                        document["delivery_blocked_at"] = time.time()
                        _atomic_json(inflight_path, document, max_artifact_bytes=max_artifact_bytes)
                        _transition(inflight_path, path, max_artifact_bytes=max_artifact_bytes)
                        retained_path = path
                        break
                    except _Misrouted as exc:
                        # The wrong program was interrupted and told to ignore the prompt.
                        # Retry from readiness, which re-resolves and re-verifies the
                        # recipient; quarantine once the same message misroutes again.
                        recorded = document.get("misroutes", [])
                        misroutes = list(recorded) if isinstance(recorded, list) else []
                        misroutes.append({
                            "at": time.time(),
                            "error": _bounded_error(str(exc), max_artifact_bytes),
                        })
                        document["misroutes"] = misroutes
                        if len(misroutes) >= MAX_MISROUTES:
                            attempts += 1
                            document["delivery_attempts"] = attempts
                            document["tui_delivery_attempts"] = attempts
                            document["delivery_error"] = _bounded_error(str(exc), max_artifact_bytes)
                            document["probable_misroute"] = True
                            document["delivery_failed_at"] = time.time()
                            _atomic_json(inflight_path, document, max_artifact_bytes=max_artifact_bytes)
                            failed_path = os.path.join(failed, os.path.basename(path))
                            _transition(inflight_path, failed_path, max_artifact_bytes=max_artifact_bytes)
                            retained_path = failed_path
                            _failed_metadata(
                                failed_path, outcome="possibly_submitted", error=str(exc),
                                max_artifact_bytes=max_artifact_bytes,
                            )
                            quarantined.append(identifier)
                            break
                        document.pop("possibly_submitted", None)
                        document.pop("inflight_at", None)
                        document["delivery_state"] = "pending"
                        document["delivery_error"] = _bounded_error(str(exc), max_artifact_bytes)
                        _atomic_json(inflight_path, document, max_artifact_bytes=max_artifact_bytes)
                        _transition(inflight_path, path, max_artifact_bytes=max_artifact_bytes)
                        retained_path = path
                        # The retry does not consume an attempt: the misroute count bounds it.
                        continue
                    except _PossiblySubmitted as exc:
                        attempts += 1
                        document["delivery_attempts"] = attempts
                        document["tui_delivery_attempts"] = attempts
                        document["delivery_error"] = _bounded_error(str(exc), max_artifact_bytes)
                        document["possibly_submitted"] = True
                        if exc.misroute:
                            document["probable_misroute"] = True
                        document["delivery_failed_at"] = time.time()
                        _atomic_json(inflight_path, document, max_artifact_bytes=max_artifact_bytes)
                        failed_path = os.path.join(failed, os.path.basename(path))
                        _transition(inflight_path, failed_path, max_artifact_bytes=max_artifact_bytes)
                        retained_path = failed_path
                        _failed_metadata(
                            failed_path, outcome="possibly_submitted", error=str(exc),
                            max_artifact_bytes=max_artifact_bytes,
                        )
                        quarantined.append(identifier)
                        break
                    else:
                        document["delivery_state"] = "processed"
                        document["confirmed_at"] = time.time()
                        _atomic_json(inflight_path, document, max_artifact_bytes=max_artifact_bytes)
                        _transition(
                            inflight_path, os.path.join(processed, os.path.basename(path)),
                            max_artifact_bytes=max_artifact_bytes,
                        )
                        delivered.append(identifier)
                        break
                except _ArtifactTooLarge as exc:
                    raise AgentPossiblySubmitted(
                        _bounded_error(
                            f"message {identifier} may have been submitted before its delivery record "
                            f"could be saved: {exc}; retained at {retained_path}; automatic resubmission is unsafe",
                            max_artifact_bytes,
                        ),
                        message_id=identifier, artifact=retained_path,
                    ) from exc
            if blocked is not None:
                break
    finally:
        if target_lock is not None:
            target_lock.close()
        os.close(descriptor)
    pending = tuple(name[:-5] for name in sorted(os.listdir(inbox)) if name.endswith(".json"))
    outcome = "pending" if blocked is not None else ("possibly_submitted" if quarantined else "delivered")
    return QueueResult("", tuple(delivered), tuple(quarantined), pending, blocked, outcome)


def send(
    client: HerdrClient, target: Target, root: str, text: str, *, message_id: str | None = None,
    **kwargs: object,
) -> QueueResult:
    """Durably enqueue one prompt, drain its bound FIFO, and return confirmed delivery."""
    atomic_policy = kwargs.pop("atomic_policy", None)
    if atomic_policy is not None and not isinstance(atomic_policy, AtomicWritePolicy):
        raise AgentDeliveryError("atomic_policy must be an AtomicWritePolicy or None")
    with atomic_write_policy(atomic_policy):
        return _send(client, target, root, text, message_id=message_id,
                     atomic_policy=_ACTIVE_ATOMIC_POLICY.get(), **kwargs)


def _send(
    client: HerdrClient, target: Target, root: str, text: str, *, message_id: str | None,
    atomic_policy: AtomicWritePolicy | None, **kwargs: object,
) -> QueueResult:

    max_artifact_bytes = kwargs.pop("max_artifact_bytes", None)
    if max_artifact_bytes is not None and not isinstance(max_artifact_bytes, int):
        raise AgentDeliveryError("max_artifact_bytes must be a positive integer or None")
    max_artifact_bytes = _effective_artifact_limit(max_artifact_bytes)
    allow_legacy_workspace_binding = kwargs.pop(
        "allow_legacy_workspace_binding", False
    )
    if not isinstance(allow_legacy_workspace_binding, bool):
        raise AgentDeliveryError("allow_legacy_workspace_binding must be boolean")
    _bind_queue(
        root, target, max_artifact_bytes=max_artifact_bytes,
        allow_legacy_workspace_binding=allow_legacy_workspace_binding,
    )
    # Generated identifiers are collision-resistant and the no-replace inbox create is atomic.
    # Do not wait behind a long-running drain merely to persist a new prompt; the subsequent drain
    # and terminal-artifact inspection resolve any cross-sender consumption safely.
    # A caller-selected ID can move out of inbox while another caller is checking
    # it. Serialize that check with delivery, so the same ID cannot be recreated.
    identifier = _enqueue(
        root, text, message_id=message_id, serialize=message_id is not None,
        max_artifact_bytes=max_artifact_bytes,
    )
    result = drain(client, target, root, max_artifact_bytes=max_artifact_bytes,
                   atomic_policy=atomic_policy,
                   allow_legacy_workspace_binding=allow_legacy_workspace_binding,
                   **kwargs)  # type: ignore[arg-type]
    filename = f"{identifier}.json"
    failed_path = os.path.join(root, "failed", filename)
    if identifier in result.quarantined or os.path.lexists(failed_path):
        detail = "unknown delivery failure"
        try:
            failed_document = _load(failed_path, max_artifact_bytes=max_artifact_bytes)
            recorded = failed_document.get("delivery_error")
            if isinstance(recorded, str):
                detail = _bounded_error(recorded, max_artifact_bytes)
        except AgentDeliveryError:
            pass
        raise AgentPossiblySubmitted(
            _bounded_error(
                f"message {identifier} has an ambiguous outcome after one injection: {detail}; "
                f"it is retained under {root}/failed",
                max_artifact_bytes,
            ),
            message_id=identifier,
            artifact=failed_path,
        )
    inflight_path = os.path.join(root, "inflight", filename)
    if os.path.lexists(inflight_path):
        raise AgentPossiblySubmitted(
            _bounded_error(
                f"message {identifier} remains behind the durable inflight barrier; "
                "automatic resubmission is unsafe",
                max_artifact_bytes,
            ),
            message_id=identifier,
            artifact=inflight_path,
        )
    inbox_path = os.path.join(root, "inbox", filename)
    if identifier in result.pending or os.path.lexists(inbox_path):
        raise AgentPending(
            _bounded_error(
                f"message {identifier} remains pending without consuming a retry attempt: {result.blocked}",
                max_artifact_bytes,
            ),
            message_id=identifier,
            artifact=inbox_path,
        )
    processed_path = os.path.join(root, "processed", filename)
    if identifier in result.delivered or os.path.lexists(processed_path):
        delivered = result.delivered
        if identifier not in delivered:
            delivered = (*delivered, identifier)
        return QueueResult(
            identifier,
            delivered,
            result.quarantined,
            result.pending,
            result.blocked,
            "delivered",
        )
    raise AgentDeliveryError(
        _bounded_error(f"message {identifier} disappeared without a durable terminal artifact", max_artifact_bytes)
    )


def status(
    client: HerdrClient, target: Target, root: str, *,
    allow_legacy_workspace_binding: bool = False,
) -> dict[str, object]:
    """Read validated live-agent and queue state without creating or changing queue files."""

    _validate_existing_queue(root)
    _validate_existing_binding(
        root, target,
        allow_legacy_workspace_binding=allow_legacy_workspace_binding,
    )
    info = resolve_target(client, target)
    inbox, inflight, _processed, failed = _dirs(root)

    def identifiers(path: str) -> list[str]:
        if not os.path.lexists(path):
            return []
        _validate_private_directory(path, "queue state directory")
        return sorted(name[:-5] for name in os.listdir(path) if name.endswith(".json"))

    return {
        "pane_id": info.pane_id, "agent": info.agent, "agent_status": info.status,
        "session_agent": info.session_agent, "session_value": info.session_value,
        "workspace_id": info.workspace_id, "cwd": info.cwd,
        "pending": identifiers(inbox),
        "inflight": identifiers(inflight),
        "failed": identifiers(failed),
    }


def read(client: HerdrClient, target: Target, *, lines: int = 500) -> str:
    """Read recent terminal output from a validated interactive-agent target."""

    info = resolve_target(client, target)
    text = client.read(info.pane_id, source="recent-unwrapped", lines=lines)
    return text if text else client.read(info.pane_id, source="recent", lines=lines)
