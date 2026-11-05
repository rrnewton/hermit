#!/usr/bin/env python3
"""Durable, single-coordinator chat bridge; transport and harness stay independent.

Run with ``python -m agentctl.chat``. A command adapter exchanges one JSON request
and response on stdin/stdout. The built-in Google Chat adapter uses the public API.
"""

from __future__ import annotations

import argparse
import ctypes
import fcntl
import hashlib
import json
import os
import re
import secrets
import selectors
import shlex
import shutil
import signal
import socket
import stat
import subprocess
import sys
import threading
import time
import uuid
from collections import Counter
from collections.abc import Callable, Iterable, Sequence
from dataclasses import asdict, dataclass
from datetime import datetime, timedelta, timezone
from functools import cached_property
from pathlib import Path
from typing import IO, Protocol
from urllib.parse import urlencode
from urllib.request import Request, urlopen

_PKG_PARENT = str(Path(__file__).resolve().parent.parent)
if _PKG_PARENT not in sys.path:
    sys.path.insert(0, _PKG_PARENT)

from agentctl import __version__
from agentctl.agent import (
    AtomicWritePolicy, atomic_write_policy, atomic_write_recovery,
    QUEUE_ERROR_SIDECAR_MAX_BYTES, QUEUE_UPDATE_MAX_BYTES,
    Target, _atomic_json, _atomic_policy_directory, _recover_atomic_slot,
    _fsync_dir, _open_private_lock,
    _validate_existing_queue, _validate_private_directory, drain, enqueue, resolve_target,
    queue_artifact_reservation_bytes,
)
from agentctl.client import AgentPaneInfo, GuardedInput, HerdrClient, Pane
from agentctl.chat_output import PaneAgentStatus, PaneOutputSnapshot, PaneOutputStream
from agentctl.chat_storage import (
    CONFIG, LEGACY_REPLY, REPLY_INPUT, REQUEST, REST_RESPONSE, ArtifactClass,
    decode_json, encoded_json, read_json, read_text, write_json,
)
from agentctl.chat_replies import (
    extract_replies, extract_sequenced_replies, reply_marker_ids, reply_marker_sets,
    sequenced_reply_id,
)
from agentctl.errors import AgentDeliveryError, HerdrRunError, HerdrUnavailable
from agentctl.jsonx import as_mapping, as_sequence, get_str
from agentctl.procstat import parse_process_stat
from agentctl.subagents import harness_arguments
from agentctl.submission import SubmissionReceipt


_MIN_POLL_INTERVAL = 0.1
_MAX_POLL_INTERVAL = 86400.0
_DEFAULT_POLL_INTERVAL = 3600.0
_SHORT_FAILURE_BACKOFF_LIMIT = 60.0
_MIN_OBSERVER_WRITE_INTERVAL = 60.0
_MAX_OBSERVER_WRITE_INTERVAL = 3600.0
_DEFAULT_OBSERVER_WRITE_INTERVAL = 60.0
_LAUNCH_EXIT_COALESCE_SECONDS = 0.2
_LAUNCH_GATE_READY_TIMEOUT = 5.0
_MAX_COMMAND_STDOUT = 8 * 1024 * 1024
_MAX_COMMAND_STDERR = 64 * 1024
_MAX_OUTPUT_SUBSCRIPTIONS = 128
_MAX_OUTPUT_PATTERN_BYTES = 32 * 1024
_MAX_REPLY_ITEMS = 1_000_000  # protocol-v3 ordinal ceiling plus one recovery item
_REPLY_BUCKET_SIZE = 1_000
_MAX_STATE_REPLY_ITEMS = 65_536
_MAX_STATE_REPLY_BYTES = 1 << 30
_MAX_REQUEST_REPLY_ITEMS = 4_096
_MAX_REQUEST_REPLY_BYTES = 64 << 20
_MAX_PENDING_REPLY_ITEMS = 1_024
_MAX_PENDING_REPLY_BYTES = 32 << 20
_MAX_REQUEST_RECORDS = 2_048
_MAX_REQUEST_SOURCE_BYTES = 64 << 20
_MAX_MESSAGE_SOURCE_BYTES = 64 << 10
_MAX_REQUEST_FILE_BYTES = 512 << 10
_MAX_REQUEST_ENCODED_BYTES = 128 << 20
_MAX_FEEDBACK_RECORDS = 1_024
_MAX_FEEDBACK_BYTES = 16 << 20
_MAX_FEEDBACK_ITEM_BYTES = 64 << 10
_MAX_FEEDBACK_FILE_BYTES = 512 << 10
_MAX_DEFERRED_RECORDS = 1_024
_MAX_DEFERRED_BYTES = 64 << 20
_MAX_QUEUE_ARTIFACT_BYTES = 512 << 10
_MAX_QUEUE_BYTES = 512 << 20
_MAX_ATOMIC_STAGE_BYTES = 512 << 10
_MAX_LEGACY_ATOMIC_TEMP_FILES = 128
_MAX_LEGACY_ATOMIC_TEMP_BYTES = 64 << 20
_MAX_CURSOR_BYTES = 8 << 10
_MAX_TOKEN_BYTES = 16 << 10
_MAX_REST_RESPONSE_BYTES = 8 << 20
_MAX_RESOURCE_BYTES = 2 << 10
_MAX_TIMESTAMP_BYTES = 128
_MAX_ALLOWED_SENDERS = 256
_MAX_SENDER_BYTES = 256
_MAX_COMMAND_ARGUMENTS = 128
_MAX_COMMAND_ARGUMENT_BYTES = 8 << 10
_MAX_COMMAND_BYTES = 128 << 10
_MAX_TARGET_FIELD_BYTES = 512
_MAX_PATH_BYTES = 4096
_MAX_LAUNCH_LOG_SEGMENT_BYTES = 1 << 20
_MAX_LEGACY_REPLY_BYTES = _MAX_STATE_REPLY_BYTES
_RFC3339_TIMESTAMP = re.compile(
    r"(?P<day>\d{4}-\d{2}-\d{2})[Tt](?P<clock>\d{2}:\d{2}:\d{2})"
    r"(?:\.(?P<fraction>\d{1,9}))?(?P<zone>[Zz]|[+-]\d{2}:\d{2})\Z"
)

_REQUEST_REQUIRED_FIELDS = frozenset({
    "key", "message", "phase", "queue_id", "request_id", "received_at",
})
_REQUEST_REPLY_SUMMARY_FIELDS = frozenset({
    "reply_item_count", "reply_sent_count", "reply_ordinal_offset",
    "reply_total_bytes", "reply_pending_bytes",
})
_REQUEST_RECORD_FIELDS = frozenset({
    *_REQUEST_REQUIRED_FIELDS,
    "ack", "queued_at", "delivery_confirmed_at", "delivery_confirmed_by",
    "reply_storage", *_REQUEST_REPLY_SUMMARY_FIELDS,
    "reply_nonce", "reply_protocol", "reply_next_ordinal",
    "reply_closed_at", "reply_close_reason", "reply_storage_error",
    "reply_submission_digest", "reply_submission_bytes",
    "reply_submission_rejected_at", "reply_submission_error",
    "reply_last_id", "reply_last_at", "reply_id", "replied_at", "reply_error",
    "reply_capture", "capture_error", "capture_failed_at",
})


@dataclass(frozen=True)
class _OutputSubscription:
    """Immutable owner-validated cache snapshot, never a public bypass flag."""

    owner: Bridge
    complete: tuple[str, ...]
    markers: tuple[str, ...]
    watching: bool


@dataclass(frozen=True)
class _LegacyReplyPlan:
    path: Path
    embedded_path: Path
    encoded_bytes: int
    count: int
    sent: int
    total_bytes: int
    pending_bytes: int
    offset: int
    next_ordinal: int | None
    last_reply_id: str | None
    last_replied_at: str | None
    digest: str


@dataclass(frozen=True)
class _AuxPopulationSnapshot:
    """One queue-stable, fully bounded view used to seed a continuous owner."""

    usage: dict[str, int]
    prompt_index: dict[str, tuple[str, str]]
    feedback_pending: dict[str, tuple[str, bool]]
    deferred_messages: dict[Path, dict[str, object]]
    queue_reservations: dict[str, int]
    queue_fixed_reservation: int


class _ServiceTerminated(Exception):
    """SIGTERM requested normal service shutdown after owned cleanup."""


class Transport(Protocol):
    """Bounded poll/send/react operations; send deduplicates request_id retries.

    React receives a stable request_id for adapter reconciliation. Adapters must
    ensure the requested reaction is present, never toggle an existing reaction.
    The public REST API has no reaction requestId or exactly-once guarantee.
    """

    def __call__(self, request: dict[str, object]) -> dict[str, object]: ...


class _LaunchClient(Protocol):
    def pane_info(self, pane_id: str) -> AgentPaneInfo: ...
    def workspace_label(self, workspace_id: str) -> str: ...


def _utc() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


def _timestamp(value: str) -> datetime:
    if len(value.encode("utf-8")) > _MAX_TIMESTAMP_BYTES:
        raise ValueError(f"chat timestamps must not exceed {_MAX_TIMESTAMP_BYTES} UTF-8 bytes")
    match = _RFC3339_TIMESTAMP.fullmatch(value)
    if match is None:
        raise ValueError(
            "chat timestamps must be RFC3339 with a timezone and at most nine fractional digits"
        )
    zone = match["zone"]
    if zone.lower() == "z":
        zone = "+00:00"
    fraction = match["fraction"]
    # datetime stores microseconds. Truncate the provider's optional nanoseconds
    # explicitly so Python 3.10 and newer runtimes accept and compare them alike.
    microsecond = int((fraction or "").ljust(6, "0")[:6])
    result = datetime.fromisoformat(f"{match['day']}T{match['clock']}{zone}")
    return result.replace(microsecond=microsecond)


def _private(directory: Path) -> None:
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    _validate_private_directory(str(directory), "chat state directory", tighten=True)


def _read(path: Path) -> dict[str, object]:
    return _read_chat_json(path)[0]


def _read_chat_json(path: Path, artifact: ArtifactClass | None = None) -> tuple[dict[str, object], int]:
    try:
        return read_json(path, artifact)
    except AgentDeliveryError as exc:
        detail = str(exc)
        if ("must not be hard-linked:" not in detail
                and "changed while it was read:" not in detail):
            raise
        state = _chat_write_state(path)
        parts = path.absolute().relative_to(state).parts
        if not (parts[0] in ("requests", "feedback", "deferred", "submissions", "reply-receipts", "replies")
                or (len(parts) == 1 and parts[0] in (
                    "bridge.json", "input.json", "output.json", "request-limit.json", "population-limit.json"))
                or parts == (".atomic", "audited-v1.json")
                or (len(parts) == 3 and parts[0] == "queue"
                    and parts[1] in ("inbox", "inflight", "processed", "failed"))):
            raise
        # A live writer may be between final-link creation and slot unlink, or
        # may finish that cleanup while this descriptor is being read. Wait for
        # its lock, recover only its fixed slot if it crashed, then repeat the
        # strict bounded read while no writer can alter the destination.
        # An unrelated hardlink still fails; the ordinary read path is unchanged.
        with atomic_write_recovery(_chat_atomic_policy(state)):
            return read_json(path, artifact)


def _read_bounded_json(
    path: Path, limit: int, label: str,
) -> tuple[dict[str, object], int]:
    """Read one private JSON object and its exact encoded size from one fd.

    The path is opened without following symlinks. Both metadata checks and the
    bounded read apply to that descriptor, so replacing the directory entry
    cannot substitute a different inode between a size check and JSON decode.
    Reading at most ``limit + 1`` bytes and checking the descriptor again at EOF
    also refuses an opened regular file that grows while it is being consumed.
    """
    try:
        return _read_chat_json(path, ArtifactClass(label, limit))
    except AgentDeliveryError as exc:
        detail = str(exc)
        if "exceeds max_artifact_bytes=" in detail:
            raise ValueError(
                f"{label} exceeds its {limit}-byte encoded file limit") from exc
        if "changed while it was read" in detail:
            raise ValueError(f"{label} changed while it was read") from exc
        raise


def _chat_atomic_policy(state: Path) -> AtomicWritePolicy:
    return AtomicWritePolicy(str(state.absolute() / ".atomic"), _MAX_ATOMIC_STAGE_BYTES)


def _chat_write_state(path: Path) -> Path:
    """Map the closed set of Chat artifact layouts to its staging owner."""
    path = path.absolute()
    parents = path.parents
    if path.name == "audited-v1.json" and parents[0].name == ".atomic":
        return parents[1]
    if path.name in ("bridge.json", "input.json", "output.json", "request-limit.json", "population-limit.json"):
        return parents[0]
    if (len(parents) > 7 and parents[1].name in ("pending", "history")
            and parents[5].name == "items" and parents[6].name == "replies"):
        return parents[7]
    if len(parents) > 2 and parents[1].name in ("reply-receipts", "queue"):
        return parents[2]
    if parents[0].name in ("requests", "feedback", "deferred", "replies", "submissions", "queue"):
        return parents[1]
    return parents[0]


def _write(path: Path, document: dict[str, object]) -> None:
    with atomic_write_policy(_chat_atomic_policy(_chat_write_state(path))):
        write_json(path, document)


def _create_chat_json(state: Path, path: Path, document: dict[str, object]) -> None:
    with atomic_write_policy(_chat_atomic_policy(state)):
        write_json(path, document, create_only=True)


def _atomic_audit_marker(state: Path) -> bool:
    marker = state / ".atomic" / "audited-v1.json"
    try:
        document, _ = _read_bounded_json(marker, 1024, "atomic staging audit marker")
    except AgentDeliveryError as exc:
        if isinstance(exc.__cause__, FileNotFoundError):
            return False
        raise
    if document != {"version": 1}:
        raise ValueError("invalid atomic staging audit marker")
    return True


def _audit_scattered_temporaries(directories: Sequence[tuple[Path, int]]) -> None:
    """Bounded read-only migration audit; never remove ambiguous old names."""
    examined = count = byte_count = 0
    examples: list[str] = []
    entry_limit = 8 * _MAX_STATE_REPLY_ITEMS + 8 * _MAX_REQUEST_RECORDS + 4096

    def inspect(directory: Path, depth: int) -> None:
        nonlocal examined, count, byte_count
        if not directory.exists():
            return
        _validate_private_directory(str(directory), "atomic migration directory", tighten=False)
        with os.scandir(directory) as entries:
            for entry in entries:
                examined += 1
                if examined > entry_limit:
                    raise ValueError(f"atomic migration audit exceeds {entry_limit} directory entries")
                metadata = entry.stat(follow_symlinks=False)
                if entry.name.startswith(".message."):
                    if (not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid()
                            or stat.S_IMODE(metadata.st_mode) != 0o600):
                        raise ValueError(f"unsafe legacy atomic temporary retained: {entry.path}")
                    if metadata.st_size > _MAX_ATOMIC_STAGE_BYTES:
                        raise ValueError(f"legacy atomic temporary exceeds {_MAX_ATOMIC_STAGE_BYTES} bytes: {entry.path}")
                    count += 1
                    byte_count += metadata.st_size
                    if len(examples) < 5:
                        examples.append(entry.path)
                    if (count > _MAX_LEGACY_ATOMIC_TEMP_FILES
                            or byte_count > _MAX_LEGACY_ATOMIC_TEMP_BYTES):
                        raise ValueError(
                            f"legacy atomic temporary budget exceeded: {count} files, {byte_count} bytes; "
                            + ", ".join(examples))
                elif depth and stat.S_ISDIR(metadata.st_mode):
                    inspect(Path(entry.path), depth - 1)
                elif depth and stat.S_ISLNK(metadata.st_mode):
                    raise ValueError(f"symlink in atomic migration directory: {entry.path}")

    for directory, depth in directories:
        inspect(directory, depth)
    if count:
        raise ValueError(
            f"legacy atomic temporaries require offline cleanup: {count} files, {byte_count} bytes; "
            + ", ".join(examples))


def _audit_chat_temporaries(state: Path, run_lock: int) -> None:
    """Audit under run -> submission -> binding -> delivery -> atomic locks.

    Queue drain/send release binding before taking delivery; enqueue starts at
    delivery, and submission never takes either queue lock. No reverse edge
    exists. Historical binding writes need the binding lock even though their
    temporary files live beside delivery's queue artifacts.
    """
    fcntl.flock(run_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    policy = _chat_atomic_policy(state)
    if _atomic_audit_marker(state):
        # The durable marker eliminates migration walks, not fixed-slot
        # recovery. A subsequent owner crash can leave that one bounded slot.
        with atomic_write_recovery(policy):
            pass
        return
    locks: list[int] = []
    directory = -1
    try:
        for path in (state / ".submissions.lock", state / "queue" / ".binding.lock",
                     state / "queue" / ".delivery.lock",
                     state / ".atomic.lock"):
            _private(path.parent)
            descriptor = _open_private_lock(str(path), "atomic migration lock")
            locks.append(descriptor)
            fcntl.flock(descriptor, fcntl.LOCK_EX)
        directory = _atomic_policy_directory(policy)
        _recover_atomic_slot(directory, policy)
        with os.scandir(directory) as entries:
            for entry in entries:
                if entry.name != "audited-v1.json":
                    raise ValueError(f"unexpected atomic staging entry retained: {entry.name}")
        _audit_scattered_temporaries([
            (state, 0), *((state / name, 0) for name in (
                "requests", "feedback", "deferred", "submissions")),
            (state / "replies", 6), (state / "reply-receipts", 1), (state / "queue", 1),
        ])
        # All historical writer locks remain held; release only the atomic
        # lock before using the normal policy writer for the durable marker.
        os.close(locks.pop())
        with atomic_write_policy(policy):
            _atomic_json(str(state / ".atomic" / "audited-v1.json"), {"version": 1})
    finally:
        if directory >= 0:
            os.close(directory)
        for descriptor in reversed(locks):
            os.close(descriptor)


def _reply_item_path(
    state: Path, key: str, index: int, *, sent: bool, create: bool = False,
) -> Path:
    """Return one bounded-fanout reply artifact path."""
    if re.fullmatch(r"[0-9a-f]{64}", key) is None:
        raise ValueError("invalid saved reply request key")
    if type(index) is not int or not 0 <= index < _MAX_REPLY_ITEMS:
        raise ValueError("reply item index is outside the supported range")
    root = (state / "replies" / "items" / key[:2] / key[2:4] / key
            / ("history" if sent else "pending"))
    bucket = root / f"{index // _REPLY_BUCKET_SIZE:04d}"
    if create:
        current = state / "replies"
        for component in (
            "items", key[:2], key[2:4], key,
            "history" if sent else "pending", bucket.name,
        ):
            parent = current
            current = current / component
            created = False
            try:
                current.mkdir(mode=0o700)
                created = True
            except FileExistsError:
                pass
            _validate_private_directory(
                str(current), "chat reply artifact directory", tighten=False)
            if created:
                _fsync_dir(str(parent))
    else:
        current = state / "replies"
        for component in (
            "items", key[:2], key[2:4], key,
            "history" if sent else "pending", bucket.name,
        ):
            current = current / component
            try:
                current.lstat()
            except FileNotFoundError:
                break
            _validate_private_directory(
                str(current), "chat reply artifact directory", tighten=False)
    return bucket / f"{index:06d}.json"


def _embedded_reply_path(state: Path, key: str) -> Path:
    return state / "replies" / f"{key}.json"


class _OutputObserver:
    """Persist the latest output state without turning flaps into write storms.

    The first state is durable immediately. Later changes inside the configured
    interval are coalesced and the latest value is flushed at the deadline. There
    is deliberately no periodic heartbeat: liveness belongs to the process
    supervisor, while this file records the latest known subscription state.
    """

    def __init__(
        self, path: Path, write_interval: float, *,
        monotonic: Callable[[], float] | None = None,
        utc: Callable[[], str] | None = None,
    ) -> None:
        if not _MIN_OBSERVER_WRITE_INTERVAL <= write_interval <= _MAX_OBSERVER_WRITE_INTERVAL:
            raise ValueError("observer write interval must be between 60 and 3600 seconds")
        self.path = path
        self.write_interval = write_interval
        self._monotonic = monotonic or time.monotonic
        self._utc = utc or _utc
        self._logical: tuple[str, str | None] | None = None
        self._persisted: tuple[str, str | None] | None = None
        self._pending: tuple[str, str | None] | None = None
        self._next_write: float | None = None
        if path.exists():
            saved = _read(path)
            state = saved.get("state")
            saved_error = saved.get("error")
            if saved_error is not None and not isinstance(saved_error, str):
                raise ValueError("saved output observer error must be null or a string")
            error = saved_error[:2000] if isinstance(saved_error, str) else None
            updated_at = saved.get("updated_at")
            self._validate(state, error)
            if not isinstance(updated_at, str):
                raise ValueError("saved output observer updated_at must be an RFC3339 timestamp")
            if error != saved_error:
                now = float(self._monotonic())
                _write(path, {"state": state, "error": error, "updated_at": str(self._utc())})
                self._logical = (str(state), error)
                self._persisted = self._logical
                self._next_write = now + write_interval
                return
            age = (_timestamp(str(self._utc())) - _timestamp(updated_at)).total_seconds()
            self._logical = (str(state), error)
            self._persisted = self._logical
            remaining = write_interval - max(0.0, age)
            self._next_write = float(self._monotonic()) + max(0.0, min(write_interval, remaining))

    @staticmethod
    def _validate(state: object, error: object) -> None:
        if state not in ("idle", "connected", "retrying"):
            raise ValueError("output observer state must be idle, connected, or retrying")
        if error is not None and (not isinstance(error, str) or len(error) > 2000):
            raise ValueError("output observer error must be null or at most 2000 characters")

    @property
    def next_write(self) -> float | None:
        """Monotonic deadline for one coalesced pending transition."""
        return self._next_write if self._pending is not None else None

    def _persist(self, logical: tuple[str, str | None], now: float) -> None:
        state, error = logical
        _write(self.path, {"state": state, "error": error, "updated_at": str(self._utc())})
        self._persisted = logical
        self._pending = None
        self._next_write = now + self.write_interval

    def observe(self, state: str, error: str | None) -> bool:
        """Record a transition, coalescing changes inside the write-rate bound."""
        self._validate(state, error)
        logical = (state, error)
        now = float(self._monotonic())
        if logical == self._logical:
            return False
        self._logical = logical
        if logical == self._persisted:
            self._pending = None
            return False
        if self._persisted is None or self._next_write is None or now >= self._next_write:
            self._persist(logical, now)
            return True
        self._pending = logical
        return False

    def flush(self) -> bool:
        """Flush the latest coalesced transition once its deadline arrives."""
        if self._pending is None or self._next_write is None:
            return False
        now = float(self._monotonic())
        if now < self._next_write:
            return False
        self._persist(self._pending, now)
        return True


def _strings(
    value: object, what: str, *, max_items: int, max_item_bytes: int,
    max_total_bytes: int,
) -> tuple[str, ...]:
    values = as_sequence(value, what)
    if not values or any(not isinstance(item, str) or not item for item in values):
        raise ValueError(f"{what} must be a nonempty list of nonempty strings")
    if len(values) > max_items:
        raise ValueError(f"{what} must contain at most {max_items} values")
    result = tuple(str(item) for item in values)
    sizes = [len(item.encode("utf-8")) for item in result]
    if any(size > max_item_bytes for size in sizes):
        raise ValueError(f"{what} values must not exceed {max_item_bytes} UTF-8 bytes")
    if sum(sizes) > max_total_bytes:
        raise ValueError(f"{what} must not exceed {max_total_bytes} aggregate UTF-8 bytes")
    return result


def _bounded_string(value: str, what: str, limit: int) -> str:
    if len(value.encode("utf-8")) > limit:
        raise ValueError(f"{what} must not exceed {limit} UTF-8 bytes")
    return value


def _cursor(value: object, what: str = "transport cursor") -> str | None:
    if value is None:
        return None
    if not isinstance(value, str) or not value:
        raise ValueError(f"{what} must be a nonempty string or null")
    if len(value.encode("utf-8")) > _MAX_CURSOR_BYTES:
        raise ValueError(f"{what} must not exceed {_MAX_CURSOR_BYTES} UTF-8 bytes")
    return value


def _reaction_emoji(value: object) -> str | None:
    if value is None or value == "":
        return None
    if (not isinstance(value, str) or not value.strip() or len(value.encode()) > 128
            or any(ord(character) < 32 or ord(character) == 127 for character in value)):
        raise ValueError("ack_reaction must be a Unicode emoji of at most 128 UTF-8 bytes, or null/empty to disable")
    return value


def _reply_instruction(nonce: str, ordinal: int = 1) -> str:
    identifier = sequenced_reply_id(nonce, ordinal)
    return ("You may send one or multiple replies to this request, including progress updates. "
            f"Your first reply ID is `{identifier}`. Compose an opening line from the literal prefix "
            "`<CHAT_REPLY_`, that ID, and `>`; compose its closing line from `</CHAT_REPLY_`, the "
            "same ID, and `>`. Keep both lines standalone and outside code fences. Increment the "
            "numeric suffix for every later reply. Each consecutive complete block is sent as a "
            "separate chat message.")


def _closing_patterns(marker_ids: Sequence[str]) -> tuple[str, ...]:
    """Build bounded, line-local Herdr predicates for protocol-v3 replies."""
    if len(marker_ids) > _MAX_OUTPUT_SUBSCRIPTIONS:
        raise ValueError(
            f"chat output has {len(marker_ids)} active requests; close replies to stay at or below "
            f"the {_MAX_OUTPUT_SUBSCRIPTIONS}-request subscription limit")
    if not marker_ids:
        return (r"^[^\S\r\n]*(?:[•⏺●][ \t]+)?</?(?:GCHAT|CHAT)_REPLY_[^<>\s]*>[^\S\r\n]*$",)
    patterns: list[str] = []
    for identifier in marker_ids:
        if (re.fullmatch(r"[A-Za-z0-9_-]{22}", identifier) is None
                and re.fullmatch(r"[A-Za-z0-9_-]{22}_[1-9][0-9]{0,5}", identifier) is None):
            raise ValueError("invalid saved chat output marker")
        patterns.append(
            r"^[^\S\r\n]*(?:[•⏺●][ \t]+)?</(?:GCHAT|CHAT)_REPLY_"
            + re.escape(identifier) + r">[^\S\r\n]*$")
    if sum(len(pattern.encode("utf-8")) for pattern in patterns) > _MAX_OUTPUT_PATTERN_BYTES:
        raise ValueError("chat output subscription patterns exceed the 32 KiB safety bound")
    for pattern in patterns:
        re.compile(pattern)
    return tuple(patterns)


def _request_path(state: Path, key: str) -> Path:
    if re.fullmatch(r"[0-9a-f]{12,64}", key) is None:
        raise ValueError("request must be a hexadecimal key or unique prefix of at least 12 characters")
    matches = list((state / "requests").glob(key + "*.json"))
    if len(matches) != 1:
        raise ValueError(f"request prefix must identify exactly one saved request; found {len(matches)}")
    return matches[0]


@dataclass(frozen=True)
class Config:
    """The authority boundary is one space, named senders, and one pinned target."""

    space: str
    allowed_senders: tuple[str, ...]
    target: Target
    agent_label: str
    transport_command: tuple[str, ...] = ()
    token_env: str = "HERDR_CHAT_TOKEN"
    token_command: tuple[str, ...] = ()
    agent_name: str | None = None
    ack_reaction: str | None = "🤖"
    reaction_user: str | None = None
    reply_mode: str = "tagged"
    event_command: tuple[str, ...] = ()
    transport_socket: str | None = None
    outbound_mode: str = "enabled"

    @cached_property
    def allowed_sender_set(self) -> frozenset[str]:
        """Constant-time authorization index excluded from durable dataclass state."""
        return frozenset(self.allowed_senders)

    @classmethod
    def parse(cls, document: dict[str, object]) -> Config:
        """Validate explicit sender, target, and transport configuration."""
        encoded_json(document, CONFIG)
        if set(document) - set(cls.__dataclass_fields__):
            raise ValueError("unknown chat configuration field")
        space = get_str(document, "space", "chat config")
        if (len(space.encode("utf-8")) > _MAX_RESOURCE_BYTES
                or re.fullmatch(r"spaces/[A-Za-z0-9_-]+", space) is None):
            raise ValueError("space must be an exact spaces/ID resource")
        label = get_str(document, "agent_label", "chat config")
        if (not label.strip() or "\n" in label or len(label) > 100
                or len(label.encode("utf-8")) > 400):
            raise ValueError("agent_label must identify the replying agent on one line")
        values = as_mapping(document.get("target"), "chat target")
        fields: dict[str, str | None] = {}
        for key in Target.__dataclass_fields__:
            value = values.get(key)
            if value is not None and (not isinstance(value, str) or not value):
                raise ValueError(f"target.{key} must be a nonempty string")
            if isinstance(value, str):
                limit = _MAX_PATH_BYTES if key == "expected_cwd" else _MAX_TARGET_FIELD_BYTES
                _bounded_string(value, f"target.{key}", limit)
            fields[key] = value if isinstance(value, str) else None
        if set(values) - set(fields):
            raise ValueError("unknown target identity assertion")
        target = Target(**fields)
        if not target.pane_id and not target.session_value:
            raise ValueError("chat target needs pane_id or session_value")
        if not target.expected_agent or not target.expected_cwd or not target.expected_workspace:
            raise ValueError("chat target requires expected_agent, expected_cwd, expected_workspace")
        command = document.get("transport_command")
        token_command = document.get("token_command")
        event_command = document.get("event_command")
        transport_socket = document.get("transport_socket")
        if transport_socket is not None and (
            not isinstance(transport_socket, str) or not os.path.isabs(transport_socket)
            or "\0" in transport_socket
            or len(transport_socket.encode("utf-8")) > _MAX_PATH_BYTES
        ):
            raise ValueError("transport_socket must be an absolute Unix socket path")
        if transport_socket is not None and command:
            raise ValueError("transport_socket and transport_command are mutually exclusive")
        token_env = document.get("token_env", "HERDR_CHAT_TOKEN")
        if (not isinstance(token_env, str)
                or re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", token_env) is None
                or len(token_env.encode("utf-8")) > 256):
            raise ValueError("token_env must name an environment variable")
        agent_name = document.get("agent_name")
        if agent_name is not None and (
            not isinstance(agent_name, str) or not agent_name.strip() or "\0" in agent_name
            or len(agent_name.encode("utf-8")) > _MAX_TARGET_FIELD_BYTES
        ):
            raise ValueError("agent_name must name a live Herdr agent")
        reaction_user = document.get("reaction_user")
        if reaction_user is not None and (
            not isinstance(reaction_user, str)
            or re.fullmatch(r"users/[A-Za-z0-9_-]+", reaction_user) is None
            or reaction_user in ("users/me", "users/app")
            or len(reaction_user.encode("utf-8")) > _MAX_SENDER_BYTES
        ):
            raise ValueError("reaction_user must be the canonical users/ID of the OAuth user")
        reply_mode = document.get("reply_mode", "tagged")
        if reply_mode not in ("tagged", "file"):
            raise ValueError("reply_mode must be tagged or file")
        outbound_mode = document.get("outbound_mode", "enabled")
        if outbound_mode not in ("enabled", "disabled"):
            raise ValueError("outbound_mode must be enabled or disabled")
        senders = _strings(
            document.get("allowed_senders"), "allowed_senders",
            max_items=_MAX_ALLOWED_SENDERS, max_item_bytes=_MAX_SENDER_BYTES,
            max_total_bytes=_MAX_ALLOWED_SENDERS * _MAX_SENDER_BYTES,
        )
        if len(set(senders)) != len(senders) or any(
            re.fullmatch(r"users/[A-Za-z0-9_-]+", sender) is None for sender in senders
        ):
            raise ValueError("allowed_senders must contain unique canonical users/ID resources")
        def command_arguments(value: object, what: str) -> tuple[str, ...]:
            return _strings(
                value, what, max_items=_MAX_COMMAND_ARGUMENTS,
                max_item_bytes=_MAX_COMMAND_ARGUMENT_BYTES,
                max_total_bytes=_MAX_COMMAND_BYTES,
            )
        return cls(space, senders, target,
                   label, () if command is None or command == [] else command_arguments(command, "transport_command"), token_env,
                   () if token_command is None or token_command == [] else command_arguments(token_command, "token_command"),
                   agent_name, _reaction_emoji(document.get("ack_reaction", "🤖")), reaction_user, str(reply_mode),
                   () if event_command is None or event_command == [] else command_arguments(event_command, "event_command"),
                   transport_socket, str(outbound_mode))


class _NamedClient(HerdrClient):
    """Revalidate a named coordinator at every readiness probe and submission."""

    def __init__(self, client: HerdrClient, name: str) -> None:
        self._delegate, self._name = client, name

    def pane_info(self, pane_id: str) -> AgentPaneInfo:
        if self._delegate.agent_pane(self._name) != pane_id:
            raise HerdrUnavailable(f"named coordinator {self._name!r} no longer owns pane {pane_id!r}")
        return self._delegate.pane_info(pane_id)

    def panes(self, workspace_id: str | None = None) -> tuple[Pane, ...]:
        pane_id = self._delegate.agent_pane(self._name)
        return tuple(pane for pane in self._delegate.panes(workspace_id) if pane.pane_id == pane_id)

    def workspace_label(self, workspace_id: str) -> str:
        return self._delegate.workspace_label(workspace_id)

    def prompt_agent(
        self, pane_id: str, command: str, *, terminal: GuardedInput | None = None,
    ) -> SubmissionReceipt | None:
        self.pane_info(pane_id)
        return self._delegate.prompt_agent(pane_id, command, terminal=terminal)

    def wait_agent_status(self, pane_id: str, status: str, timeout_ms: int) -> None:
        self.pane_info(pane_id)
        self._delegate.wait_agent_status(pane_id, status, timeout_ms)
        self.pane_info(pane_id)

    def read(self, pane_id: str, *, source: str = "recent-unwrapped", lines: int | None = None) -> str:
        self.pane_info(pane_id)
        return self._delegate.read(pane_id, source=source, lines=lines)

    def event_socket(self) -> str:
        return self._delegate.event_socket()


class CommandTransport:
    """Operator-selected adapter; arguments are literal, never evaluated by a shell."""

    def __init__(self, command: Sequence[str]) -> None:
        self.command = tuple(command)

    def __call__(self, request: dict[str, object]) -> dict[str, object]:
        result = _run_command(self.command, input_text=json.dumps(request), timeout=60)
        if result.returncode:
            raise ValueError(f"chat adapter exited {result.returncode}: {result.stderr[-2000:]}")
        return decode_json(
            result.stdout.encode("utf-8"),
            ArtifactClass("chat adapter response", _MAX_COMMAND_STDOUT),
        )


@dataclass(frozen=True)
class _CommandAnchorIdentity:
    pid: int
    starttime: int
    ppid: int
    pgrp: int
    session: int
    state: str


def _command_process_identity(pid: int) -> _CommandAnchorIdentity | None:
    """Read one Linux process identity without confusing ')' in comm for syntax."""
    try:
        raw = Path(f"/proc/{pid}/stat").read_bytes()
    except FileNotFoundError:
        return None
    parsed = parse_process_stat(raw)
    if parsed is None or parsed.pid != pid:
        raise ValueError("malformed command process identity record")
    return _CommandAnchorIdentity(
        pid=pid,
        state=parsed.state,
        ppid=parsed.ppid,
        pgrp=parsed.pgrp,
        session=parsed.session,
        starttime=parsed.starttime,
    )


def _same_command_process(
    expected: _CommandAnchorIdentity, observed: _CommandAnchorIdentity | None,
) -> bool:
    return observed is not None and observed.pid == expected.pid and (
        observed.starttime == expected.starttime)


def _validated_supervisor_identity(
    process: subprocess.Popen[bytes], context: str,
) -> _CommandAnchorIdentity:
    """Capture one live, unreaped direct session leader immediately after Popen."""
    observed = _command_process_identity(process.pid)
    if (observed is None or observed.ppid != os.getpid()
            or observed.pgrp != process.pid or observed.session != process.pid
            or observed.state in ("X", "Z")):
        raise ValueError(f"{context} did not publish a stable direct-child identity")
    return observed


def _validated_numeric_direct_child_signal(
    process: subprocess.Popen[bytes], expected: _CommandAnchorIdentity,
    signum: int, context: str,
) -> None:
    """Signal a numeric PID only while its unreaped direct-child identity pins it."""
    observed = _command_process_identity(process.pid)
    if (not _same_command_process(expected, observed) or observed is None
            or expected.ppid != os.getpid() or observed.ppid != expected.ppid
            or observed.pgrp != expected.pgrp or observed.session != expected.session
            or observed.state == "X"):
        raise RuntimeError(f"{context} identity could not pin numeric cleanup")
    try:
        os.kill(process.pid, signum)
    except ProcessLookupError as exc:
        if _same_command_process(expected, _command_process_identity(process.pid)):
            raise RuntimeError(f"{context} rejected validated numeric cleanup") from exc


def _cleanup_unpinned_direct_child(
    process: subprocess.Popen[bytes], expected: _CommandAnchorIdentity,
    context: str, *, timeout: float = 5.0,
) -> None:
    """Wake, kill, reap, and prove disappearance without a pidfd."""
    _validated_numeric_direct_child_signal(process, expected, signal.SIGCONT, context)
    _validated_numeric_direct_child_signal(process, expected, signal.SIGKILL, context)
    try:
        process.wait(timeout=timeout)
    except subprocess.TimeoutExpired as exc:
        raise RuntimeError(f"{context} survived validated numeric cleanup") from exc
    if _same_command_process(expected, _command_process_identity(process.pid)):
        raise RuntimeError(f"{context} remained after reap")


def _command_group_members(anchor: _CommandAnchorIdentity) -> tuple[_CommandAnchorIdentity, ...]:
    """Enumerate one frozen emergency group; normal command completion never scans /proc."""
    members: list[_CommandAnchorIdentity] = []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            observed = _command_process_identity(int(entry.name))
        except (OSError, ValueError):
            continue
        if (observed is not None and observed.pgrp == anchor.pgrp
                and observed.session == anchor.session):
            members.append(observed)
    return tuple(sorted(members, key=lambda item: item.pid))


def _command_identities_gone(
    identities: Sequence[_CommandAnchorIdentity], wait: float,
) -> bool:
    end = time.monotonic() + wait
    while True:
        if all(not _same_command_process(item, _command_process_identity(item.pid))
               for item in identities):
            return True
        remaining = end - time.monotonic()
        if remaining <= 0:
            return False
        time.sleep(min(0.01, remaining))


def _read_command_publication(
    descriptor: int, supervisor_pid: int, deadline: float,
) -> _CommandAnchorIdentity:
    os.set_blocking(descriptor, False)
    selector = selectors.DefaultSelector()
    data = bytearray()
    try:
        selector.register(descriptor, selectors.EVENT_READ)
        while b"\n" not in data:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise subprocess.TimeoutExpired("chat command supervisor handshake", 0)
            if not selector.select(remaining):
                raise subprocess.TimeoutExpired("chat command supervisor handshake", 0)
            try:
                chunk = os.read(descriptor, 4096)
            except BlockingIOError:
                continue
            if not chunk:
                raise ValueError("chat command supervisor closed before publishing containment")
            data.extend(chunk)
            if len(data) > 4096:
                raise ValueError("chat command supervisor publication exceeds 4096 bytes")
        line, remainder = bytes(data).split(b"\n", 1)
        if remainder:
            raise ValueError("chat command supervisor sent trailing control data")
        document = as_mapping(json.loads(line, object_pairs_hook=_command_object_pairs),
                              "chat command supervisor publication")
        if set(document) != {
            "version", "supervisor_pid", "pid", "starttime", "ppid", "pgrp", "session", "state",
        }:
            raise ValueError("chat command supervisor publication has invalid fields")
        fields: dict[str, int] = {}
        for key in ("version", "supervisor_pid", "pid", "starttime", "ppid", "pgrp", "session"):
            value = document.get(key)
            if not isinstance(value, int) or isinstance(value, bool):
                raise ValueError(f"chat command supervisor publication {key} must be an integer")
            fields[key] = value
        state = get_str(document, "state", "chat command supervisor publication")
        if (fields["version"] != 1 or fields["supervisor_pid"] != supervisor_pid
                or fields["pid"] <= 0 or fields["starttime"] <= 0
                or fields["ppid"] != supervisor_pid or fields["pgrp"] != fields["pid"]
                or fields["session"] != supervisor_pid or state in ("X", "Z")):
            raise ValueError("chat command supervisor published an invalid containment identity")
        expected = _CommandAnchorIdentity(
            pid=fields["pid"], starttime=fields["starttime"], ppid=fields["ppid"],
            pgrp=fields["pgrp"], session=fields["session"], state=state,
        )
        observed = _command_process_identity(expected.pid)
        if (not _same_command_process(expected, observed) or observed is None
                or observed.ppid != expected.ppid or observed.pgrp != expected.pgrp
                or observed.session != expected.session or observed.state in ("X", "Z")):
            raise ValueError("chat command anchor identity changed before acknowledgement")
        return expected
    finally:
        selector.close()


def _write_command_control(descriptor: int, value: bytes, deadline: float) -> None:
    selector = selectors.DefaultSelector()
    try:
        selector.register(descriptor, selectors.EVENT_WRITE)
        while value:
            try:
                written = os.write(descriptor, value)
            except BlockingIOError:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not selector.select(remaining):
                    raise subprocess.TimeoutExpired("chat command supervisor handshake", 0)
                continue
            value = value[written:]
    finally:
        selector.close()


def _command_object_pairs(pairs: list[tuple[str, object]]) -> dict[str, object]:
    document: dict[str, object] = {}
    for key, value in pairs:
        if key in document:
            raise ValueError("chat command supervisor repeated a control field")
        document[key] = value
    return document


def _read_command_result(descriptor: int) -> int:
    """Read the trusted supervisor's exact, bounded final status after its exit."""
    os.set_blocking(descriptor, False)
    data = bytearray()
    while True:
        try:
            chunk = os.read(descriptor, 257 - len(data))
        except BlockingIOError as exc:
            raise ValueError("chat command supervisor left its final result open") from exc
        if not chunk:
            break
        data.extend(chunk)
        if len(data) > 256:
            raise ValueError("chat command supervisor final result exceeds 256 bytes")
    if not data.endswith(b"\n") or data.count(b"\n") != 1:
        raise ValueError("chat command supervisor final result is missing or malformed")
    document = as_mapping(json.loads(data, object_pairs_hook=_command_object_pairs),
                          "chat command supervisor final result")
    if set(document) != {"version", "returncode"} or type(document["version"]) is not int:
        raise ValueError("chat command supervisor final result has invalid fields")
    value = document["returncode"]
    if (document["version"] != 1 or not isinstance(value, int) or isinstance(value, bool)
            or not -64 <= value <= 255):
        raise ValueError("chat command supervisor final result has an invalid returncode")
    return value


def _run_command(
    command: Sequence[str], *, timeout: float, input_text: str | None = None,
    stdout_limit: int | None = None,
) -> subprocess.CompletedProcess[str]:
    """Bound adapter duration, its original process group, stdout, and stderr."""
    if not command:
        raise ValueError("chat helper command must not be empty")
    selected_stdout_limit = _MAX_COMMAND_STDOUT if stdout_limit is None else stdout_limit
    if (type(selected_stdout_limit) is not int
            or not 0 < selected_stdout_limit <= _MAX_COMMAND_STDOUT):
        raise ValueError(
            f"chat helper stdout limit must be an integer from 1 to {_MAX_COMMAND_STDOUT}")
    if not hasattr(os, "pidfd_open") or not hasattr(signal, "pidfd_send_signal"):
        raise RuntimeError("chat command containment requires Linux process descriptors")
    _require_waitable_sigchld_children("chat command containment")
    # Prove descriptor support before any child is created. Resource exhaustion
    # or a kernel denying pidfd_open must never grant adapter authority.
    try:
        probe_pidfd = os.pidfd_open(os.getpid())
    except OSError as exc:
        raise RuntimeError(f"chat command process descriptor unavailable: {exc}") from exc
    os.close(probe_pidfd)
    payload = b"" if input_text is None else input_text.encode("utf-8")
    selector = selectors.DefaultSelector()
    supervisor = Path(__file__).with_name("_command_supervisor.py")
    controls: list[int] = []

    def control_pipe() -> tuple[int, int]:
        pair = os.pipe2(os.O_CLOEXEC)
        controls.extend(pair)
        return pair

    deadline = time.monotonic() + timeout
    try:
        identity_read, identity_write = control_pipe()
        acknowledge_read, acknowledge_write = control_pipe()
        lifeline_read, lifeline_write = control_pipe()
        result_read, result_write = control_pipe()
        process = subprocess.Popen(
            (sys.executable, str(supervisor), str(os.getpid()),
             str(identity_write), str(acknowledge_read), str(lifeline_read), str(result_write),
             *command),
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            start_new_session=True, bufsize=0,
            pass_fds=(identity_write, acknowledge_read, lifeline_read, result_write),
        )
    except BaseException:
        selector.close()
        for descriptor in controls:
            os.close(descriptor)
        raise
    os.close(identity_write)
    os.close(acknowledge_read)
    os.close(lifeline_read)
    os.close(result_write)
    pidfd: int | None = None
    anchor_pidfd: int | None = None
    anchor: _CommandAnchorIdentity | None = None
    lifeline_open = True
    supervisor_identity: _CommandAnchorIdentity | None = None
    authority_granted = False
    result_confirmed = False

    def signal_supervisor(signum: int) -> None:
        if pidfd is not None:
            try:
                signal.pidfd_send_signal(pidfd, signum)
            except ProcessLookupError:
                pass

    def close_lifeline() -> None:
        nonlocal lifeline_open
        if lifeline_open:
            os.close(lifeline_write)
            lifeline_open = False

    def wait_anchor_gone(wait: float) -> bool:
        if anchor is None:
            return True
        end = time.monotonic() + wait
        while True:
            if not _same_command_process(anchor, _command_process_identity(anchor.pid)):
                return True
            remaining = end - time.monotonic()
            if remaining <= 0:
                return False
            time.sleep(min(0.01, remaining))

    def stop_direct_supervisor() -> str | None:
        """Freeze the exact supervisor while its waitable identity remains pinned."""
        signal_supervisor(signal.SIGSTOP)
        end = time.monotonic() + 0.5
        while True:
            observed = _command_process_identity(process.pid)
            if (supervisor_identity is None
                    or not _same_command_process(supervisor_identity, observed)):
                return None
            if observed is None or observed.state in ("T", "t", "Z"):
                return None if observed is None else observed.state
            if time.monotonic() >= end:
                return observed.state
            time.sleep(0.005)

    def contain_anchor(
        supervisor_state: str | None,
    ) -> tuple[_CommandAnchorIdentity, ...] | None:
        """Freeze, census, and kill only the revalidated anchored process group."""
        if anchor is None:
            return None
        if anchor_pidfd is not None:
            try:
                signal.pidfd_send_signal(anchor_pidfd, signal.SIGSTOP)
            except ProcessLookupError:
                return None
        else:
            return None
        if supervisor_state not in (None, "T", "t", "Z"):
            return None
        end = time.monotonic() + 0.5
        def pinned() -> _CommandAnchorIdentity | None:
            assert anchor is not None
            observed = _command_process_identity(anchor.pid)
            supervisor_now = _command_process_identity(process.pid)
            stopped_supervisor = (
                supervisor_identity is not None
                and _same_command_process(supervisor_identity, supervisor_now)
                and supervisor_now is not None and supervisor_now.state in ("T", "t"))
            dead_supervisor = (
                supervisor_identity is not None
                and (not _same_command_process(supervisor_identity, supervisor_now)
                     or (supervisor_now is not None and supervisor_now.state == "Z")))
            if (not _same_command_process(anchor, observed) or observed is None
                    or observed.pgrp != anchor.pid or observed.session != process.pid):
                return None
            # A zombie reserves the PGID only while its exact parent cannot
            # reap it. A live stopped anchor also pins it after S has died.
            if (stopped_supervisor and observed.ppid == process.pid
                    and observed.state in ("T", "t", "Z")):
                return observed
            if dead_supervisor and observed.state in ("T", "t"):
                return observed
            return None

        observed = _command_process_identity(anchor.pid)
        while (_same_command_process(anchor, observed)
               and observed is not None and observed.state not in ("T", "t", "Z")):
            if time.monotonic() >= end:
                return None
            time.sleep(0.005)
            observed = _command_process_identity(anchor.pid)
        if pinned() is None:
            return None
        previous: tuple[tuple[int, int], ...] | None = None
        members: tuple[_CommandAnchorIdentity, ...] = ()
        census_count = 0
        # Stability requires two observations.  A large host can make one
        # complete /proc census outlast the nominal retry window, so the
        # deadline bounds additional retries rather than silently weakening
        # the proof to one snapshot.
        while census_count < 2 or time.monotonic() < end:
            if pinned() is None:
                return None
            try:
                os.killpg(anchor.pid, signal.SIGSTOP)
            except ProcessLookupError:
                return None
            members = _command_group_members(anchor)
            census_count += 1
            signature = tuple((member.pid, member.starttime) for member in members)
            if (signature == previous and members
                    and all(member.state in ("T", "t", "Z") for member in members)):
                break
            previous = signature
            time.sleep(0.005)
        else:
            return None
        # The stable stopped census and exact anchor pin the numeric PGID
        # against reuse while this kills every member of the original group.
        if pinned() is None:
            return None
        os.killpg(anchor.pid, signal.SIGKILL)
        return members

    def stop_supervisor() -> None:
        nonlocal acknowledge_write, result_confirmed
        if pidfd is None:
            # No adapter authority exists before ACK. Close every authority and
            # lifeline channel first, then use the captured unreaped direct-child
            # identity to wake and remove even a stopped trusted supervisor.
            if acknowledge_write >= 0:
                os.close(acknowledge_write)
                acknowledge_write = -1
            close_lifeline()
            if supervisor_identity is None:
                try:
                    process.wait(timeout=2)
                except subprocess.TimeoutExpired as exc:
                    raise RuntimeError(
                        "chat command supervisor lacked a validated cleanup identity",
                    ) from exc
                if _command_process_identity(process.pid) is not None:
                    raise RuntimeError("chat command supervisor remained after reap")
            else:
                _cleanup_unpinned_direct_child(
                    process, supervisor_identity, "chat command supervisor",
                )
            return
        # Keep the lifeline open until containment is proved. Closing it first
        # can destroy the anchor before a stable emergency census is possible.
        recorded: tuple[_CommandAnchorIdentity, ...] = ()
        proved = not authority_granted
        cleanup_fault: BaseException | None = None
        try:
            if acknowledge_write >= 0:
                os.close(acknowledge_write)
                acknowledge_write = -1
            signal_supervisor(signal.SIGTERM)
            signal_supervisor(signal.SIGCONT)
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                pass
            else:
                if not result_confirmed:
                    try:
                        _read_command_result(result_read)
                    except (ValueError, TypeError, OSError):
                        pass
                    else:
                        result_confirmed = True
                proved = proved or result_confirmed
            if not proved or not wait_anchor_gone(0):
                supervisor_state = stop_direct_supervisor()
                contained = contain_anchor(supervisor_state)
                if contained is not None:
                    recorded = contained
                    proved = True
                # Give the trusted subreaper its bounded chance to reap the
                # killed group before forcing the supervisor itself to exit.
                signal_supervisor(signal.SIGCONT)
                signal_supervisor(signal.SIGTERM)
                try:
                    process.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    pass
        except BaseException as exc:
            cleanup_fault = exc
        finally:
            # Even a /proc read failure or census error must not leave S or A
            # stopped. First preserve S's opportunity to reap adopted children.
            # A's parent-death SIGCONT + lifeline EOF remains the last bound.
            signal_supervisor(signal.SIGCONT)
            signal_supervisor(signal.SIGTERM)
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                pass
            finally:
                signal_supervisor(signal.SIGKILL)
                close_lifeline()
                try:
                    process.wait(timeout=2)
                except BaseException as exc:
                    cleanup_fault = exc
                finally:
                    if anchor_pidfd is not None:
                        try:
                            signal.pidfd_send_signal(anchor_pidfd, signal.SIGCONT)
                        except ProcessLookupError:
                            pass
        if cleanup_fault is not None:
            # A transient wait failure must not strand a reapable direct
            # child. Retry only after the anchor wake has been guaranteed.
            try:
                process.wait(timeout=2)
            except BaseException as exc:
                cleanup_fault = exc
        if not result_confirmed:
            try:
                _read_command_result(result_read)
            except (ValueError, TypeError, OSError):
                pass
            else:
                result_confirmed = True
                proved = True
        identities = (*recorded, *((supervisor_identity,) if supervisor_identity else ()))
        anchor_gone = wait_anchor_gone(2)
        identities_gone = _command_identities_gone(identities, 1)
        if cleanup_fault is not None and not authority_granted:
            raise RuntimeError("chat command supervisor cleanup failed") from cleanup_fault
        if not proved or not anchor_gone or not identities_gone:
            raise RuntimeError(
                "chat command containment could not prove every process disappeared") from cleanup_fault

    try:
        assert process.stdin is not None and process.stdout is not None and process.stderr is not None
        supervisor_identity = _validated_supervisor_identity(
            process, "chat command supervisor",
        )
        # The supervisor is awaiting our ACK and cannot grant authority yet.
        # The pre-spawn sigaction proof keeps this direct child waitable. Retain
        # a pidfd anyway so normal completion never depends on a numeric PID.
        try:
            pidfd = os.pidfd_open(process.pid)
        except OSError as original_pidfd_error:
            # A competing thread can consume the descriptor headroom proved
            # before Popen. Release a descriptor with no remaining purpose on
            # this refusal path, then obtain a safe handle solely for cleanup.
            os.close(identity_read)
            identity_read = -1
            try:
                pidfd = os.pidfd_open(process.pid)
            except OSError:
                pidfd = None
            raise original_pidfd_error
        anchor = _read_command_publication(identity_read, process.pid, deadline)
        anchor_pidfd = os.pidfd_open(anchor.pid)
        observed_anchor = _command_process_identity(anchor.pid)
        if (not _same_command_process(anchor, observed_anchor) or observed_anchor is None
                or observed_anchor.ppid != process.pid or observed_anchor.pgrp != anchor.pid
                or observed_anchor.session != process.pid
                or observed_anchor.state in ("X", "Z")):
            raise ValueError("chat command anchor identity changed before control acknowledgement")
        _write_command_control(acknowledge_write, b"1", deadline)
        authority_granted = True
        os.close(acknowledge_write)
        acknowledge_write = -1
        os.close(identity_read)
        identity_read = -1
        stdin, stdout_pipe, stderr_pipe = process.stdin, process.stdout, process.stderr
        pending = memoryview(payload)
        stdout_fd, stderr_fd = stdout_pipe.fileno(), stderr_pipe.fileno()
        stdout_buffer, stderr_buffer = bytearray(), bytearray()
        streams = {stdout_fd: (stdout_pipe, selected_stdout_limit, stdout_buffer),
                   stderr_fd: (stderr_pipe, _MAX_COMMAND_STDERR, stderr_buffer)}
        for pipe, _, _ in streams.values():
            os.set_blocking(pipe.fileno(), False)
            selector.register(pipe, selectors.EVENT_READ, "output")
        selector.register(pidfd, selectors.EVENT_READ, "leader")
        os.set_blocking(stdin.fileno(), False)
        if pending:
            selector.register(stdin, selectors.EVENT_WRITE, "input")
        else:
            stdin.close()
        leader_exited = False

        def close_input() -> None:
            if not stdin.closed:
                try:
                    selector.unregister(stdin)
                except KeyError:
                    pass
                stdin.close()

        def read_output(fd: int) -> None:
            pipe, limit, buffer = streams[fd]
            try:
                chunk = os.read(fd, 65536)
            except BlockingIOError:
                return
            if not chunk:
                selector.unregister(pipe)
                return
            buffer.extend(chunk)
            if len(buffer) > limit:
                stream = "stdout" if fd == stdout_fd else "stderr"
                raise ValueError(f"chat helper {stream} exceeds its byte limit")

        while not leader_exited:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise subprocess.TimeoutExpired(command, timeout)
            for key, _ in selector.select(remaining):
                if key.data == "leader":
                    leader_exited = True
                    continue
                if key.data == "input":
                    try:
                        sent = os.write(stdin.fileno(), pending)
                    except BlockingIOError:
                        continue
                    except BrokenPipeError:
                        sent = len(pending)
                    pending = pending[sent:]
                    if not pending:
                        close_input()
                    continue
                read_output(key.fd)

        # The private supervisor exits only after the adapter leader's original
        # process group has been killed and every adopted descendant reaped.
        returncode = _read_command_result(result_read)
        result_confirmed = True
        close_input()
        selector.unregister(pidfd)
        cleanup_deadline = time.monotonic() + 2
        while any(key.data == "output" for key in selector.get_map().values()):
            remaining = cleanup_deadline - time.monotonic()
            if remaining <= 0:
                raise subprocess.TimeoutExpired(command, 2)
            ready = selector.select(remaining)
            if not ready:
                raise subprocess.TimeoutExpired(command, 2)
            for key, _ in ready:
                read_output(key.fd)
        if not wait_anchor_gone(max(0, cleanup_deadline - time.monotonic())):
            stop_supervisor()
        else:
            process.wait(timeout=max(0, cleanup_deadline - time.monotonic()))
    except BaseException:
        stop_supervisor()
        raise
    finally:
        selector.close()
        if pidfd is not None:
            os.close(pidfd)
        if anchor_pidfd is not None:
            os.close(anchor_pidfd)
        for descriptor in (identity_read, acknowledge_write, result_read):
            if descriptor >= 0:
                os.close(descriptor)
        close_lifeline()
        for candidate in (process.stdin, process.stdout, process.stderr):
            if candidate is not None and not candidate.closed:
                candidate.close()
    stdout = bytes(stdout_buffer).decode("utf-8")
    stderr = bytes(stderr_buffer).decode("utf-8")
    return subprocess.CompletedProcess(command, returncode, stdout, stderr)


class GoogleChatTransport:
    """Public Google Chat REST transport with an operator-provided OAuth access token."""

    def __init__(self, token_env: str = "HERDR_CHAT_TOKEN", token_command: Sequence[str] = (),
                 reaction_user: str | None = None) -> None:
        self.token_env = token_env
        self.token_command = tuple(token_command)
        self.reaction_user = reaction_user

    @staticmethod
    def _http(token: str, path: str, params: dict[str, str], data: bytes | None = None) -> dict[str, object]:
        query = "?" + urlencode(params) if params else ""
        http = Request(f"https://chat.googleapis.com/v1/{path}{query}", data=data,
                       headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"})
        with urlopen(http, timeout=45) as response:
            encoded = bytearray()
            while True:
                remaining = _MAX_REST_RESPONSE_BYTES + 1 - len(encoded)
                if remaining <= 0:
                    raise ValueError(
                        f"Google Chat response exceeds {_MAX_REST_RESPONSE_BYTES} bytes")
                block = response.read(min(64 << 10, remaining))
                if not block:
                    break
                encoded.extend(block)
                if len(encoded) > _MAX_REST_RESPONSE_BYTES:
                    raise ValueError(
                        f"Google Chat response exceeds {_MAX_REST_RESPONSE_BYTES} bytes")
            return decode_json(bytes(encoded), REST_RESPONSE)

    def _react(self, token: str, space: str, request: dict[str, object]) -> dict[str, object]:
        message = get_str(request, "message", "reaction request")
        if re.fullmatch(re.escape(space) + r"/messages/[A-Za-z0-9_.-]+", message) is None:
            raise ValueError("reaction message must belong to the configured space")
        uuid.UUID(get_str(request, "request_id", "reaction request"))
        emoji = _reaction_emoji(request.get("emoji"))
        if emoji is None:
            raise ValueError("reaction request requires a Unicode emoji")
        path = message + "/reactions"

        def identifier(document: dict[str, object]) -> str:
            name = get_str(document, "name", "Google Chat reaction")
            if re.fullmatch(re.escape(path) + r"/[A-Za-z0-9_.-]+", name) is None:
                raise ValueError("Google Chat returned a reaction outside the requested message")
            return name

        if self.reaction_user:
            # Neither users/me nor requestId is part of the documented reaction
            # contract. An explicit OAuth actor lets us reconcile a lost create
            # response without mistaking somebody else's emoji for our ACK.
            params = {"pageSize": "200", "filter":
                      f"emoji.unicode = {json.dumps(emoji, ensure_ascii=False)} AND user.name = {json.dumps(self.reaction_user)}"}
            document = self._http(token, path, params)
            for value in as_sequence(document.get("reactions", []), "Google Chat reactions"):
                reaction = as_mapping(value, "Google Chat reaction")
                user = as_mapping(reaction.get("user", {}), "reaction user")
                existing_emoji = as_mapping(reaction.get("emoji", {}), "reaction emoji")
                if user.get("name") == self.reaction_user and existing_emoji.get("unicode") == emoji:
                    return {"id": identifier(reaction)}
            if document.get("nextPageToken"):
                # A single actor/emoji should yield at most one result. Fail
                # visibly if that contract changes; never perform an unbounded
                # scan before delivering the user's prompt.
                raise ValueError("Google Chat reaction reconciliation unexpectedly requires pagination")
        document = self._http(token, path, {}, json.dumps({"emoji": {"unicode": emoji}}).encode())
        return {"id": identifier(document)}

    def __call__(self, request: dict[str, object]) -> dict[str, object]:
        token = os.environ.get(self.token_env)
        if self.token_command:
            # Permit a conventional trailing LF or CRLF without retaining an
            # adapter-sized stdout buffer for a credential expected to be tiny.
            completed = _run_command(
                self.token_command, timeout=45, stdout_limit=_MAX_TOKEN_BYTES + 2)
            if completed.returncode:
                raise ValueError(f"token command exited {completed.returncode}")
            token = completed.stdout.strip()
        if not token:
            raise ValueError(f"set {self.token_env} to a Google Chat OAuth access token")
        if len(token.encode("utf-8")) > _MAX_TOKEN_BYTES:
            raise ValueError(f"OAuth access token exceeds {_MAX_TOKEN_BYTES} UTF-8 bytes")
        if any(not 0x21 <= ord(character) <= 0x7e for character in token):
            # The HTTP library's invalid-header exception includes the value.
            # Refuse malformed helper output without echoing credentials to logs.
            raise ValueError("OAuth access token must be ASCII without whitespace")
        space = get_str(request, "space", "chat request")
        if re.fullmatch(r"spaces/[A-Za-z0-9_-]+", space) is None:
            raise ValueError("invalid Google Chat space")
        action = request.get("action")
        if action == "react":
            return self._react(token, space, request)
        data: bytes | None = None
        params: dict[str, str] = {}
        if action == "context":
            from agentctl.chat_context import google_context_params
            params.update(google_context_params(request))
        elif action == "poll":
            after = get_str(request, "after", "poll request")
            _timestamp(after)
            params.update(pageSize="100", orderBy="createTime asc", filter=f'createTime > "{after}"')
            cursor = _cursor(request.get("cursor"))
            if cursor:
                params["pageToken"] = cursor
        elif action == "send":
            request_id = get_str(request, "request_id", "send request")
            uuid.UUID(request_id)
            reply_thread = get_str(request, "thread", "send request")
            if re.fullmatch(re.escape(space) + r"/threads/[A-Za-z0-9_.-]+", reply_thread) is None:
                raise ValueError("reply thread must belong to the configured space")
            params.update(requestId=request_id,
                          messageReplyOption="REPLY_MESSAGE_OR_FAIL")
            data = json.dumps({"text": get_str(request, "text", "send request"),
                               "thread": {"name": reply_thread}}).encode()
        else:
            raise ValueError("unknown chat transport action")
        document = self._http(token, space + "/messages", params, data)
        if action == "send":
            identifier = get_str(document, "name", "sent message")
            if not identifier.startswith(space + "/messages/"):
                raise ValueError("Google Chat returned a reply outside the configured space")
            return {"id": identifier}
        messages: list[dict[str, object]] = []
        for value in as_sequence(document.get("messages", []), "Google Chat messages"):
            message = as_mapping(value, "Google Chat message")
            sender = as_mapping(message.get("sender", {}), "Google Chat sender")
            thread = as_mapping(message.get("thread", {}), "Google Chat thread")
            normalized: dict[str, object] = {"id": message.get("name"), "text": message.get("text", ""),
                                            "sender": sender.get("name"), "thread": thread.get("name"),
                                            "created_at": message.get("createTime")}
            if "threadReply" in message:
                if not isinstance(message["threadReply"], bool):
                    raise ValueError("Google Chat threadReply must be a boolean")
                normalized["thread_reply"] = message["threadReply"]
            messages.append(normalized)
        return {"messages": messages,
                "cursor": _cursor(document.get("nextPageToken") or None)}


class Bridge:
    """Restartable inbox, Herdr delivery queue, and idempotent threaded reply outbox."""

    def __init__(self, state: Path, client: HerdrClient | None = None,
                 transport: Transport | None = None) -> None:
        self.state = state.absolute()
        _private(self.state)
        saved_state = _read(self.state / "bridge.json")
        self.config = Config.parse(as_mapping(saved_state["config"], "saved config"))
        # Reject malformed continuation authority before constructing a client
        # or transport that could observe it.
        self._poll_request(saved_state)
        selected_client = client or HerdrClient()
        self.client = _NamedClient(selected_client, self.config.agent_name) if self.config.agent_name else selected_client
        from agentctl.chat_socket import SocketTransport
        self.transport: Transport = transport or (SocketTransport(self.config.transport_socket)
            if self.config.transport_socket else CommandTransport(self.config.transport_command)
            if self.config.transport_command else GoogleChatTransport(
                self.config.token_env, self.config.token_command, self.config.reaction_user))
        for name in (
            "requests", "replies", "reply-receipts", "submissions", "queue", "feedback",
            "deferred",
        ):
            _private(self.state / name)
        # Rebuilt from constant-size request summaries at startup/recovery, then
        # maintained by the single bridge owner. Sent history is never scanned.
        self._usage: dict[str, int] | None = None
        # Full normalized message bytes are immutable after intake. Rebuild
        # this admission counter by streaming durable records at startup and
        # recovery, then maintain it after each atomic request create.
        self._request_usage: dict[str, int] | None = None
        # Preserve exact one-descriptor file sizes when an event-driven owner
        # reuses decoded records. Canonical re-encoding may be smaller than a
        # valid, whitespace-heavy durable artifact; later mutations take the
        # conservative maximum until the next authoritative disk pass.
        self._request_encoded_sizes: dict[str, int] | None = None
        self._aux_usage: dict[str, int] | None = None
        self._queue_reservations: dict[str, int] | None = None
        self._queue_fixed_reservation = 2 * _MAX_QUEUE_ARTIFACT_BYTES + 8192
        self._storage_validated = False
        # Continuous owners prime this once from the durable harness queue and
        # register new prompts before enqueue. One-shot callers intentionally
        # leave it unset and perform an authoritative scan for that operation.
        # Values are one small literal line plus a full-text digest, not the
        # potentially 30 KiB prompt. Request prompts are reconstructed from the
        # already-cached record only when that line is visible in pane output.
        self._prompt_text_cache: dict[str, tuple[str, str]] | None = None

    def _transport_request(self, request: dict[str, object]) -> dict[str, object]:
        """Apply configured outbound authority before invoking any adapter."""
        if (self.config.outbound_mode == "disabled"
                and request.get("action") in ("react", "send")):
            raise ValueError("outbound Chat is disabled for this bridge")
        return self.transport(request)

    @classmethod
    def initialize(cls, state: Path, config: Config, *, after: str | None = None) -> None:
        """Bind new private state to this configuration and a history cutoff."""
        _private(state)
        descriptor = _open_private_lock(str(state / ".bridge.lock"), "chat bridge lock")
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX)
            path = state / "bridge.json"
            if path.exists():
                raise ValueError("bridge already initialized; use a new state directory to change its authority")
            start = after or _utc()
            _timestamp(start)
            _write(path, {"version": 1, "config": asdict(config), "after": start,
                          "cursor": None, "high_water": start, "started_at": start})
        finally:
            os.close(descriptor)

    def _ingest(self, checkpoint: dict[str, object]) -> None:
        result = self._transport_request(self._poll_request(checkpoint))
        self._ingest_result(result, checkpoint)

    def _poll_request(self, checkpoint: dict[str, object]) -> dict[str, object]:
        """Validate durable continuation state before granting it to an adapter."""
        after = get_str(checkpoint, "after", "checkpoint")
        _timestamp(after)
        _timestamp(get_str(checkpoint, "high_water", "checkpoint"))
        _timestamp(get_str(checkpoint, "started_at", "checkpoint"))
        return {
            "action": "poll", "space": self.config.space, "after": after,
            "cursor": _cursor(checkpoint.get("cursor"), "saved poll cursor"),
        }

    def _validate_message_source(self, message: dict[str, object]) -> datetime:
        """Validate normalized resource identity and creation time before saving input."""
        self._validate_message_shape(message)
        identifier = get_str(message, "id", "message")
        thread = get_str(message, "thread", "message")
        if (len(identifier.encode("utf-8")) > _MAX_RESOURCE_BYTES
                or len(thread.encode("utf-8")) > _MAX_RESOURCE_BYTES
                or re.fullmatch(re.escape(self.config.space) + r"/messages/[A-Za-z0-9_.-]+", identifier) is None
                or re.fullmatch(re.escape(self.config.space) + r"/threads/[A-Za-z0-9_.-]+", thread) is None):
            raise ValueError("transport returned a message outside the configured space")
        return _timestamp(get_str(message, "created_at", "message"))

    @staticmethod
    def _validate_message_shape(message: dict[str, object]) -> None:
        required = {"id", "text", "sender", "thread", "created_at"}
        allowed = required | {"thread_reply"}
        missing = required - set(message)
        extra = set(message) - allowed
        if missing:
            raise ValueError(
                "normalized chat message is missing required fields: "
                + ", ".join(sorted(missing)))
        if extra:
            if len(extra) == 1:
                field = next(iter(extra))
                detail = field if len(field.encode("utf-8")) <= 128 else "one oversized field"
            else:
                detail = f"{len(extra)} fields"
            raise ValueError(
                "normalized chat message contains unsupported fields: " + detail)
        for field in required:
            get_str(message, field, "message")
        sender = get_str(message, "sender", "message")
        if (len(sender.encode("utf-8")) > _MAX_SENDER_BYTES
                or re.fullmatch(r"users/[A-Za-z0-9_-]+", sender) is None):
            raise ValueError("message sender must be a canonical users/ID resource")
        if "thread_reply" in message and not isinstance(message["thread_reply"], bool):
            raise ValueError("thread_reply must be a boolean")

    @staticmethod
    def _validate_message_content(message: dict[str, object]) -> int:
        """Validate authorized nonempty text and optional provider reply metadata."""
        Bridge._validate_message_shape(message)
        text = get_str(message, "text", "message")
        if len(text.encode("utf-8")) > 32000:
            raise ValueError("chat message exceeds 32000 bytes")
        source_bytes = Bridge._message_source_bytes(message)
        if source_bytes > _MAX_MESSAGE_SOURCE_BYTES:
            raise ValueError(
                f"normalized chat message exceeds {_MAX_MESSAGE_SOURCE_BYTES} bytes")
        return source_bytes

    def _validate_message(self, message: dict[str, object]) -> tuple[datetime, int]:
        """Validate one exact normalized message before every routing decision."""
        created = self._validate_message_source(message)
        return created, self._validate_message_content(message)

    @staticmethod
    def _message_source_bytes(message: dict[str, object]) -> int:
        """Return the complete normalized message's canonical UTF-8 footprint."""
        try:
            encoded = json.dumps(
                message, sort_keys=True, separators=(",", ":"),
                ensure_ascii=False, allow_nan=False,
            ).encode("utf-8")
        except (TypeError, ValueError) as exc:
            raise ValueError("normalized chat message must be finite JSON data") from exc
        return len(encoded)

    def _request_population(
        self, records: Sequence[dict[str, object]] | None = None,
        *, retained: list[dict[str, object]] | None = None,
    ) -> dict[str, int]:
        """Stream/measure retained request authority without retaining file bodies."""
        count = 0
        source_bytes = 0
        encoded_bytes = 0
        seen: set[str] = set()
        observed_sizes: dict[str, int] = {}
        source: Iterable[tuple[dict[str, object], str | None, int]] = (
            ((record, None, self._cached_request_record_bytes(record)) for record in records)
            if records is not None else
            ((record, path.stem, size)
             for path in (self.state / "requests").glob("*.json")
             for record, size in (self._read_bounded_record(path),))
        )
        for record, path_key, record_bytes in source:
            self._validate_request_record_schema(record)
            key = get_str(record, "key", "request")
            if re.fullmatch(r"[0-9a-f]{64}", key) is None:
                raise ValueError("saved request key must be 64 lowercase hexadecimal characters")
            if path_key is not None and key != path_key:
                raise ValueError("saved request key does not match its file")
            if key in seen:
                raise ValueError("saved request population contains a duplicate key")
            seen.add(key)
            observed_sizes[key] = record_bytes
            message = as_mapping(record.get("message"), "saved source message")
            _, message_bytes = self._validate_message(message)
            count += 1
            source_bytes += message_bytes
            encoded_bytes += record_bytes
            usage = {
                "records": count, "source_bytes": source_bytes,
                "encoded_bytes": encoded_bytes,
            }
            if self._request_limit_reason(usage) is not None:
                # A corrupt/legacy directory can be arbitrarily larger than
                # the configured state. Stop at the first proving record so
                # recovery time and the retained key set stay cap-bounded.
                self._request_usage = usage
                self._request_encoded_sizes = observed_sizes
                return usage
            if retained is not None:
                # Admit each bounded record only after its count and source
                # bytes pass. Never retain the first over-limit record.
                retained.append(record)
        usage = {
            "records": count, "source_bytes": source_bytes,
            "encoded_bytes": encoded_bytes,
        }
        self._request_usage = usage
        self._request_encoded_sizes = observed_sizes
        return usage

    def _cached_request_record_bytes(self, record: dict[str, object]) -> int:
        """Keep exact opened size authority across decoded in-memory snapshots."""
        canonical = len(encoded_json(record, REQUEST))
        key = record.get("key")
        if isinstance(key, str) and self._request_encoded_sizes is not None:
            return max(canonical, self._request_encoded_sizes.get(key, 0))
        return canonical

    @staticmethod
    def _validate_request_record_schema(record: dict[str, object]) -> None:
        """Refuse padding fields and require the fields owned by each storage version."""
        unknown = set(record) - _REQUEST_RECORD_FIELDS
        if unknown:
            raise ValueError(
                f"saved request record contains {len(unknown)} unsupported fields")
        missing = _REQUEST_REQUIRED_FIELDS - set(record)
        if missing:
            raise ValueError(
                "saved request record is missing required fields: "
                + ", ".join(sorted(missing)))

        storage = record.get("reply_storage")
        summary_fields = _REQUEST_REPLY_SUMMARY_FIELDS & set(record)
        if storage is None:
            if summary_fields:
                raise ValueError(
                    "legacy request record contains current reply summary fields")
        elif storage == 2:
            missing_summary = _REQUEST_REPLY_SUMMARY_FIELDS - set(record)
            if missing_summary:
                raise ValueError(
                    "current request record is missing reply summary fields: "
                    + ", ".join(sorted(missing_summary)))
        else:
            raise ValueError("saved request record has an unsupported reply storage version")

        protocol = record.get("reply_protocol")
        if protocol not in (None, 2, 3):
            raise ValueError("saved request record has an unsupported reply protocol")
        if protocol == 3:
            tagged_missing = {"reply_nonce", "reply_next_ordinal"} - set(record)
            if tagged_missing:
                raise ValueError(
                    "protocol-v3 request record is missing fields: "
                    + ", ".join(sorted(tagged_missing)))

    @staticmethod
    def _bounded_file(path: Path, limit: int, label: str) -> os.stat_result:
        metadata = path.lstat()
        if (not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid()
                or metadata.st_mode & 0o077):
            raise ValueError(f"{label} must be a private regular file owned by this account")
        if metadata.st_size > limit:
            raise ValueError(f"{label} exceeds its {limit}-byte encoded file limit")
        return metadata

    def _read_bounded_record(self, path: Path) -> tuple[dict[str, object], int]:
        return _read_bounded_json(
            path, _MAX_REQUEST_FILE_BYTES, "saved request record")

    @staticmethod
    def _request_limit_reason(usage: dict[str, int]) -> str | None:
        if usage["records"] > _MAX_REQUEST_RECORDS:
            return f"request record limit {_MAX_REQUEST_RECORDS} exceeded"
        if usage["source_bytes"] > _MAX_REQUEST_SOURCE_BYTES:
            return f"request source byte limit {_MAX_REQUEST_SOURCE_BYTES} exceeded"
        if usage["encoded_bytes"] > _MAX_REQUEST_ENCODED_BYTES:
            return f"encoded request byte limit {_MAX_REQUEST_ENCODED_BYTES} exceeded"
        return None

    def _record_request_limit(
        self, reason: str, usage: dict[str, int], *, attempted_id: str | None = None,
        attempted_bytes: int | None = None, attempted_encoded_bytes: int | None = None,
    ) -> None:
        """Persist one coalesced, constant-size refusal diagnostic."""
        document: dict[str, object] = {
            "error": reason,
            "records": usage["records"],
            "source_bytes": usage["source_bytes"],
            "encoded_bytes": usage["encoded_bytes"],
            "limits": {"records": _MAX_REQUEST_RECORDS,
                       "source_bytes": _MAX_REQUEST_SOURCE_BYTES,
                       "encoded_bytes": _MAX_REQUEST_ENCODED_BYTES,
                       "message_bytes": _MAX_MESSAGE_SOURCE_BYTES},
        }
        if attempted_id is not None:
            document["attempted_message_sha256"] = hashlib.sha256(
                attempted_id.encode("utf-8")).hexdigest()
        if attempted_bytes is not None:
            document["attempted_source_bytes"] = attempted_bytes
        if attempted_encoded_bytes is not None:
            document["attempted_encoded_bytes"] = attempted_encoded_bytes
        path = self.state / "request-limit.json"
        if path.exists():
            previous = _read(path)
            previous.pop("recorded_at", None)
            if previous == document:
                return
        document["recorded_at"] = _utc()
        _write(path, document)

    def validate_request_population(self) -> dict[str, int]:
        """Refuse oversized saved request state before constructing its R list."""
        usage = self._request_population()
        reason = self._request_limit_reason(usage)
        if reason is not None:
            self._record_request_limit(reason, usage)
            raise ValueError(reason + "; rotate to a fresh Chat state after draining accepted work")
        return usage

    def _load_request_records(self) -> list[dict[str, object]]:
        records: list[dict[str, object]] = []
        usage = self._request_population(retained=records)
        reason = self._request_limit_reason(usage)
        if reason is not None:
            self._record_request_limit(reason, usage)
            raise ValueError(reason + "; rotate to a fresh Chat state after draining accepted work")
        records.sort(key=lambda record: get_str(record, "key", "request"))
        return records

    @staticmethod
    def _document_bytes(document: dict[str, object], label: str) -> int:
        try:
            return len(json.dumps(
                document, sort_keys=True, separators=(",", ":"),
                ensure_ascii=False, allow_nan=False,
            ).encode("utf-8"))
        except (TypeError, ValueError) as exc:
            raise ValueError(f"{label} must be finite JSON data") from exc

    def _aux_population_locked(
        self, records: Sequence[dict[str, object]] | None = None,
        *, collect_caches: bool = False,
    ) -> _AuxPopulationSnapshot:
        """Decode each auxiliary artifact once while queue phase names are stable."""
        request_usage = self._request_usage or self._request_population(records)
        request_reason = self._request_limit_reason(request_usage)
        if request_reason is not None:
            self._record_request_limit(request_reason, request_usage)
            raise ValueError(
                request_reason + "; rotate to a fresh Chat state after draining accepted work")
        request_queue_ids: set[str] = set()
        artifact_bounds: dict[str, int] = {}
        request_source: Iterable[tuple[dict[str, object], str | None]] = (
            ((record, None) for record in records) if records is not None else
            ((record, path.stem)
             for path in (self.state / "requests").glob("*.json")
             for record, _ in (self._read_bounded_record(path),))
        )
        for record, path_key in request_source:
            key = get_str(record, "key", "request")
            queue_id = get_str(record, "queue_id", "request")
            if ((path_key is not None and key != path_key)
                    or re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,254}", queue_id) is None):
                raise ValueError("saved request queue identity is invalid")
            if queue_id in request_queue_ids:
                raise ValueError("saved requests contain a duplicate queue identity")
            request_queue_ids.add(queue_id)
            artifact_bounds[queue_id] = queue_artifact_reservation_bytes(
                self._prompt(record), message_id=queue_id)
            self._check_queue_artifact_bound(artifact_bounds[queue_id])
            if len(request_queue_ids) > _MAX_REQUEST_RECORDS:
                raise ValueError("saved request queue population exceeds its request bound")

        feedback_records = feedback_bytes = 0
        deferred_records = deferred_bytes = 0
        observed_queue_ids: set[str] = set()
        queue_bytes = 0
        sidecar_sizes: dict[str, int] = {}
        temporary_records = 0
        fixed_reservation = 2 * _MAX_QUEUE_ARTIFACT_BYTES + 8192
        reserved_bytes = fixed_reservation + sum(
            2 * size + 2 * QUEUE_ERROR_SIDECAR_MAX_BYTES for size in artifact_bounds.values())
        feedback_texts: dict[str, str] = {}
        deferred_messages: dict[Path, dict[str, object]] = {}
        prompt_index: dict[str, tuple[str, str]] = {}
        queue_phases: dict[str, str] = {}

        def usage() -> dict[str, int]:
            measured = {
                "feedback_records": feedback_records,
                "feedback_bytes": feedback_bytes,
                "deferred_records": deferred_records,
                "deferred_bytes": deferred_bytes,
                "queue_records": len(observed_queue_ids),
                "queue_bytes": queue_bytes,
                "queue_reserved_bytes": reserved_bytes,
                "queue_temporary_records": temporary_records,
            }
            return measured

        def snapshot(measured: dict[str, int]) -> _AuxPopulationSnapshot:
            feedback_pending: dict[str, tuple[str, bool]] = {}
            for identifier, text in feedback_texts.items():
                phase = queue_phases.get(identifier)
                if phase is None:
                    feedback_pending[identifier] = (text, False)
                elif phase in ("inbox", "inflight"):
                    feedback_pending[identifier] = (text, True)
            return _AuxPopulationSnapshot(
                usage=dict(measured), prompt_index=dict(prompt_index),
                feedback_pending=feedback_pending,
                deferred_messages=dict(deferred_messages),
                queue_reservations={
                    key: 2 * size + 2 * max(QUEUE_ERROR_SIDECAR_MAX_BYTES, sidecar_sizes.get(key, 0))
                    for key, size in artifact_bounds.items()},
                queue_fixed_reservation=fixed_reservation,
            )

        feedback_queue_ids: set[str] = set()
        for path in (self.state / "feedback").glob("*.json"):
            feedback, _ = _read_bounded_json(
                path, _MAX_FEEDBACK_FILE_BYTES, "saved feedback record")
            queue_id = get_str(feedback, "queue_id", "reply feedback")
            if (queue_id != f"feedback-{path.stem}"
                    or re.fullmatch(r"feedback-[0-9a-f]{64}", queue_id) is None):
                raise ValueError("saved feedback queue identity is invalid")
            if queue_id in request_queue_ids:
                raise ValueError("saved request and feedback records share a queue identity")
            _timestamp(get_str(feedback, "created_at", "reply feedback"))
            if queue_id in feedback_queue_ids:
                raise ValueError("saved feedback contains a duplicate queue identity")
            feedback_queue_ids.add(queue_id)
            feedback_text = get_str(feedback, "text", "reply feedback")
            if collect_caches:
                feedback_texts[queue_id] = feedback_text
            artifact_bounds[queue_id] = queue_artifact_reservation_bytes(
                feedback_text, message_id=queue_id)
            self._check_queue_artifact_bound(artifact_bounds[queue_id])
            reserved_bytes += 2 * artifact_bounds[queue_id] + 2 * QUEUE_ERROR_SIDECAR_MAX_BYTES
            feedback_records += 1
            feedback_bytes += self._document_bytes(feedback, "reply feedback")
            measured = usage()
            if self._aux_limit_reason(measured) is not None:
                return snapshot(measured)

        for path in (self.state / "deferred").glob("*.json"):
            message, _ = _read_bounded_json(
                path, _MAX_REQUEST_FILE_BYTES, "deferred message record")
            identifier = get_str(message, "id", "deferred message")
            self._validate_message_source(message)
            source_bytes = self._validate_message_content(message)
            if hashlib.sha256(identifier.encode("utf-8")).hexdigest() != path.stem:
                raise ValueError("deferred message identity does not match its file")
            if collect_caches:
                deferred_messages[path] = message
            deferred_records += 1
            deferred_bytes += source_bytes
            measured = usage()
            if self._aux_limit_reason(measured) is not None:
                return snapshot(measured)

        allowed_queue_ids = request_queue_ids | feedback_queue_ids
        for phase in ("inbox", "inflight", "processed", "failed"):
            directory = self.state / "queue" / phase
            if not directory.exists():
                continue
            _validate_private_directory(str(directory), "chat queue phase", tighten=False)
            for path in directory.glob("*.json"):
                queued, encoded_size = _read_bounded_json(
                    path, _MAX_QUEUE_ARTIFACT_BYTES, "saved queue artifact")
                identifier = get_str(queued, "id", "queued prompt")
                prompt = get_str(queued, "text", "queued prompt")
                if identifier != path.stem or identifier not in allowed_queue_ids:
                    raise ValueError("saved queue artifact has no matching request or feedback record")
                if identifier in observed_queue_ids:
                    raise ValueError("saved queue identity appears in more than one phase")
                observed_queue_ids.add(identifier)
                if collect_caches:
                    queue_phases[identifier] = phase
                    prompt_index[identifier] = self._prompt_cache_entry(prompt)
                queue_bytes += encoded_size
                old_bound = artifact_bounds[identifier]
                rewritten_bytes = len((json.dumps(queued, indent=2, sort_keys=True) + "\n").encode("utf-8"))
                artifact_bounds[identifier] = max(
                    old_bound, max(encoded_size, rewritten_bytes) + QUEUE_UPDATE_MAX_BYTES)
                self._check_queue_artifact_bound(artifact_bounds[identifier])
                reserved_bytes += 2 * (artifact_bounds[identifier] - old_bound)
                measured = usage()
                if self._aux_limit_reason(measured) is not None:
                    return snapshot(measured)

            for path in directory.glob("*.json.error"):
                document, encoded_size = _read_bounded_json(
                    path, _MAX_QUEUE_ARTIFACT_BYTES, "saved queue error sidecar")
                identifier = path.name.removesuffix(".json.error")
                if identifier not in allowed_queue_ids or identifier in sidecar_sizes:
                    raise ValueError("saved queue error sidecar has no unique request or feedback identity")
                if document.get("artifact") != identifier + ".json":
                    raise ValueError("saved queue error sidecar names a different artifact")
                get_str(document, "error", "saved queue error sidecar")
                sidecar_sizes[identifier] = encoded_size
                queue_bytes += encoded_size
                reserved_bytes += 2 * max(0, encoded_size - QUEUE_ERROR_SIDECAR_MAX_BYTES)
                measured = usage()
                if self._aux_limit_reason(measured) is not None:
                    return snapshot(measured)

        # A crash can leave pre-rename .message.* files behind. They are
        # retained evidence, not spare capacity: count bytes and entries in
        # every queue directory, including target.json's root directory.
        queue_root = self.state / "queue"
        for directory in (queue_root, *(queue_root / phase for phase in (
            "inbox", "inflight", "processed", "failed"))):
            for path in directory.glob(".message.*"):
                try:
                    metadata = self._bounded_file(
                        path, _MAX_QUEUE_ARTIFACT_BYTES, "queue atomic temporary artifact")
                except FileNotFoundError:
                    # A concurrently committing delivery may consume its own
                    # temporary; its per-ID reservation already covers it.
                    continue
                temporary_records += 1
                queue_bytes += metadata.st_size
                fixed_reservation += metadata.st_size
                reserved_bytes += metadata.st_size
                measured = usage()
                if (temporary_records > _MAX_REQUEST_RECORDS + _MAX_FEEDBACK_RECORDS + 16
                        or self._aux_limit_reason(measured) is not None):
                    return snapshot(measured)
        for name in ("target.json", ".delivery.lock", ".binding.lock"):
            path = queue_root / name
            if path.exists():
                metadata = self._bounded_file(
                    path, _MAX_QUEUE_ARTIFACT_BYTES if name == "target.json" else 4096,
                    "queue control artifact")
                queue_bytes += metadata.st_size
        # The fixed allowance reserves two maximum control artifacts and a
        # small lock envelope independently of the request/feedback budget.
        return snapshot(usage())

    def _aux_population(
        self, records: Sequence[dict[str, object]] | None = None,
        *, collect_caches: bool = False,
    ) -> _AuxPopulationSnapshot:
        """Return an auxiliary snapshot serialized with every queue mutation.

        Queue enqueue and drain take ``.delivery.lock`` before creating, replacing,
        or renaming an artifact. Some callers already hold ``.bridge.lock``; that
        bridge-to-delivery order is safe because queue code never acquires the
        bridge lock, and its separate binding lock is released before delivery.
        """
        descriptor = _open_private_lock(
            str(self.state / "queue" / ".delivery.lock"), "queue delivery lock")
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX)
            return self._aux_population_locked(records, collect_caches=collect_caches)
        finally:
            os.close(descriptor)

    @staticmethod
    def _aux_limit_reason(usage: dict[str, int]) -> str | None:
        limits = (
            (usage["feedback_records"] > _MAX_FEEDBACK_RECORDS,
             f"feedback record limit {_MAX_FEEDBACK_RECORDS} exceeded"),
            (usage["feedback_bytes"] > _MAX_FEEDBACK_BYTES,
             f"feedback byte limit {_MAX_FEEDBACK_BYTES} exceeded"),
            (usage["deferred_records"] > _MAX_DEFERRED_RECORDS,
             f"deferred message limit {_MAX_DEFERRED_RECORDS} exceeded"),
            (usage["deferred_bytes"] > _MAX_DEFERRED_BYTES,
             f"deferred message byte limit {_MAX_DEFERRED_BYTES} exceeded"),
            (usage["queue_records"] > _MAX_REQUEST_RECORDS + _MAX_FEEDBACK_RECORDS,
             "queue artifact population exceeds its derived request/feedback limit"),
            (max(usage["queue_bytes"], usage.get("queue_reserved_bytes", 0)) > _MAX_QUEUE_BYTES,
             f"queue artifact byte limit {_MAX_QUEUE_BYTES} exceeded"),
            (usage.get("queue_temporary_records", 0) > _MAX_REQUEST_RECORDS + _MAX_FEEDBACK_RECORDS + 16,
             "queue temporary artifact population limit exceeded"),
        )
        return next((message for failed, message in limits if failed), None)

    def _record_aux_limit(self, reason: str, usage: dict[str, int]) -> None:
        document: dict[str, object] = {
            "error": reason,
            "usage": dict(usage),
            "limits": {
                "feedback_records": _MAX_FEEDBACK_RECORDS,
                "feedback_bytes": _MAX_FEEDBACK_BYTES,
                "deferred_records": _MAX_DEFERRED_RECORDS,
                "deferred_bytes": _MAX_DEFERRED_BYTES,
                "queue_records": _MAX_REQUEST_RECORDS + _MAX_FEEDBACK_RECORDS,
                "queue_bytes": _MAX_QUEUE_BYTES,
                "queue_artifact_bytes": _MAX_QUEUE_ARTIFACT_BYTES,
            },
        }
        path = self.state / "population-limit.json"
        if path.exists():
            previous = _read(path)
            previous.pop("recorded_at", None)
            if previous == document:
                return
        document["recorded_at"] = _utc()
        _write(path, document)

    def _validated_aux_snapshot(
        self, *, install_prompt_index: bool,
        records: Sequence[dict[str, object]] | None = None,
    ) -> _AuxPopulationSnapshot:
        try:
            snapshot = self._aux_population(records, collect_caches=install_prompt_index)
            reason = self._aux_limit_reason(snapshot.usage)
            if reason is not None:
                self._record_aux_limit(reason, snapshot.usage)
                raise ValueError(reason + "; rotate to a fresh Chat state after draining accepted work")
        except BaseException:
            # A prefix of a failed scan is never admission authority. The next
            # caller must reconstruct every count/reservation from disk.
            self._aux_usage = None
            self._queue_reservations = None
            self._prompt_text_cache = None
            raise
        # Publish only a complete, cap-validated snapshot, on the owner thread.
        self._aux_usage = dict(snapshot.usage)
        self._queue_reservations = dict(snapshot.queue_reservations)
        self._queue_fixed_reservation = snapshot.queue_fixed_reservation
        if install_prompt_index:
            # The queue was immobile throughout the scan, so replacement is exact;
            # the old merge behavior was needed only to survive phase-move races.
            self._prompt_text_cache = dict(snapshot.prompt_index)
        return snapshot

    def validate_aux_snapshot(
        self, records: Sequence[dict[str, object]] | None = None,
    ) -> _AuxPopulationSnapshot:
        """Validate and install the one-pass caches needed by a continuous owner."""
        return self._validated_aux_snapshot(install_prompt_index=True, records=records)

    def validate_aux_population(self) -> dict[str, int]:
        """Stream bodies without retaining caches when only bounded usage is needed."""
        return dict(self._validated_aux_snapshot(
            install_prompt_index=False, records=None).usage)

    def _check_queue_artifact_bound(self, size: int) -> None:
        if size > _MAX_QUEUE_ARTIFACT_BYTES:
            reason = f"queue artifact exceeds its {_MAX_QUEUE_ARTIFACT_BYTES}-byte reserved limit"
            self._record_aux_limit(reason, self._aux_usage or {
                "feedback_records": 0, "feedback_bytes": 0,
                "deferred_records": 0, "deferred_bytes": 0,
                "queue_records": 0, "queue_bytes": 0, "queue_reserved_bytes": 0})
            raise ValueError(reason)

    def _reserve_queue_prompts(self, prompts: Sequence[tuple[str, str]]) -> None:
        """Reserve a whole admission batch, idempotently, before its durable writes.

        Reserve both old/new atomic artifacts and bounded sidecars. A failed
        enqueue retains its reservation until recovery; retries cannot consume
        it twice or rely on a worker racing the owner's byte accounting.
        """
        if self._queue_reservations is None or self._aux_usage is None:
            self.validate_aux_population()
        assert self._queue_reservations is not None and self._aux_usage is not None
        additions: dict[str, int] = {}
        for identifier, prompt in prompts:
            if identifier in self._queue_reservations or identifier in additions:
                continue
            size = queue_artifact_reservation_bytes(prompt, message_id=identifier)
            self._check_queue_artifact_bound(size)
            additions[identifier] = 2 * size + 2 * QUEUE_ERROR_SIDECAR_MAX_BYTES
        proposed = dict(self._aux_usage)
        proposed["queue_reserved_bytes"] = (
            self._queue_fixed_reservation + sum(self._queue_reservations.values()) + sum(additions.values()))
        reason = self._aux_limit_reason(proposed)
        if reason is not None:
            self._record_aux_limit(reason, proposed)
            raise ValueError(reason + "; rotate to a fresh Chat state after draining accepted work")
        self._queue_reservations.update(additions)
        self._aux_usage = proposed

    def _ingest_result(
        self, result: dict[str, object], checkpoint: dict[str, object] | None = None,
        records: Sequence[dict[str, object]] | None = None,
        own_replies: set[object] | None = None,
    ) -> list[tuple[Path, dict[str, object]]]:
        known_replies = set() if own_replies is None else own_replies
        # Recompute from the caller's already-loaded snapshot, or from one
        # bounded disk pass for polling callers. Request records can grow as
        # lifecycle fields are added, so a prior encoded-byte cache is not
        # admission authority for a new record.
        durable_usage = dict(self._request_population(records))
        current_reason = self._request_limit_reason(durable_usage)
        if current_reason is not None:
            self._record_request_limit(current_reason, durable_usage)
            raise ValueError(
                current_reason + "; rotate to a fresh Chat state after draining accepted work")
        planned_usage = dict(durable_usage)
        known_keys = ({get_str(record, "key", "request") for record in records}
                      if records is not None else {
                          path.stem for path in (self.state / "requests").glob("*.json")})
        caller_known_keys = set(known_keys)
        admitted: list[tuple[Path, dict[str, object]]] = []
        admitted_keys: set[str] = set()
        planned_keys: dict[str, str] = {}
        planned: list[tuple[Path, dict[str, object], int, int]] = []
        saved = checkpoint if checkpoint is not None else _read(self.state / "bridge.json")
        high = _timestamp(get_str(saved, "high_water", "checkpoint"))
        start = _timestamp(get_str(saved, "started_at", "checkpoint"))
        cursor = _cursor(result.get("cursor"))
        for value in as_sequence(result.get("messages"), "polled messages"):
            message = as_mapping(value, "polled message")
            created, source_bytes = self._validate_message(message)
            identifier = get_str(message, "id", "message")
            if (identifier in known_replies
                    or (own_replies is None and self.config.outbound_mode == "enabled"
                        and self._is_own_reply(identifier))):
                continue
            high = max(high, created)
            if created < start:
                continue
            if message.get("sender") not in self.config.allowed_sender_set or not message.get("text"):
                continue
            key = hashlib.sha256(identifier.encode()).hexdigest()
            path = self.state / "requests" / f"{key}.json"
            if key in planned_keys:
                if planned_keys[key] != identifier:
                    raise ValueError("request identity hash collides within one provider batch")
                continue
            if path.exists():
                existing, _ = self._read_bounded_record(path)
                self._validate_request_record_schema(existing)
                existing_message = as_mapping(existing.get("message"), "saved source message")
                if get_str(existing_message, "id", "saved source message") != identifier:
                    raise ValueError("request identity hash collides with a different source message")
                if key not in known_keys:
                    # A prior atomic create may have reached disk before its
                    # caller observed an fsync error. Repair the in-memory
                    # admission counter from authority before continuing.
                    durable_usage = dict(self._request_population())
                    known_keys = {
                        candidate.stem for candidate in (self.state / "requests").glob("*.json")}
                    planned_usage = {
                        "records": durable_usage["records"] + len(planned),
                        "source_bytes": durable_usage["source_bytes"]
                        + sum(item_bytes for _, _, item_bytes, _ in planned),
                        "encoded_bytes": durable_usage["encoded_bytes"]
                        + sum(item_bytes for _, _, _, item_bytes in planned),
                    }
                if key not in caller_known_keys and key not in admitted_keys:
                    admitted.append((path, existing))
                    admitted_keys.add(key)
                continue
            record: dict[str, object] = {"key": key, "message": message, "phase": "received",
                                        "queue_id": f"{int(created.timestamp() * 1_000_000):020d}-{key}",
                                        "request_id": str(uuid.uuid5(uuid.NAMESPACE_URL, identifier)),
                                        "received_at": _utc(), "ack": self._ack_record(identifier),
                                        "reply_storage": 2, "reply_item_count": 0,
                                        "reply_sent_count": 0, "reply_ordinal_offset": 0,
                                        "reply_total_bytes": 0, "reply_pending_bytes": 0}
            if self.config.outbound_mode == "enabled" and self.config.reply_mode == "tagged":
                # Persist an unpredictable marker before the prompt can reach the TUI.
                # Source text cannot know it; retained output from another request cannot match it.
                record["reply_nonce"] = secrets.token_urlsafe(16)
                record["reply_protocol"] = 3
                record["reply_next_ordinal"] = 1
            record_bytes = len(encoded_json(record, REQUEST))
            proposed = {
                "records": planned_usage["records"] + 1,
                "source_bytes": planned_usage["source_bytes"] + source_bytes,
                "encoded_bytes": planned_usage["encoded_bytes"] + record_bytes,
            }
            reason = self._request_limit_reason(proposed)
            if reason is not None:
                self._record_request_limit(
                    reason, durable_usage, attempted_id=identifier,
                    attempted_bytes=source_bytes, attempted_encoded_bytes=record_bytes)
                raise ValueError(
                    reason + "; rotate to a fresh Chat state after draining accepted work")
            planned.append((path, record, source_bytes, record_bytes))
            planned_keys[key] = identifier
            planned_usage = proposed
            known_keys.add(key)
        final_reason = self._request_limit_reason(planned_usage)
        if final_reason is not None:
            self._record_request_limit(final_reason, durable_usage)
            raise ValueError(
                final_reason + "; rotate to a fresh Chat state after draining accepted work")
        committed_usage = dict(durable_usage)
        self._reserve_queue_prompts([
            (get_str(record, "queue_id", "request"), self._prompt(record))
            for _, record, _, _ in planned])
        try:
            for path, record, source_bytes, record_bytes in planned:
                _write(path, record)
                committed_usage["records"] += 1
                committed_usage["source_bytes"] += source_bytes
                committed_usage["encoded_bytes"] += record_bytes
                if self._request_encoded_sizes is None:
                    self._request_encoded_sizes = {}
                self._request_encoded_sizes[get_str(record, "key", "request")] = record_bytes
                self._request_usage = dict(committed_usage)
        except BaseException:
            # An atomic rename may have reached durable storage before a later
            # directory fsync reported failure. Force the next operation to
            # reconstruct admission authority instead of trusting a low cache.
            self._request_usage = None
            self._request_encoded_sizes = None
            raise
        admitted.extend((path, record) for path, record, _, _ in planned)
        if checkpoint is None:
            return admitted
        original_checkpoint = dict(checkpoint)
        checkpoint.update(cursor=cursor or None, high_water=high.isoformat().replace("+00:00", "Z"))
        if not cursor:
            # Overlap the high-water timestamp: messages with equal timestamps and brief
            # indexing delays are deduplicated by their immutable resource names.
            checkpoint["after"] = (high - timedelta(seconds=60)).isoformat().replace("+00:00", "Z")
        if checkpoint != original_checkpoint:
            _write(self.state / "bridge.json", checkpoint)
        return admitted

    def _ack_record(self, message: str, *, completed_legacy: bool = False) -> dict[str, object]:
        disabled = (self.config.outbound_mode == "disabled"
                    or self.config.ack_reaction is None or completed_legacy)
        return {"state": "disabled" if disabled else "pending",
                "emoji": None if self.config.outbound_mode == "disabled" else self.config.ack_reaction,
                "request_id": str(uuid.uuid5(uuid.NAMESPACE_URL, message + "#agentctl-ack")),
                "attempts": 0, "error": None, "next_retry_at": None,
                "last_attempt_at": None, "reaction_id": None, "acked_at": None}

    def _acknowledge(self) -> None:
        """Attempt due durable ACKs without coupling failure to agent execution."""
        for path in sorted((self.state / "requests").glob("*.json")):
            record = _read(path)
            message = as_mapping(record["message"], "source message")
            identifier = get_str(message, "id", "source message")
            if "ack" not in record:
                # Upgrade unfinished work, but do not decorate historical final
                # answers when an existing bridge first gains ACK support.
                record["ack"] = self._ack_record(identifier, completed_legacy=record.get("phase") == "replied")
                _write(path, record)
            ack = as_mapping(record["ack"], "request acknowledgement")
            if self.config.outbound_mode == "disabled":
                if (ack.get("state") != "disabled" or ack.get("emoji") is not None
                        or ack.get("error") is not None or ack.get("next_retry_at") is not None):
                    ack.update(state="disabled", emoji=None, error=None, next_retry_at=None)
                    record["ack"] = ack
                    _write(path, record)
                continue
            if ack.get("state") != "pending":
                continue
            now = datetime.now(timezone.utc)
            retry_at = ack.get("next_retry_at")
            if isinstance(retry_at, str) and _timestamp(retry_at) > now:
                continue
            attempts = ack.get("attempts", 0)
            if not isinstance(attempts, int) or isinstance(attempts, bool) or attempts < 0:
                raise ValueError("ack attempts must be a nonnegative integer")
            attempts += 1
            retry = now + timedelta(seconds=min(60, 3 * 2 ** min(attempts - 1, 5)))
            ack.update(attempts=attempts, last_attempt_at=now.isoformat().replace("+00:00", "Z"),
                       next_retry_at=retry.isoformat().replace("+00:00", "Z"))
            record["ack"] = ack
            # Commit the attempt before contacting Chat. A crash or lost response
            # retries the same identity after the saved deadline.
            _write(path, record)
            try:
                result = self._transport_request({"action": "react", "space": self.config.space,
                    "message": identifier, "emoji": ack["emoji"], "request_id": ack["request_id"]})
                reaction = get_str(result, "id", "acknowledgement reaction")
                if re.fullmatch(re.escape(identifier) + r"/reactions/[A-Za-z0-9_.-]+", reaction) is None:
                    raise ValueError("transport returned a reaction outside the requested message")
            except (OSError, ValueError, TypeError, HerdrRunError, subprocess.SubprocessError) as exc:
                ack["error"] = str(exc)[:2000]
            else:
                ack.update(state="acked", reaction_id=reaction, acked_at=_utc(), error=None, next_retry_at=None)
            _write(path, record)

    def _prompt(self, record: dict[str, object]) -> str:
        message = as_mapping(record["message"], "message")
        key = get_str(record, "key", "request")
        context = ""
        if message.get("thread_reply") is True:
            launcher = Path(__file__).resolve().parent.parent / "bin" / "agentctl"
            invocation = ([str(launcher), "chat"] if launcher.is_file() and os.access(launcher, os.X_OK)
                          else [sys.executable, str(Path(__file__).resolve())])
            command = shlex.join([*invocation, "context", "--state", str(self.state),
                                  "--request", key[:12], "--limit", "10"])
            context = ("Run this for the prior 10 messages in this thread; read further back if needed:\n"
                       f"{command}\n")
        if self.config.outbound_mode == "disabled":
            return ("A message from an authorized user arrived through your configured inbound-only chat bridge.\n"
                    f"Source: {message['id']}\nSender: {message['sender']}\n{context}\n"
                    f"{message['text']}\n\n"
                    "Complete this user's request using your normal instructions and tools. Outbound Chat is "
                    "disabled for this bridge: no Chat reply will be published. Do not emit CHAT_REPLY tags "
                    "or create a reply file for this request.")
        nonce = record.get("reply_nonce")
        if isinstance(nonce, str):
            ordinal = record.get("reply_next_ordinal", 1)
            if type(ordinal) is not int:
                raise ValueError("saved chat reply ordinal must be an integer")
            return ("The user's request arrived through the chat bridge.\n"
                    f"Source: {message['id']}\n"
                    f"{_reply_instruction(nonce, ordinal)}\n"
                    f"{context}\n{message['text']}")
        reply = shlex.join([sys.executable, str(Path(__file__).resolve()), "reply", "--state", str(self.state),
                            "--request", key, "--file", "PATH_TO_YOUR_REPLY"])
        return ("A message from an authorized user arrived through your configured chat bridge.\n"
                f"Source: {message['id']}\nSender: {message['sender']}\n{context}\n"
                f"{message['text']}\n\n"
                "Complete this user's request using your normal instructions and tools. Only you are the "
                "chat coordinator; manage other long-lived agents through agentctl as needed. "
                "To return your final user-facing answer to the originating chat thread, write it to a "
                "UTF-8 file and run this command, replacing PATH_TO_YOUR_REPLY with that file:\n"
                f"{reply}\nDo not send a separate chat message; the bridge delivers this reply durably.")

    @staticmethod
    def _reply_summary_count(record: dict[str, object], field: str) -> int:
        value = record.get(field, 0)
        if type(value) is not int or not 0 <= value <= _MAX_REPLY_ITEMS:
            raise ValueError(f"saved {field} must be an integer between 0 and {_MAX_REPLY_ITEMS}")
        return value

    @staticmethod
    def _reply_summary_bytes(record: dict[str, object], field: str) -> int:
        value = record.get(field, 0)
        if type(value) is not int or not 0 <= value <= _MAX_REPLY_ITEMS * 30_000:
            raise ValueError(f"saved {field} is outside the durable reply byte range")
        return value

    @staticmethod
    def _reply_identity(record: dict[str, object], index: int) -> str:
        base = uuid.UUID(get_str(record, "request_id", "request"))
        return str(base if index == 0 else uuid.uuid5(base, f"reply:{index}"))

    def _reply_summary(self, record: dict[str, object]) -> tuple[int, int, int, int]:
        if record.get("reply_storage") != 2:
            raise ValueError("reply outbox migration was not completed")
        count = self._reply_summary_count(record, "reply_item_count")
        sent = self._reply_summary_count(record, "reply_sent_count")
        total_bytes = self._reply_summary_bytes(record, "reply_total_bytes")
        pending_bytes = self._reply_summary_bytes(record, "reply_pending_bytes")
        if sent > count or pending_bytes > total_bytes:
            raise ValueError("saved reply summary counts or bytes are inconsistent")
        return count, sent, total_bytes, pending_bytes

    def _validate_reply_item(
        self, record: dict[str, object], item: dict[str, object], index: int,
    ) -> dict[str, object]:
        allowed = {
            "reply_key", "request_id", "reply_source", "text", "reply_ordinal",
            "reply_shift_from", "reply_id", "replied_at",
        }
        unknown = set(item) - allowed
        if unknown:
            raise ValueError(
                "reply artifact contains unsupported fields: " + ", ".join(sorted(unknown)))
        if item.get("reply_key") != str(index):
            raise ValueError("reply artifact key does not match its sequence path")
        if get_str(item, "request_id", "reply item") != self._reply_identity(record, index):
            raise ValueError("reply artifact request ID does not match its stable sequence identity")
        text = get_str(item, "text", "reply item")
        if not text.strip() or len(text.encode("utf-8")) > 30_000:
            raise ValueError("reply artifact text must contain 1-30000 UTF-8 bytes")
        source = item.get("reply_source")
        if source not in ("file", "capture", "legacy"):
            raise ValueError("reply artifact source must be file, capture, or legacy")
        ordinal = item.get("reply_ordinal")
        if ordinal is not None and (type(ordinal) is not int or not 1 <= ordinal <= 999_999):
            raise ValueError("reply artifact ordinal must be between 1 and 999999")
        shift = item.get("reply_shift_from")
        if shift is not None and not (
            shift == 0 and index == 1 and source == "capture" and ordinal == 1
        ):
            raise ValueError("reply shift journal has invalid transaction metadata")
        identifier = item.get("reply_id")
        if identifier is not None and (
            not isinstance(identifier, str)
            or re.fullmatch(re.escape(self.config.space) + r"/messages/[A-Za-z0-9_.-]+", identifier) is None
        ):
            raise ValueError("reply artifact provider ID belongs outside the configured space")
        replied_at = item.get("replied_at")
        if replied_at is not None:
            if not isinstance(replied_at, str):
                raise ValueError("reply artifact completion time must be a string")
            _timestamp(replied_at)
        if (identifier is None) != (replied_at is None):
            raise ValueError("reply artifact provider ID and completion time must appear together")
        return item

    def _read_reply_item(self, record: dict[str, object], index: int, *, sent: bool) -> dict[str, object]:
        key = get_str(record, "key", "request")
        return self._validate_reply_item(
            record, _read(_reply_item_path(self.state, key, index, sent=sent)), index)

    def _create_reply_item(
        self, record: dict[str, object], index: int, item: dict[str, object], *, sent: bool = False,
    ) -> tuple[dict[str, object], bool]:
        key = get_str(record, "key", "request")
        path = _reply_item_path(self.state, key, index, sent=sent, create=True)
        proposed = self._validate_reply_item(record, item, index)
        try:
            _create_chat_json(self.state, path, proposed)
        except FileExistsError:
            return self._validate_reply_item(record, _read(path), index), False
        return proposed, True

    def _write_pending_reply(self, record: dict[str, object], item: dict[str, object]) -> None:
        index = int(get_str(item, "reply_key", "reply item"))
        validated = self._validate_reply_item(record, item, index)
        key = get_str(record, "key", "request")
        _write(_reply_item_path(self.state, key, index, sent=False, create=True), validated)

    def _ensure_reply_receipt(self, record: dict[str, object], item: dict[str, object]) -> None:
        identifier = get_str(item, "reply_id", "reply item")
        digest = hashlib.sha256(identifier.encode()).hexdigest()
        directory = self.state / "reply-receipts" / digest[:2]
        created = False
        try:
            directory.mkdir(mode=0o700)
            created = True
        except FileExistsError:
            pass
        _validate_private_directory(
            str(directory), "chat reply receipt directory", tighten=False)
        if created:
            _fsync_dir(str(directory.parent))
        path = directory / f"{digest}.json"
        document: dict[str, object] = {
            "reply_id": identifier, "request": get_str(record, "key", "request"),
            "reply_key": get_str(item, "reply_key", "reply item")}
        try:
            _create_chat_json(self.state, path, document)
        except FileExistsError:
            if _read(path) != document:
                raise ValueError("reply receipt hash collides with different durable state") from None

    def _is_own_reply(self, identifier: str) -> bool:
        digest = hashlib.sha256(identifier.encode()).hexdigest()
        directory = self.state / "reply-receipts" / digest[:2]
        try:
            directory.lstat()
        except FileNotFoundError:
            return False
        _validate_private_directory(
            str(directory), "chat reply receipt directory", tighten=False)
        path = directory / f"{digest}.json"
        return path.exists() and _read(path).get("reply_id") == identifier

    def _rebuild_reply_usage(
        self, records: Sequence[dict[str, object]] | None = None,
    ) -> dict[str, int]:
        usage = {"items": 0, "bytes": 0, "pending_items": 0, "pending_bytes": 0}
        loaded = (records if records is not None else
                  (_read(path) for path in (self.state / "requests").glob("*.json")))
        for record in loaded:
            count, sent, total_bytes, pending_bytes = self._reply_summary(record)
            if count > _MAX_REQUEST_REPLY_ITEMS or total_bytes > _MAX_REQUEST_REPLY_BYTES:
                raise ValueError("reply recovery exceeds the per-request storage limits")
            usage["items"] += count
            usage["bytes"] += total_bytes
            usage["pending_items"] += count - sent
            usage["pending_bytes"] += pending_bytes
            if (usage["items"] > _MAX_STATE_REPLY_ITEMS
                    or usage["bytes"] > _MAX_STATE_REPLY_BYTES
                    or usage["pending_items"] > _MAX_PENDING_REPLY_ITEMS
                    or usage["pending_bytes"] > _MAX_PENDING_REPLY_BYTES):
                raise ValueError("reply recovery exceeds the state-wide storage limits")
        self._usage = usage
        return usage

    def _reply_usage(self) -> dict[str, int]:
        return self._usage if self._usage is not None else self._rebuild_reply_usage()

    def _capacity_error(
        self, record: dict[str, object], *, add_items: int, add_bytes: int,
    ) -> str | None:
        count, sent, total_bytes, pending_bytes = self._reply_summary(record)
        usage = self._reply_usage()
        checks = (
            (count + add_items > _MAX_REQUEST_REPLY_ITEMS,
             f"per-request reply count limit {_MAX_REQUEST_REPLY_ITEMS} reached"),
            (total_bytes + add_bytes > _MAX_REQUEST_REPLY_BYTES,
             f"per-request reply byte limit {_MAX_REQUEST_REPLY_BYTES} reached"),
            (usage["items"] + add_items > _MAX_STATE_REPLY_ITEMS,
             f"state-wide reply count limit {_MAX_STATE_REPLY_ITEMS} reached"),
            (usage["bytes"] + add_bytes > _MAX_STATE_REPLY_BYTES,
             f"state-wide reply byte limit {_MAX_STATE_REPLY_BYTES} reached"),
            (usage["pending_items"] + add_items > _MAX_PENDING_REPLY_ITEMS,
             f"state-wide pending reply count limit {_MAX_PENDING_REPLY_ITEMS} reached"),
            (usage["pending_bytes"] + add_bytes > _MAX_PENDING_REPLY_BYTES,
             f"state-wide pending reply byte limit {_MAX_PENDING_REPLY_BYTES} reached"),
        )
        return next((message for failed, message in checks if failed), None)

    def _close_for_storage_limit(
        self, path: Path, record: dict[str, object], reason: str,
    ) -> None:
        before = dict(record)
        record.setdefault("reply_closed_at", _utc())
        record["reply_close_reason"] = "storage_limit"
        record["reply_storage_error"] = reason
        if record != before:
            _write(path, record)

    def _reject_submission(
        self, path: Path, record: dict[str, object], submission_path: Path,
        text: str, error: str, *, close_reason: str,
    ) -> bool:
        """Record one bounded rejection, then remove the unaccounted body last."""
        encoded = text.encode("utf-8")
        digest = hashlib.sha256(encoded).hexdigest()
        if not (
            record.get("reply_submission_digest") == digest
            and record.get("reply_submission_bytes") == len(encoded)
            and isinstance(record.get("reply_submission_rejected_at"), str)
            and isinstance(record.get("reply_submission_error"), str)
        ):
            record.update(
                reply_submission_digest=digest,
                reply_submission_bytes=len(encoded),
                reply_submission_rejected_at=_utc(),
                reply_submission_error=error[:2000],
            )
            record.setdefault("reply_closed_at", _utc())
            record["reply_close_reason"] = close_reason
            _write(path, record)
        try:
            submission_path.unlink()
        except FileNotFoundError:
            pass
        else:
            _fsync_dir(str(submission_path.parent))
        return False

    def _account_new_reply(self, byte_count: int) -> None:
        usage = self._reply_usage()
        usage["items"] += 1
        usage["bytes"] += byte_count
        usage["pending_items"] += 1
        usage["pending_bytes"] += byte_count

    def _account_finalized_reply(self, byte_count: int) -> None:
        usage = self._reply_usage()
        usage["pending_items"] -= 1
        usage["pending_bytes"] -= byte_count
        if usage["pending_items"] < 0 or usage["pending_bytes"] < 0:
            raise ValueError("reply usage accounting underflow")

    def _write_reply_summary(
        self, path: Path, record: dict[str, object], *, count: int, sent: int,
        total_bytes: int, pending_bytes: int, next_ordinal: int | None = None,
        offset: int | None = None,
    ) -> None:
        if not 0 <= sent <= count <= _MAX_REPLY_ITEMS:
            raise ValueError("reply summary counts are outside the supported range")
        if not 0 <= pending_bytes <= total_bytes <= _MAX_REPLY_ITEMS * 30_000:
            raise ValueError("reply summary bytes are outside the supported range")
        record.update(reply_storage=2, reply_item_count=count, reply_sent_count=sent,
                      reply_total_bytes=total_bytes, reply_pending_bytes=pending_bytes)
        if next_ordinal is not None:
            record["reply_next_ordinal"] = next_ordinal
        if offset is not None:
            record["reply_ordinal_offset"] = offset
        if next_ordinal is not None and next_ordinal > 999_999:
            record.setdefault("reply_closed_at", _utc())
            record.setdefault("reply_close_reason", "ordinal_exhausted")
        _write(path, record)

    def _preflight_legacy_outbox(
        self, path: Path, record: dict[str, object],
    ) -> tuple[_LegacyReplyPlan, list[dict[str, object]]]:
        """Validate one bounded monolith and return a body-free durable plan."""
        key = get_str(record, "key", "request")
        embedded = _embedded_reply_path(self.state, key)
        raw_items: list[dict[str, object]] = []
        encoded_bytes = 0
        try:
            document, encoded_bytes = read_json(embedded, LEGACY_REPLY)
        except AgentDeliveryError as exc:
            if isinstance(exc.__cause__, FileNotFoundError):
                document = None
            else:
                raise
        if document is not None:
            if "items" in document:
                if set(document) != {"text", "items"}:
                    raise ValueError(
                        "embedded reply outbox must contain exactly text and items")
                get_str(document, "text", "embedded reply")
                values = as_sequence(document["items"], "embedded reply items")
                if len(values) > _MAX_REQUEST_REPLY_ITEMS:
                    raise ValueError(
                        "embedded reply outbox exceeds the per-request item limit")
                raw_items = [dict(as_mapping(value, "embedded reply item"))
                             for value in values]
            else:
                if set(document) != {"text"}:
                    raise ValueError(
                        "embedded single reply must contain exactly text")
                item: dict[str, object] = {
                    "reply_key": "0", "request_id": get_str(record, "request_id", "request"),
                    "reply_source": "file", "text": get_str(document, "text", "embedded reply"),
                }
                if record.get("phase") == "replied":
                    item["reply_id"] = get_str(record, "reply_id", "request")
                    item["replied_at"] = (
                        record["replied_at"] if isinstance(record.get("replied_at"), str)
                        else get_str(record, "received_at", "request"))
                raw_items = [item]

        validated_items: list[dict[str, object]] = []
        sent = 0
        saw_pending = False
        ordinals: list[int] = []
        total_bytes = 0
        pending_bytes = 0
        digest = hashlib.sha256()
        for index, raw in enumerate(raw_items):
            item = dict(raw)
            if "reply_source" not in item:
                item["reply_source"] = (
                    "capture" if item.get("reply_ordinal") is not None else "legacy")
            validated = self._validate_reply_item(record, item, index)
            validated_items.append(validated)
            encoded = json.dumps(
                validated, sort_keys=True, separators=(",", ":"), ensure_ascii=True,
            ).encode("utf-8")
            digest.update(len(encoded).to_bytes(8, "big"))
            digest.update(encoded)
            body_bytes = len(get_str(validated, "text", "reply item").encode("utf-8"))
            total_bytes += body_bytes
            is_sent = validated.get("reply_id") is not None
            if is_sent and saw_pending:
                raise ValueError("embedded reply confirmations are not a contiguous prefix")
            if is_sent:
                sent += 1
            else:
                saw_pending = True
                pending_bytes += body_bytes
            ordinal = validated.get("reply_ordinal")
            if type(ordinal) is int:
                ordinals.append(ordinal)
        if total_bytes > _MAX_REQUEST_REPLY_BYTES:
            raise ValueError("embedded reply outbox exceeds the per-request byte limit")

        offset = 0
        next_ordinal: int | None = None
        if record.get("reply_protocol") == 3:
            unsequenced = [index for index, item in enumerate(validated_items)
                           if item.get("reply_ordinal") is None]
            if unsequenced not in ([], [0]):
                raise ValueError(
                    "embedded protocol-v3 unsequenced reply must be the first and only one")
            if unsequenced and validated_items[0].get("reply_source") != "file":
                raise ValueError(
                    "embedded protocol-v3 unsequenced reply must be a file recovery")
            if ordinals != list(range(1, len(ordinals) + 1)):
                raise ValueError("embedded protocol-v3 replies skip or duplicate an ordinal")
            offset = len(unsequenced)
            next_ordinal = len(ordinals) + 1
        last_reply_id = (get_str(validated_items[sent - 1], "reply_id", "reply item")
                         if sent else None)
        last_replied_at = (get_str(validated_items[sent - 1], "replied_at", "reply item")
                           if sent else None)
        plan = _LegacyReplyPlan(
            path=path, embedded_path=embedded, encoded_bytes=encoded_bytes,
            count=len(validated_items), sent=sent, total_bytes=total_bytes,
            pending_bytes=pending_bytes, offset=offset, next_ordinal=next_ordinal,
            last_reply_id=last_reply_id, last_replied_at=last_replied_at,
            digest=digest.hexdigest())
        return plan, validated_items

    def _migrate_reply_outboxes_locked(self) -> None:
        """Preflight every legacy outbox, then expand with the marker committed last."""
        self.validate_request_population()
        self.validate_aux_population()
        request_paths = sorted((self.state / "requests").glob("*.json"))
        known = {path.stem for path in request_paths}
        for embedded in (self.state / "replies").glob("*.json"):
            if embedded.stem not in known:
                raise ValueError("embedded reply outbox has no matching request record")

        plans: list[_LegacyReplyPlan] = []
        state_items = state_bytes = pending_items = pending_state_bytes = 0
        legacy_encoded_bytes = 0
        for path in request_paths:
            record = _read(path)
            key = get_str(record, "key", "request")
            if key != path.stem:
                raise ValueError("saved request key does not match its file")
            if record.get("reply_storage") == 2:
                count, sent, total_bytes, pending_bytes = self._reply_summary(record)
            else:
                plan, decoded_items = self._preflight_legacy_outbox(path, record)
                plans.append(plan)
                legacy_encoded_bytes += plan.encoded_bytes
                if legacy_encoded_bytes > _MAX_LEGACY_REPLY_BYTES:
                    raise ValueError(
                        "embedded reply outboxes exceed the state-wide encoded byte limit "
                        f"{_MAX_LEGACY_REPLY_BYTES}")
                count, sent = plan.count, plan.sent
                total_bytes, pending_bytes = plan.total_bytes, plan.pending_bytes
                # The state-wide plan retains only scalar summaries and a digest.
                # Release this request's decoded bodies before reading the next.
                del decoded_items
            state_items += count
            state_bytes += total_bytes
            pending_items += count - sent
            pending_state_bytes += pending_bytes
            if count > _MAX_REQUEST_REPLY_ITEMS or total_bytes > _MAX_REQUEST_REPLY_BYTES:
                raise ValueError("reply recovery exceeds the per-request storage limits")
            del record

        # Refuse before creating even one sharded file. The legacy monolith stays
        # intact for explicit inspection/recovery on a suitably bounded state.
        if (
            state_items > _MAX_STATE_REPLY_ITEMS
            or state_bytes > _MAX_STATE_REPLY_BYTES
            or pending_items > _MAX_PENDING_REPLY_ITEMS
            or pending_state_bytes > _MAX_PENDING_REPLY_BYTES
        ):
            raise ValueError("reply recovery exceeds the state-wide storage limits")

        # Re-read one request at a time. The semantic digest makes a concurrent
        # owner edit between passes fail closed instead of expanding different
        # content from the population that passed the global cap calculation.
        for expected in plans:
            record = _read(expected.path)
            actual, items = self._preflight_legacy_outbox(expected.path, record)
            if actual != expected:
                raise ValueError("embedded reply outbox changed during migration preflight")
            for index, validated in enumerate(items):
                is_sent = index < expected.sent
                durable, _ = self._create_reply_item(record, index, validated, sent=is_sent)
                if durable != validated:
                    raise ValueError("expanded reply artifact conflicts with embedded outbox")
                if is_sent:
                    self._ensure_reply_receipt(record, durable)
            if expected.last_reply_id is not None:
                record["reply_last_id"] = expected.last_reply_id
                record["reply_last_at"] = expected.last_replied_at
            self._write_reply_summary(
                expected.path, record, count=expected.count, sent=expected.sent,
                total_bytes=expected.total_bytes, pending_bytes=expected.pending_bytes,
                next_ordinal=expected.next_ordinal, offset=expected.offset)
            current = _read(expected.path)
            if self._reply_summary(current) != (
                expected.count, expected.sent, expected.total_bytes, expected.pending_bytes,
            ):
                raise ValueError("migrated reply summary does not match its durable plan")
            if (current.get("reply_ordinal_offset") != expected.offset
                    or (expected.next_ordinal is not None
                        and current.get("reply_next_ordinal") != expected.next_ordinal)):
                raise ValueError("migrated reply protocol summary does not match its durable plan")
            self._unlink_legacy_monolith(expected.embedded_path)
            del items
            del record

        usage = {"items": state_items, "bytes": state_bytes,
                 "pending_items": pending_items, "pending_bytes": pending_state_bytes}
        self._usage = usage

    @staticmethod
    def _unlink_legacy_monolith(embedded: Path) -> None:
        """Durably remove one superseded monolith; absence is an idempotent success."""
        try:
            embedded.unlink()
        except FileNotFoundError:
            return
        _fsync_dir(str(embedded.parent))

    def _cleanup_legacy_outboxes_locked(self, request_paths: Sequence[Path]) -> None:
        """Delete migrated monoliths only after current sharded state verifies."""
        for path in request_paths:
            key = path.stem
            embedded = _embedded_reply_path(self.state, key)
            try:
                embedded.lstat()
            except FileNotFoundError:
                continue
            record = _read(path)
            if get_str(record, "key", "request") != key:
                raise ValueError("saved request key does not match its file")
            pending = self._recover_reply_state(path, record)
            count, sent, _, _ = self._reply_summary(record)
            if len(pending) != count - sent:
                raise ValueError(
                    "saved reply count summary disagrees with durable pending journals")
            self._unlink_legacy_monolith(embedded)

    def migrate_reply_outboxes(self) -> None:
        """Expand saved reply monoliths into bounded shards and verify recovery."""
        descriptor = _open_private_lock(str(self.state / ".bridge.lock"), "chat bridge lock")
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX)
            self._migrate_reply_outboxes_locked()
            paths = sorted((self.state / "requests").glob("*.json"))
            if not self._storage_validated:
                for path in paths:
                    record = _read(path)
                    pending = self._recover_reply_state(path, record)
                    count, sent, _, _ = self._reply_summary(record)
                    if len(pending) != count - sent:
                        raise ValueError(
                            "saved reply count summary disagrees with durable pending journals")
                    del pending
                    del record
                self._storage_validated = True
            self._cleanup_legacy_outboxes_locked(paths)
        finally:
            os.close(descriptor)

    def _archive_confirmed(
        self, path: Path, record: dict[str, object], item: dict[str, object],
    ) -> None:
        """Finalize one confirmed pending journal; pending is removed last."""
        self._reply_usage()
        count, sent, total_bytes, pending_bytes = self._reply_summary(record)
        index = int(get_str(item, "reply_key", "reply item"))
        if index != sent or index >= count:
            raise ValueError("reply confirmation is outside strict pending order")
        body_bytes = len(get_str(item, "text", "reply item").encode("utf-8"))
        durable, _ = self._create_reply_item(record, index, item, sent=True)
        if durable != item:
            raise ValueError("immutable reply history conflicts with the pending journal")
        self._ensure_reply_receipt(record, durable)
        new_sent = sent + 1
        record.update(
            phase="reply_pending" if new_sent < count else "replied",
            reply_id=get_str(item, "reply_id", "reply item"),
            replied_at=get_str(item, "replied_at", "reply item"),
            reply_last_id=get_str(item, "reply_id", "reply item"),
            reply_last_at=get_str(item, "replied_at", "reply item"),
        )
        record.pop("reply_error", None)
        saved_next = record.get("reply_next_ordinal")
        saved_offset = record.get("reply_ordinal_offset")
        self._write_reply_summary(
            path, record, count=count, sent=new_sent, total_bytes=total_bytes,
            pending_bytes=pending_bytes - body_bytes,
            next_ordinal=saved_next if type(saved_next) is int else None,
            offset=saved_offset if type(saved_offset) is int else None)
        self._account_finalized_reply(body_bytes)
        pending_path = _reply_item_path(
            self.state, get_str(record, "key", "request"), index, sent=False)
        try:
            pending_path.unlink()
        except FileNotFoundError:
            pass
        else:
            _fsync_dir(str(pending_path.parent))

    def _recover_reply_state(self, path: Path, record: dict[str, object]) -> list[dict[str, object]]:
        """Recover exact pending journals without enumerating immutable history."""
        self._reply_usage()
        count, sent, total_bytes, pending_bytes = self._reply_summary(record)
        key = get_str(record, "key", "request")
        changed = False

        # Adopt at most one append whose artifact fsync preceded its summary fsync.
        next_pending = _reply_item_path(self.state, key, count, sent=False)
        if count < _MAX_REPLY_ITEMS and next_pending.exists():
            adopted = self._read_reply_item(record, count, sent=False)
            if adopted.get("reply_shift_from") == 0:
                submission_path = self.state / "submissions" / f"{key}.json"
                if not (count == 1 and sent == 0 and record.get("reply_protocol") == 3
                        and record.get("reply_ordinal_offset", 0) == 0
                        and record.get("reply_next_ordinal") == 2
                        and submission_path.exists()):
                    raise ValueError("incomplete reply shift has no recoverable transaction state")
                submission = _read(submission_path)
                if submission.get("request") != key:
                    raise ValueError("reply shift submission belongs to another request")
                manual_text = get_str(submission, "text", "reply shift submission")
                first = self._read_reply_item(record, 0, sent=False)
                shifted_text = get_str(adopted, "text", "shifted reply")
                if not (adopted.get("reply_source") == "capture"
                        and adopted.get("reply_ordinal") == 1):
                    raise ValueError("reply shift journal does not preserve ordinal one")
                if first.get("reply_source") == "capture":
                    if (first.get("reply_ordinal") != 1
                            or get_str(first, "text", "reply item") != shifted_text):
                        raise ValueError("reply shift journal conflicts with captured ordinal one")
                    manual: dict[str, object] = {
                        "reply_key": "0", "request_id": self._reply_identity(record, 0),
                        "reply_source": "file", "text": manual_text}
                    self._write_pending_reply(record, manual)
                elif not (first.get("reply_source") == "file"
                          and first.get("reply_ordinal") is None
                          and get_str(first, "text", "reply item") == manual_text):
                    raise ValueError("reply shift journal conflicts with the file submission")
                manual_bytes = len(manual_text.encode("utf-8"))
                capture_bytes = len(shifted_text.encode("utf-8"))
                if total_bytes != capture_bytes or pending_bytes != capture_bytes:
                    raise ValueError("reply shift summary disagrees with captured ordinal one")
                count = 2
                total_bytes = manual_bytes + capture_bytes
                pending_bytes = total_bytes
                self._write_reply_summary(
                    path, record, count=count, sent=0, total_bytes=total_bytes,
                    pending_bytes=pending_bytes, next_ordinal=2, offset=1)
                self._account_new_reply(manual_bytes)
                changed = False
            else:
                body_bytes = len(get_str(adopted, "text", "reply item").encode("utf-8"))
                count += 1
                total_bytes += body_bytes
                pending_bytes += body_bytes
                record.update(reply_item_count=count, reply_total_bytes=total_bytes,
                              reply_pending_bytes=pending_bytes)
                self._account_new_reply(body_bytes)
                changed = True
            if count < _MAX_REPLY_ITEMS and _reply_item_path(
                self.state, key, count, sent=False
            ).exists():
                raise ValueError("reply outbox has more than one uncommitted append")

        # Finish a provider confirmation that reached either journal namespace.
        while sent < count:
            pending_path = _reply_item_path(self.state, key, sent, sent=False)
            history_path = _reply_item_path(self.state, key, sent, sent=True)
            pending_item = (
                self._read_reply_item(record, sent, sent=False) if pending_path.exists() else None)
            history_item = (
                self._read_reply_item(record, sent, sent=True) if history_path.exists() else None)
            confirmed = history_item
            if pending_item is not None and pending_item.get("reply_id") is not None:
                if history_item is not None and history_item != pending_item:
                    raise ValueError("pending confirmation conflicts with immutable reply history")
                confirmed = pending_item
            if confirmed is None:
                if pending_item is None:
                    raise ValueError("pending reply summary points to a missing artifact")
                break
            if pending_item is None:
                body_bytes = len(get_str(confirmed, "text", "reply item").encode("utf-8"))
                self._ensure_reply_receipt(record, confirmed)
                sent += 1
                pending_bytes -= body_bytes
                record.update(reply_sent_count=sent, reply_pending_bytes=pending_bytes,
                              reply_last_id=confirmed["reply_id"],
                              reply_last_at=confirmed["replied_at"])
                self._account_finalized_reply(body_bytes)
                changed = True
            else:
                if changed:
                    _write(path, record)
                    changed = False
                self._archive_confirmed(path, record, confirmed)
                count, sent, total_bytes, pending_bytes = self._reply_summary(record)

        # A crash after summary fsync but before journal unlink leaves exactly S-1.
        if sent:
            stale = _reply_item_path(self.state, key, sent - 1, sent=False)
            if stale.exists():
                pending_item = self._read_reply_item(record, sent - 1, sent=False)
                historical = self._read_reply_item(record, sent - 1, sent=True)
                if pending_item != historical:
                    raise ValueError("stale pending journal conflicts with immutable history")
                stale.unlink()
                _fsync_dir(str(stale.parent))

        pending = [self._read_reply_item(record, index, sent=False)
                   for index in range(sent, count)]
        for position, item in enumerate(pending):
            if item.get("reply_shift_from") is None:
                continue
            if not (count == 2 and sent == 0 and position == 1
                    and record.get("reply_ordinal_offset") == 1
                    and record.get("reply_next_ordinal") == 2):
                raise ValueError("counted reply shift has inconsistent summary metadata")
            cleaned = dict(item)
            cleaned.pop("reply_shift_from")
            self._write_pending_reply(record, cleaned)
            pending[position] = cleaned
        actual_pending_bytes = sum(
            len(get_str(item, "text", "reply item").encode("utf-8")) for item in pending)
        if actual_pending_bytes != pending_bytes:
            raise ValueError("saved pending reply bytes disagree with durable journals")
        if any(item.get("reply_id") is not None for item in pending):
            raise ValueError("only the first pending journal may contain a recoverable confirmation")

        if record.get("reply_protocol") == 3:
            offset = record.get("reply_ordinal_offset", 0)
            if type(offset) is not int or offset not in (0, 1):
                raise ValueError("saved reply ordinal offset must be zero or one")
            if (offset == 1 and count == 1 and sent == 0 and pending
                    and pending[0].get("reply_source") == "file"
                    and pending[0].get("reply_ordinal") == 1):
                # Binding an identical file submission writes the item before
                # rebasing the constant summary. Complete that two-write journal.
                offset = 0
                record["reply_ordinal_offset"] = 0
                changed = True
            durable_next = count - offset + 1
            saved_next = record.get("reply_next_ordinal", 1)
            if type(saved_next) is not int or not 1 <= saved_next <= durable_next:
                raise ValueError("saved protocol-v3 next ordinal disagrees with durable artifacts")
            if saved_next != durable_next:
                record["reply_next_ordinal"] = durable_next
                changed = True
            if durable_next > 999_999 and record.get("reply_closed_at") is None:
                record.update(reply_closed_at=_utc(), reply_close_reason="ordinal_exhausted")
                changed = True
        if record.get("reply_item_count") != count or record.get("reply_sent_count") != sent:
            record.update(reply_item_count=count, reply_sent_count=sent,
                          reply_total_bytes=total_bytes, reply_pending_bytes=pending_bytes)
            changed = True
        if count and sent == count and record.get("phase") in (
            "awaiting_reply", "reply_pending", "delivery_uncertain"
        ):
            record.update(phase="replied", reply_id=record.get("reply_last_id"),
                          replied_at=record.get("reply_last_at"))
            record.pop("reply_error", None)
            changed = True
        if changed:
            _write(path, record)
        return pending

    def _reply_items(self, record: dict[str, object]) -> list[dict[str, object]]:
        """Read only the bounded mutable pending suffix, never sent history."""
        count, sent, _, _ = self._reply_summary(record)
        return [self._read_reply_item(record, index, sent=False)
                for index in range(sent, count)]

    def _all_reply_items(self, record: dict[str, object]) -> list[dict[str, object]]:
        """Explicit history/audit read; cost is linear in all retained reply bodies."""
        count, sent, _, _ = self._reply_summary(record)
        return [self._read_reply_item(record, index, sent=index < sent)
                for index in range(count)]

    def _next_reply(
        self, source: Sequence[dict[str, object]] | dict[str, object],
    ) -> dict[str, object] | None:
        # Keep the narrow record form for diagnostic callers; the runtime passes
        # its cached pending list and performs no filesystem read here.
        items = self._reply_items(source) if isinstance(source, dict) else source
        return items[0] if items else None

    def _outbound_text(self, text: str) -> str:
        prefix = f"[{self.config.agent_label}]"
        return text if text.startswith(prefix) else f"{prefix} {text}"

    def _pending_outbound_texts(self, record: dict[str, object]) -> list[str]:
        return [self._outbound_text(get_str(item, "text", "reply item"))
                for item in self._reply_items(record)]

    def _adopt_submission(
        self, path: Path, record: dict[str, object], items: list[dict[str, object]],
    ) -> bool:
        key = get_str(record, "key", "request")
        submission_path = self.state / "submissions" / f"{key}.json"
        if not submission_path.exists():
            return False
        with atomic_write_recovery(_chat_atomic_policy(self.state)):
            pass
        submission = _read(submission_path)
        if submission.get("request") != key:
            raise ValueError("reply submission belongs to a different request")
        text = get_str(submission, "text", "reply submission")
        if not text.strip() or len(text.encode("utf-8")) > 30_000:
            raise ValueError("reply submission must contain 1-30000 UTF-8 bytes")
        submission_digest = hashlib.sha256(text.encode("utf-8")).hexdigest()
        if (record.get("reply_submission_digest") == submission_digest
                and record.get("reply_submission_bytes") == len(text.encode("utf-8"))
                and isinstance(record.get("reply_submission_rejected_at"), str)):
            # The request write is the rejection transaction's commit marker;
            # finish an unlink lost to a crash without retrying the body.
            try:
                submission_path.unlink()
            except FileNotFoundError:
                pass
            else:
                _fsync_dir(str(submission_path.parent))
            return False
        count, sent, total_bytes, pending_bytes = self._reply_summary(record)
        if count:
            first = self._read_reply_item(record, 0, sent=sent > 0)
            first_text = get_str(first, "text", "reply item")
            if first_text != text:
                # A submission can race the create-only first capture. The
                # owner deterministically gives the deliberate file recovery
                # index zero and shifts that just-created ordinal to index one.
                if not (count == 1 and sent == 0 and first.get("reply_source") == "capture"
                        and first.get("reply_ordinal") == 1
                        and record.get("reply_protocol") == 3):
                    return self._reject_submission(
                        path, record, submission_path, text,
                        "file submission conflicts with durable reply history",
                        close_reason="submission_conflict")
                body_bytes = len(text.encode("utf-8"))
                reason = self._capacity_error(record, add_items=1, add_bytes=body_bytes)
                if reason is not None:
                    return self._reject_submission(
                        path, record, submission_path, text, reason,
                        close_reason="storage_limit")
                manual: dict[str, object] = {
                    "reply_key": "0", "request_id": self._reply_identity(record, 0),
                    "reply_source": "file", "text": text}
                shifted = dict(
                    first, reply_key="1", request_id=self._reply_identity(record, 1),
                    reply_shift_from=0)
                durable, created = self._create_reply_item(record, 1, shifted)
                if not created and durable != shifted:
                    raise ValueError("raced captured reply conflicts with its shifted artifact")
                self._write_pending_reply(record, manual)
                self._write_reply_summary(
                    path, record, count=2, sent=0, total_bytes=total_bytes + body_bytes,
                    pending_bytes=pending_bytes + body_bytes, next_ordinal=2, offset=1)
                self._account_new_reply(body_bytes)
                durable = dict(durable)
                durable.pop("reply_shift_from", None)
                self._write_pending_reply(record, durable)
                items[:] = [manual, durable]
        else:
            reason = self._capacity_error(record, add_items=1, add_bytes=len(text.encode("utf-8")))
            if reason is not None:
                return self._reject_submission(
                    path, record, submission_path, text, reason,
                    close_reason="storage_limit")
            item: dict[str, object] = {
                "reply_key": "0", "request_id": self._reply_identity(record, 0),
                "reply_source": "file", "text": text}
            durable, created = self._create_reply_item(record, 0, item)
            if not created and durable != item:
                raise ValueError("existing reply artifact conflicts with file submission")
            items.append(durable)
            body_bytes = len(text.encode("utf-8"))
            self._write_reply_summary(
                path, record, count=1, sent=sent, total_bytes=body_bytes,
                pending_bytes=body_bytes,
                next_ordinal=(1 if record.get("reply_protocol") == 3 else None),
                offset=(1 if record.get("reply_protocol") == 3 else 0))
            self._account_new_reply(body_bytes)
        submission_path.unlink()
        _fsync_dir(str(submission_path.parent))
        return True

    def _append_pending_item(
        self, path: Path, record: dict[str, object], items: list[dict[str, object]],
        *, text: str, source: str, ordinal: int | None, next_ordinal: int | None,
        offset: int,
    ) -> dict[str, object]:
        count, sent, total_bytes, pending_bytes = self._reply_summary(record)
        body_bytes = len(text.encode("utf-8"))
        reason = self._capacity_error(record, add_items=1, add_bytes=body_bytes)
        if reason is not None:
            self._close_for_storage_limit(path, record, reason)
            raise ValueError(reason)
        item: dict[str, object] = {
            "reply_key": str(count), "request_id": self._reply_identity(record, count),
            "reply_source": source, "text": text,
        }
        if ordinal is not None:
            item["reply_ordinal"] = ordinal
        durable, created = self._create_reply_item(record, count, item)
        if not created and durable != item:
            raise ValueError("existing reply artifact conflicts with captured reply")
        items.append(durable)
        self._write_reply_summary(
            path, record, count=count + 1, sent=sent, total_bytes=total_bytes + body_bytes,
            pending_bytes=pending_bytes + body_bytes, next_ordinal=next_ordinal, offset=offset)
        self._account_new_reply(body_bytes)
        return durable

    def _append_replies(
        self, record: dict[str, object], answers: list[str],
        items: list[dict[str, object]] | None = None,
    ) -> bool:
        """Commit legacy occurrences; this explicit recovery path may read history."""
        key = get_str(record, "key", "request")
        path = self.state / "requests" / f"{key}.json"
        history = self._all_reply_items(record)
        pending = self._reply_items(record) if items is None else items
        previous = Counter(hashlib.sha256(get_str(item, "text", "reply item").encode()).hexdigest()
                           for item in history)
        observed: Counter[str] = Counter()
        additions: list[str] = []
        for answer in answers:
            digest = hashlib.sha256(answer.encode()).hexdigest()
            observed[digest] += 1
            if observed[digest] > previous[digest]:
                additions.append(answer)
        if additions:
            total_add = sum(len(answer.encode("utf-8")) for answer in additions)
            reason = self._capacity_error(
                record, add_items=len(additions), add_bytes=total_add)
            if reason is not None:
                self._close_for_storage_limit(path, record, reason)
                raise ValueError(reason)
        for answer in additions:
            self._append_pending_item(
                path, record, pending, text=answer, source="legacy", ordinal=None,
                next_ordinal=None, offset=0)
        return bool(additions)

    def _append_sequenced_replies(
        self, record: dict[str, object], answers: list[tuple[int, str]],
        items: list[dict[str, object]] | None = None,
    ) -> bool:
        """Commit consecutive protocol-v3 occurrences before any provider send."""
        key = get_str(record, "key", "request")
        path = self.state / "requests" / f"{key}.json"
        pending = self._reply_items(record) if items is None else items
        count, sent, _, _ = self._reply_summary(record)
        offset = record.get("reply_ordinal_offset", 0)
        if type(offset) is not int or offset not in (0, 1):
            raise ValueError("saved reply ordinal offset must be zero or one")
        next_ordinal = count - offset + 1
        if record.get("reply_next_ordinal", 1) != next_ordinal:
            raise ValueError("saved protocol-v3 next ordinal disagrees with the durable outbox")

        seen: set[int] = set()
        planned: list[tuple[int, str, str]] = []
        simulated_next = next_ordinal
        simulated_offset = offset
        for ordinal, answer in answers:
            if ordinal in seen:
                raise ValueError(f"reply ordinal {ordinal} appears more than once in retained output")
            seen.add(ordinal)
            if ordinal < simulated_next:
                index = ordinal - 1 + simulated_offset
                previous = self._read_reply_item(record, index, sent=index < sent)
                if (previous.get("reply_ordinal") != ordinal
                        or get_str(previous, "text", "reply item") != answer):
                    raise ValueError(f"reply ordinal {ordinal} was reused with different text")
                continue
            if ordinal > simulated_next:
                raise ValueError(
                    f"reply ordinal {ordinal} skips required ordinal {simulated_next}")
            if (simulated_offset == 1 and ordinal == 1 and count == 1
                    and pending and pending[0].get("reply_source") == "file"
                    and pending[0].get("reply_ordinal") is None
                    and get_str(pending[0], "text", "reply item") == answer):
                planned.append((ordinal, answer, "bind"))
                simulated_offset = 0
            else:
                planned.append((ordinal, answer, "append"))
            simulated_next += 1

        additions = [answer for _, answer, operation in planned if operation == "append"]
        if additions:
            reason = self._capacity_error(
                record, add_items=len(additions),
                add_bytes=sum(len(answer.encode("utf-8")) for answer in additions))
            if reason is not None:
                self._close_for_storage_limit(path, record, reason)
                raise ValueError(reason)

        added = False
        for ordinal, answer, operation in planned:
            if operation == "bind":
                bound = pending[0]
                bound["reply_ordinal"] = ordinal
                self._write_pending_reply(record, bound)
                offset = 0
                next_ordinal += 1
                count, sent, total_bytes, pending_bytes = self._reply_summary(record)
                self._write_reply_summary(
                    path, record, count=count, sent=sent, total_bytes=total_bytes,
                    pending_bytes=pending_bytes, next_ordinal=next_ordinal, offset=offset)
            else:
                next_ordinal += 1
                self._append_pending_item(
                    path, record, pending, text=answer, source="capture", ordinal=ordinal,
                    next_ordinal=next_ordinal, offset=offset)
            added = True
        return added

    def _reply_started(self, path: Path, record: dict[str, object], reply: dict[str, object]) -> None:
        if record["phase"] == "delivery_uncertain":
            record["delivery_confirmed_by"] = "reply_artifact"
        record["phase"] = "reply_pending"
        _write(path, record)

    def _reply_complete(
        self, path: Path, record: dict[str, object], reply: dict[str, object],
        identifier: str | None, finished_at: str, error: str | None,
    ) -> None:
        if error is not None:
            record.update(phase="reply_pending", reply_error=error[:2000])
            _write(path, record)
            return
        if identifier is None:
            raise ValueError("successful chat reply has no provider message id")
        index = int(get_str(reply, "reply_key", "reply item"))
        pending = self._read_reply_item(record, index, sent=False)
        if pending != reply:
            raise ValueError("completed reply does not match its durable pending journal")
        pending.update(reply_id=identifier, replied_at=finished_at)
        self._write_pending_reply(record, pending)
        self._archive_confirmed(path, record, pending)

    def _feedback_delivery(self) -> tuple[bool, list[tuple[str, str]]]:
        """Return unsubmitted feedback and whether its durable harness queue still needs draining."""
        pending = False
        prompts: list[tuple[str, str]] = []
        for path in sorted((self.state / "feedback").glob("*.json")):
            feedback = _read(path)
            identifier = get_str(feedback, "queue_id", "reply feedback")
            phases = [phase for phase in ("inbox", "inflight", "processed", "failed")
                      if (self.state / "queue" / phase / f"{identifier}.json").exists()]
            if not phases:
                prompts.append((identifier, get_str(feedback, "text", "reply feedback")))
                pending = True
            elif any(phase in ("inbox", "inflight") for phase in phases):
                pending = True
        return pending, prompts

    def _feedback_pending_records(self) -> dict[str, tuple[str, bool]]:
        """Recovery-only scan of feedback still absent from or active in the queue."""
        pending: dict[str, tuple[str, bool]] = {}
        for path in (self.state / "feedback").glob("*.json"):
            feedback = _read(path)
            identifier = get_str(feedback, "queue_id", "reply feedback")
            phases = [phase for phase in ("inbox", "inflight", "processed", "failed")
                      if (self.state / "queue" / phase / f"{identifier}.json").exists()]
            if not phases:
                pending[identifier] = (get_str(feedback, "text", "reply feedback"), False)
            elif any(phase in ("inbox", "inflight") for phase in phases):
                pending[identifier] = (get_str(feedback, "text", "reply feedback"), True)
        return pending

    def _prime_prompt_cache(self) -> None:
        """Rebuild the owner-only prompt cache during startup/recovery."""
        # Merge monotonically: a concurrent drain may move an artifact into a
        # phase already scanned. Queue identities and text are immutable, so
        # retaining the prior value is both race-safe and conservative.
        prompts = dict(self._prompt_text_cache or {})
        for phase in ("inbox", "inflight", "processed", "failed"):
            for path in (self.state / "queue" / phase).glob("*.json"):
                try:
                    prompt = get_str(_read(path), "text", "queued prompt")
                except AgentDeliveryError as exc:
                    # Queue delivery atomically moves artifacts between phases.
                    if isinstance(exc.__cause__, FileNotFoundError):
                        continue
                    raise
                identifier = path.stem
                entry = self._prompt_cache_entry(prompt)
                previous = prompts.get(identifier)
                if previous is not None and previous != entry:
                    raise ValueError("queue phases contain conflicting prompt text")
                prompts[identifier] = entry
        self._prompt_text_cache = prompts

    @staticmethod
    def _prompt_cache_entry(prompt: str) -> tuple[str, str]:
        lines = [line for line in prompt.splitlines() if line]
        if not lines:
            raise ValueError("queued prompt text must not be empty")
        source = next((line for line in lines if line.startswith("Source: ")), None)
        anchor = source if source is not None else lines[0]
        return anchor, hashlib.sha256(prompt.encode("utf-8")).hexdigest()

    def _remember_prompt(self, identifier: str, prompt: str) -> bool:
        """Register a prompt before a continuous owner schedules its enqueue."""
        if self._prompt_text_cache is None:
            return False
        entry = self._prompt_cache_entry(prompt)
        previous = self._prompt_text_cache.get(identifier)
        if previous is not None and previous != entry:
            raise ValueError("queue identity was reused with different prompt text")
        self._prompt_text_cache[identifier] = entry
        return previous is None

    def _forget_tentative_prompt(self, identifier: str) -> None:
        """Drop a failed pre-registration after its enqueue worker has stopped."""
        if self._prompt_text_cache is None:
            return
        if not any((self.state / "queue" / phase / f"{identifier}.json").exists()
                   for phase in ("processed", "inflight", "inbox", "failed")):
            self._prompt_text_cache.pop(identifier, None)

    def _prompt_echo_views(self, text: str) -> tuple[str, str]:
        """Return full and history-trimmed text after one prompt population pass.

        Matching an exact saved prompt is intentionally conservative: arbitrary
        user text may contain reply-tag examples.  Build both views from the
        same matches. Continuous owners use their recovery-built anchor index,
        so a real output event performs no queue directory scan and reads only
        exact artifacts whose small anchors are visible.
        """
        matches: list[tuple[int, str]] = []
        if self._prompt_text_cache is None:
            for phase in ("processed", "inflight", "inbox", "failed"):
                for path in (self.state / "queue" / phase).glob("*.json"):
                    prompt = get_str(_read(path), "text", "queued prompt")
                    start = text.find(prompt)
                    if start >= 0:
                        matches.append((start, prompt))
        else:
            visible_lines = set(text.splitlines())
            for identifier, (anchor, digest) in self._prompt_text_cache.items():
                if anchor not in visible_lines:
                    continue
                # The immutable queue artifact, rather than a prompt rebuilt
                # from mutable request metadata, is authoritative. Probe exact
                # paths in delivery order to tolerate an atomic phase move.
                prompt = ""
                for phase in ("inbox", "inflight", "processed", "failed"):
                    candidate = self.state / "queue" / phase / f"{identifier}.json"
                    try:
                        prompt = get_str(_read(candidate), "text", "queued prompt")
                        break
                    except AgentDeliveryError as exc:
                        if isinstance(exc.__cause__, FileNotFoundError):
                            continue
                        raise
                if not prompt:
                    raise ValueError("cached prompt has no durable queue artifact")
                if hashlib.sha256(prompt.encode("utf-8")).hexdigest() != digest:
                    raise ValueError("cached prompt digest disagrees with durable state")
                start = text.find(prompt)
                if start >= 0:
                    matches.append((start, prompt))
        # A retained envelope proves earlier scrollback predates the visible request set.
        recent = text[min(start for start, _ in matches):] if matches else text
        for _, prompt in matches:
            text = text.replace(prompt, "")
            recent = recent.replace(prompt, "")
        return text, recent

    def _without_prompt_echoes(self, text: str, *, trim_history: bool = False) -> str:
        """Remove full saved user envelopes, including source text with copied fence examples."""
        full, recent = self._prompt_echo_views(text)
        return recent if trim_history else full

    def _unknown_reply_feedback(
        self, text: str, available: dict[str, str],
        records: Sequence[dict[str, object]] | None = None,
        marker_ids: Sequence[str] | None = None,
        *, inspect_legacy_history: bool = True,
        created_feedback: list[tuple[str, str]] | None = None,
    ) -> None:
        known = set(available)
        visible_order = list(marker_ids) if marker_ids is not None else reply_marker_ids(text)
        visible = set(visible_order)
        sequenced_visible: dict[str, set[str]] = {}
        for identifier in visible_order:
            match = re.fullmatch(r"([A-Za-z0-9_-]{22})_[1-9][0-9]{0,5}", identifier)
            if match is not None:
                nonce = match.group(1)
                sequenced_visible.setdefault(nonce, set()).add(identifier)
                if nonce in available:
                    known.add(identifier)
        # Already completed legacy fences in retained history are not new routing mistakes.
        loaded = records
        if loaded is None:
            loaded = [_read(path) for path in (self.state / "requests").glob("*.json")]
        legacy = [record for record in loaded
                  if (record.get("phase") == "replied"
                      and (record.get("reply_protocol") not in (2, 3)
                           or (record.get("reply_protocol") == 2
                               and record.get("reply_closed_at") is not None))
                      and isinstance(record.get("reply_nonce"), str))]
        visible_legacy = {str(record["reply_nonce"]) for record in legacy
                          if str(record["reply_nonce"]) in visible}
        parsed_legacy: dict[str, list[str]] | None = {}
        if visible_legacy:
            try:
                parsed_legacy = extract_replies(text, visible_legacy)
            except ValueError:
                # Malformed legacy blocks retain per-request isolation at the
                # documented adversarial O(R*L) cost.
                parsed_legacy = None
        for record in legacy:
            nonce = str(record["reply_nonce"])
            if nonce not in visible:
                known.add(nonce)
                continue
            if not inspect_legacy_history:
                # Continuous protocol-v3 capture never traverses lifetime
                # protocol-v2 history. Explicit tick/deep recovery retains the
                # more precise old-ID diagnostic below.
                known.add(nonce)
                continue
            # Protocol-v2 is refused by continuous run. Its explicit recovery
            # diagnostic may therefore pay the documented full-history cost.
            historical = {get_str(item, "text", "reply item")
                          for item in self._all_reply_items(record)}
            if parsed_legacy is None:
                try:
                    bodies = extract_replies(text, (nonce,)).get(nonce, [])
                except ValueError:
                    bodies = []
            else:
                bodies = parsed_legacy.get(nonce, [])
            # Suppress old delivered text (including clipped history), but diagnose an
            # actual new response directed to a closed legacy request.
            if not historical or not bodies or all(body in historical for body in bodies):
                known.add(nonce)
        closed_v3 = [record for record in loaded
                     if (record.get("phase") == "replied"
                         and record.get("reply_protocol") == 3
                         and record.get("reply_closed_at") is not None
                         and isinstance(record.get("reply_nonce"), str))]
        visible_v3 = {str(record["reply_nonce"]) for record in closed_v3
                      if str(record["reply_nonce"]) in sequenced_visible}
        parsed_v3: dict[str, list[tuple[int, str]]] | None = {}
        if visible_v3:
            try:
                parsed_v3 = extract_sequenced_replies(
                    text, visible_v3, marker_ids=visible_order)
            except ValueError:
                parsed_v3 = None
        for record in closed_v3:
            nonce = str(record["reply_nonce"])
            identifiers = sequenced_visible.get(nonce, set())
            if not identifiers:
                continue
            if parsed_v3 is None:
                try:
                    bodies_v3 = extract_sequenced_replies(
                        text, (nonce,), marker_ids=visible_order).get(nonce, [])
                except ValueError:
                    bodies_v3 = []
            else:
                bodies_v3 = parsed_v3.get(nonce, [])
            count, sent, _, _ = self._reply_summary(record)
            offset = record.get("reply_ordinal_offset", 0)
            if type(offset) is not int or offset not in (0, 1):
                raise ValueError("saved reply ordinal offset must be zero or one")
            matched = True
            for ordinal, body in bodies_v3:
                index = ordinal - 1 + offset
                if not 0 <= index < count:
                    matched = False
                    break
                historical_item = self._read_reply_item(record, index, sent=index < sent)
                if historical_item.get("reply_ordinal") != ordinal or get_str(
                    historical_item, "text", "reply item"
                ) != body:
                    matched = False
                    break
            if not bodies_v3 or matched:
                known.update(identifiers)
        invalid = [identifier for identifier in visible_order if identifier not in known]
        if not invalid:
            return
        by_nonce = {str(record.get("reply_nonce")): record for record in loaded
                    if isinstance(record.get("reply_nonce"), str)}
        destinations: list[tuple[str, str]] = []
        for nonce, key in available.items():
            destination_record = by_nonce.get(nonce)
            identifier = nonce
            if destination_record is not None and destination_record.get("reply_protocol") == 3:
                selected_ordinal = destination_record.get("reply_next_ordinal")
                if type(selected_ordinal) is not int:
                    raise ValueError("saved chat reply ordinal must be an integer")
                identifier = sequenced_reply_id(nonce, selected_ordinal)
            destinations.append((identifier, key))
        destinations.sort()
        listing = "\n".join(f"- {identifier} (request {key[:12]})"
                            for identifier, key in destinations)
        if not listing:
            listing = "(none)"
        destination_ids = [identifier for identifier, _ in destinations]
        usage = self._aux_usage or self.validate_aux_population()
        planned: list[tuple[Path, dict[str, object], str, str, int]] = []
        planned_paths: set[Path] = set()
        planned_records = usage["feedback_records"]
        planned_bytes = usage["feedback_bytes"]
        for nonce in invalid:
            identity = hashlib.sha256(json.dumps([nonce, destination_ids], ensure_ascii=True).encode()).hexdigest()
            path = self.state / "feedback" / f"{identity}.json"
            if path.exists() or path in planned_paths:
                continue
            # Stop at the first proving marker. Building every remaining detail
            # would otherwise multiply its O(R) destination listing by an
            # adversarial number of distinct invalid IDs before refusing F.
            count_proposed = dict(usage)
            count_proposed["feedback_records"] = planned_records + 1
            count_proposed["feedback_bytes"] = planned_bytes
            count_reason = self._aux_limit_reason(count_proposed)
            if count_reason is not None:
                self._record_aux_limit(count_reason, count_proposed)
                raise ValueError(
                    count_reason + "; rotate to a fresh Chat state after draining accepted work")
            detail = (f"Chat reply routing error: you referenced ID {json.dumps(nonce[:256])}, "
                      "which is not available. No message was sent for that ID.\n"
                      f"The outstanding user request IDs available in this session are:\n{listing}\n"
                      "Please direct your response to the appropriate request using its exact next ID. "
                      "Use CHAT_REPLY opening and closing tags on their own lines. "
                      "Each complete block is a separate message and later replies increment the ordinal; "
                      "an earlier reply does not close a request that supports multiple replies. "
                      "This is bridge feedback, not a new user request.")
            queue_id = f"feedback-{identity}"
            document: dict[str, object] = {
                "queue_id": queue_id, "text": detail, "created_at": _utc()}
            byte_count = self._document_bytes(document, "reply feedback")
            if byte_count > _MAX_FEEDBACK_ITEM_BYTES:
                raise ValueError(
                    f"reply feedback exceeds {_MAX_FEEDBACK_ITEM_BYTES} bytes")
            byte_proposed = dict(count_proposed)
            byte_proposed["feedback_bytes"] = planned_bytes + byte_count
            byte_reason = self._aux_limit_reason(byte_proposed)
            if byte_reason is not None:
                self._record_aux_limit(byte_reason, byte_proposed)
                raise ValueError(
                    byte_reason + "; rotate to a fresh Chat state after draining accepted work")
            planned.append((path, document, queue_id, detail, byte_count))
            planned_paths.add(path)
            planned_records += 1
            planned_bytes += byte_count

        proposed = dict(usage)
        proposed["feedback_records"] = planned_records
        proposed["feedback_bytes"] = planned_bytes
        self._reserve_queue_prompts([(queue_id, detail)
                                     for _, _, queue_id, detail, _ in planned])
        assert self._aux_usage is not None
        proposed["queue_reserved_bytes"] = self._aux_usage["queue_reserved_bytes"]
        try:
            for path, document, queue_id, detail, _ in planned:
                _write(path, document)
                if created_feedback is not None:
                    created_feedback.append((queue_id, detail))
        except BaseException:
            # A prefix may already be durable, including the write whose
            # directory fsync failed after rename. Rebuild bounded authority
            # before the next admission; retain conservative queue reservations
            # until that recovery instead of trusting an understated cache.
            self._aux_usage = None
            raise
        self._aux_usage = proposed

    def _deliver(self) -> None:
        queue = self.state / "queue"
        paths = sorted((self.state / "requests").glob("*.json"),
                       key=lambda path: get_str(_read(path), "received_at", "request"))
        # `drain` types prompts in the order they were queued, and a queue ID begins
        # with its message's creation time: queue in queue-ID order, so that this
        # pass's requests reach the agent in the order their messages were created.
        for path in sorted(paths, key=lambda path: get_str(_read(path), "queue_id", "request")):
            record = _read(path)
            queue_id = get_str(record, "queue_id", "request")
            if record["phase"] == "received":
                if not any((queue / phase / f"{queue_id}.json").exists()
                           for phase in ("inbox", "inflight", "processed", "failed")):
                    prompt = self._prompt(record)
                    self._reserve_queue_prompts([(queue_id, prompt)])
                    tentative = self._remember_prompt(queue_id, prompt)
                    try:
                        enqueue(str(queue), prompt, message_id=queue_id,
                                max_artifact_bytes=_MAX_QUEUE_ARTIFACT_BYTES,
                                atomic_policy=_chat_atomic_policy(self.state))
                    except Exception:
                        if tentative:
                            self._forget_tentative_prompt(queue_id)
                        raise
                record["phase"] = "queued"
                _write(path, record)
        feedback_pending, feedback_prompts = self._feedback_delivery()
        for identifier, prompt in feedback_prompts:
            self._reserve_queue_prompts([(identifier, prompt)])
            tentative = self._remember_prompt(identifier, prompt)
            try:
                enqueue(str(queue), prompt, message_id=identifier,
                        max_artifact_bytes=_MAX_QUEUE_ARTIFACT_BYTES,
                        atomic_policy=_chat_atomic_policy(self.state))
            except Exception:
                if tentative:
                    self._forget_tentative_prompt(identifier)
                raise
        if feedback_pending or any(record.get("phase") == "queued" for record in map(_read, paths)):
            # Zero readiness wait keeps chat polling responsive while the lead is busy.
            drain(self.client, self.config.target, str(queue), ready_timeout=0,
                  max_artifact_bytes=_MAX_QUEUE_ARTIFACT_BYTES,
                  atomic_policy=_chat_atomic_policy(self.state))
        for path in paths:
            record = _read(path)
            queue_id = get_str(record, "queue_id", "request")
            if record["phase"] == "queued":
                if (queue / "processed" / f"{queue_id}.json").exists():
                    record["phase"] = "awaiting_reply"
                elif (queue / "failed" / f"{queue_id}.json").exists():
                    record["phase"] = "delivery_uncertain"
                if record["phase"] != "queued":
                    _write(path, record)
            lock = _open_private_lock(str(self.state / ".bridge.lock"), "chat bridge lock")
            try:
                fcntl.flock(lock, fcntl.LOCK_EX)
                record = _read(path)
                items = self._recover_reply_state(path, record)
                self._adopt_submission(path, record, items)
            finally:
                os.close(lock)
            if (self.config.outbound_mode == "enabled"
                    and record["phase"] in ("awaiting_reply", "reply_pending", "delivery_uncertain", "replied")):
                while (item := self._next_reply(items)) is not None:
                    lock = _open_private_lock(
                        str(self.state / ".bridge.lock"), "chat bridge lock")
                    try:
                        fcntl.flock(lock, fcntl.LOCK_EX)
                        record = _read(path)
                        self._reply_started(path, record, item)
                    finally:
                        os.close(lock)
                    source = as_mapping(record["message"], "source message")
                    try:
                        result = self._transport_request({"action": "send", "space": self.config.space,
                            "thread": source["thread"], "request_id": item["request_id"],
                            "text": self._outbound_text(get_str(item, "text", "reply item"))})
                        reply_id = get_str(result, "id", "sent reply")
                        if re.fullmatch(re.escape(self.config.space) + r"/messages/[A-Za-z0-9_.-]+", reply_id) is None:
                            raise ValueError("transport returned a reply outside the configured space")
                    except (OSError, ValueError, TypeError, HerdrRunError, subprocess.SubprocessError) as exc:
                        lock = _open_private_lock(
                            str(self.state / ".bridge.lock"), "chat bridge lock")
                        try:
                            fcntl.flock(lock, fcntl.LOCK_EX)
                            record = _read(path)
                            self._reply_complete(path, record, item, None, _utc(), str(exc))
                        finally:
                            os.close(lock)
                        raise
                    lock = _open_private_lock(
                        str(self.state / ".bridge.lock"), "chat bridge lock")
                    try:
                        fcntl.flock(lock, fcntl.LOCK_EX)
                        record = _read(path)
                        self._reply_complete(path, record, item, reply_id, _utc(), None)
                    finally:
                        os.close(lock)
                    items.pop(0)

    def tick(self) -> dict[str, object]:
        """Reconcile replies, poll a page, and attempt ready deliveries once."""
        self.migrate_reply_outboxes()
        records = self._load_request_records()
        self._output_markers(records, retry_failed=True)
        # Subscription/storage refusal precedes target or adapter access.
        resolve_target(self.client, self.config.target)
        self._acknowledge()
        # Recover an accepted reply's lost acknowledgement before reading its
        # echo. This matters when the adapter posts as an allowlisted user.
        self._deliver()
        checkpoint = _read(self.state / "bridge.json")
        result = self._transport_request(self._poll_request(checkpoint))
        descriptor = _open_private_lock(str(self.state / ".bridge.lock"), "chat bridge lock")
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX)
            self._ingest_result(result, checkpoint)
        finally:
            os.close(descriptor)
        self._acknowledge()
        self._deliver()
        return self.status()

    def _output_requests(
        self, records: Sequence[dict[str, object]], *, retry_failed: bool = False,
    ) -> dict[str, str]:
        """Return addressable tagged requests from one already-loaded snapshot."""
        if self.config.outbound_mode == "disabled":
            return {}
        result: dict[str, str] = {}
        for record in records:
            nonce = record.get("reply_nonce")
            if not isinstance(nonce, str) or record.get("phase") not in (
                "queued", "awaiting_reply", "delivery_uncertain", "reply_pending", "replied"
            ) or record.get("reply_closed_at") is not None:
                continue
            if record.get("reply_protocol") != 2 and record.get("phase") in ("reply_pending", "replied"):
                if record.get("reply_protocol") != 3:
                    continue
            if re.fullmatch(r"[A-Za-z0-9_-]{22}", nonce) is None:
                raise ValueError("invalid saved chat reply nonce")
            key = get_str(record, "key", "chat request")
            if record.get("capture_error") and not retry_failed:
                continue
            if nonce in result:
                raise ValueError("saved chat requests contain a duplicate reply nonce")
            result[nonce] = key
        return result

    def _output_markers(
        self, records: Sequence[dict[str, object]], *, retry_failed: bool = False,
    ) -> tuple[str, ...]:
        by_key = {get_str(record, "key", "request"): record for record in records}
        def markers(available: dict[str, str]) -> tuple[str, ...]:
            result: list[str] = []
            for nonce, key in available.items():
                record = by_key[key]
                if record.get("reply_protocol") == 3:
                    ordinal = record.get("reply_next_ordinal")
                    if type(ordinal) is not int:
                        raise ValueError("saved chat reply ordinal must be an integer")
                    result.append(sequenced_reply_id(nonce, ordinal))
                else:
                    result.append(nonce)
            return tuple(result)

        # Validate the complete addressable set before applying capture-error
        # filtering. Otherwise a 129th failed request could be hidden until a
        # later reconnect and open a target/socket with an unsafe state.
        complete = markers(self._output_requests(records, retry_failed=True))
        _closing_patterns(complete)
        return (complete if retry_failed else
                markers(self._output_requests(records, retry_failed=False)))

    def output_requests(self, *, retry_failed: bool = False) -> dict[str, str]:
        """Return addressable tagged requests; protocol v2 stays open after each reply."""
        if self.config.outbound_mode == "disabled":
            return {}
        records = self._load_request_records()
        return self._output_requests(records, retry_failed=retry_failed)

    def output_markers(self, *, retry_failed: bool = False) -> tuple[str, ...]:
        """Return the exact next marker IDs used by line-local subscriptions."""
        if self.config.outbound_mode == "disabled":
            return ()
        records = self._load_request_records()
        return self._output_markers(records, retry_failed=retry_failed)

    def open_output(self, nonces: Sequence[str]) -> PaneOutputStream:
        """Subscribe to unique closing markers after checking the pinned coordinator."""
        if self.config.outbound_mode == "disabled":
            raise ValueError("outbound Chat is disabled; reply capture is unavailable")
        records = self._load_request_records()
        self._output_markers(records, retry_failed=True)
        patterns = _closing_patterns(nonces)
        info = resolve_target(self.client, self.config.target)
        return PaneOutputStream(self.client.event_socket(), info.pane_id,
                               patterns, watch_settled=True)

    def _output_subscription(
        self, records: Sequence[dict[str, object]], *, watch_delivery: bool = False,
    ) -> _OutputSubscription:
        """Authorize a subscription from the owner's already-validated request cache."""
        complete = self._output_markers(records, retry_failed=True)
        markers = self._output_markers(records)
        watching = (watch_delivery or (self.config.outbound_mode == "enabled"
                    and (self.config.reply_mode == "tagged" or bool(complete))))
        return _OutputSubscription(self, complete, markers, watching)

    def _open_cached_output(self, subscription: _OutputSubscription) -> PaneOutputStream:
        """Open only an owner-authorized immutable snapshot, with no state-file I/O."""
        if subscription.owner is not self or not subscription.watching:
            raise ValueError("output subscription is not authorized for this bridge")
        _closing_patterns(subscription.complete)
        if not set(subscription.markers).issubset(subscription.complete):
            raise ValueError("output markers are outside the authorized request snapshot")
        if self.config.outbound_mode == "disabled" and subscription.markers:
            raise ValueError("outbound Chat is disabled; reply capture is unavailable")
        patterns = (() if self.config.outbound_mode == "disabled"
                    else _closing_patterns(subscription.markers))
        info = resolve_target(self.client, self.config.target)
        return PaneOutputStream(self.client.event_socket(), info.pane_id,
                               patterns, watch_settled=True)

    def capture_event(self, event: PaneOutputSnapshot | PaneAgentStatus, *, deliver: bool = True) -> dict[str, object]:
        """Treat settled-state events as a hint to recheck incomplete replies."""
        return self._capture_event(event, deliver=deliver)

    def _capture_event(
        self, event: PaneOutputSnapshot | PaneAgentStatus, *, deliver: bool = True,
        records: Sequence[dict[str, object]] | None = None,
    ) -> dict[str, object]:
        if self.config.outbound_mode == "disabled":
            return {"captured": [], "errors": []}
        if isinstance(event, PaneOutputSnapshot):
            return self._capture_output(event, deliver=deliver, verify_target=True, records=records)
        info = resolve_target(self.client, self.config.target)
        if info.pane_id != event.pane_id:
            raise HerdrUnavailable("chat status event belongs to a different coordinator pane")
        if info.status not in ("idle", "done"):
            return {"captured": [], "errors": []}
        # One read per settled event recovers an early close inside a quoted
        # example, or a missed output edge. Idle alone never proves an answer.
        text = self.client.read(info.pane_id, source="recent-unwrapped", lines=4000)
        if len(text.encode("utf-8")) > 2 * 1024 * 1024:
            raise ValueError("retained chat output exceeds the 2 MiB capture limit")
        return self._capture_output(PaneOutputSnapshot(info.pane_id, text, None),
                                    deliver=deliver, verify_target=True, records=records)

    def capture_output(self, snapshot: PaneOutputSnapshot, *, deliver: bool = True) -> dict[str, object]:
        """Durably capture complete matching blocks and immediately reconcile the outbox."""
        return self._capture_output(snapshot, deliver=deliver, verify_target=True)

    def _capture_output(
        self, snapshot: PaneOutputSnapshot, *, deliver: bool, verify_target: bool,
        records: Sequence[dict[str, object]] | None = None,
        touched: set[str] | None = None,
        reply_items: dict[str, list[dict[str, object]]] | None = None,
        feedback_prompts: list[tuple[str, str]] | None = None,
    ) -> dict[str, object]:
        if self.config.outbound_mode == "disabled":
            return {"captured": [], "errors": []}
        preloaded = list(records) if records is not None else self._load_request_records()
        self._output_markers(preloaded, retry_failed=True)
        if verify_target:
            info = resolve_target(self.client, self.config.target)
            if snapshot.pane_id != info.pane_id:
                raise HerdrUnavailable("chat reply output belongs to a different coordinator pane")
        descriptor = _open_private_lock(str(self.state / ".bridge.lock"), "chat bridge lock")
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
            captured: list[str] = []
            errors: list[dict[str, str]] = []
            text, recent = self._prompt_echo_views(snapshot.text)
            loaded = preloaded
            available = self._output_requests(loaded, retry_failed=True)
            by_key = {get_str(record, "key", "request"): record for record in loaded}
            marker_ids, closing_ids = reply_marker_sets(text)
            # Only routing diagnostics trim output predating the oldest retained
            # prompt. Valid replies to older still-addressable requests must remain
            # capturable even when their own prompt has scrolled away.
            feedback_ids = marker_ids if recent == text else reply_marker_ids(recent)
            closing_by_nonce: dict[str, set[str]] = {}
            for identifier in closing_ids:
                match = re.fullmatch(r"([A-Za-z0-9_-]{22})_[1-9][0-9]{0,5}", identifier)
                base = match.group(1) if match is not None else identifier
                closing_by_nonce.setdefault(base, set()).add(identifier)
            self._unknown_reply_feedback(
                recent, available, loaded, feedback_ids,
                inspect_legacy_history=reply_items is None,
                created_feedback=feedback_prompts)
            v3_nonces = {nonce for nonce, key in available.items()
                         if by_key[key].get("reply_protocol") == 3}
            v2_nonces = set(available) - v3_nonces
            try:
                parsed_v3 = extract_sequenced_replies(
                    text, v3_nonces, marker_ids=marker_ids)
            except ValueError:
                parsed_v3 = None
            try:
                parsed_v2 = extract_replies(text, v2_nonces)
            except ValueError:
                parsed_v2 = None
            for nonce, key in available.items():
                record_snapshot = by_key[key]
                protocol = record_snapshot.get("reply_protocol")
                identifiers = closing_by_nonce.get(nonce, set())
                if not identifiers:
                    continue
                path = self.state / "requests" / f"{key}.json"
                record = _read(path)
                pending = (reply_items.get(key) if reply_items is not None else None)
                if pending is None:
                    pending = self._recover_reply_state(path, record)
                    if reply_items is not None:
                        reply_items[key] = pending
                self._adopt_submission(path, record, pending)
                original_record = dict(record)
                try:
                    if protocol == 3:
                        answers_v3 = (parsed_v3.get(nonce, []) if parsed_v3 is not None
                                      else extract_sequenced_replies(
                                          text, (nonce,), marker_ids=marker_ids).get(nonce, []))
                        if not answers_v3:
                            raise ValueError("closing marker is visible but no complete protocol-v3 reply block is retained")
                        added = self._append_sequenced_replies(record, answers_v3, pending)
                    else:
                        answers_v2 = (parsed_v2.get(nonce, []) if parsed_v2 is not None
                                      else extract_replies(text, (nonce,)).get(nonce, []))
                        if not answers_v2:
                            raise ValueError("closing marker is visible but no complete reply block is retained")
                        # Upgrade an already-delivered legacy prompt while keeping its old marker
                        # valid; historical completed legacy requests remain closed during migration.
                        record["reply_protocol"] = 2
                        added = self._append_replies(record, answers_v2, pending)
                    if protocol == 3 and record.get("reply_protocol") != 3:
                        raise ValueError("saved request changed reply protocol during capture")
                except ValueError as exc:
                    error = str(exc)[:2000]
                    record.update(capture_error=error, capture_failed_at=_utc())
                    errors.append({"request": key, "error": error})
                else:
                    record.pop("capture_error", None)
                    record.pop("capture_failed_at", None)
                    if added:
                        record["reply_capture"] = {"source": "herdr_output", "pane_id": snapshot.pane_id,
                                                   "captured_at": _utc(), "snapshot_truncated": snapshot.truncated}
                        captured.append(key)
                if record != original_record:
                    _write(path, record)
                    if touched is not None:
                        touched.add(key)
            outcome: dict[str, object] = {"captured": captured, "errors": errors}
        finally:
            os.close(descriptor)
        # Existing send request IDs retain their idempotency semantics. A
        # provider failure here leaves the captured artifact ready for retry.
        # The provider call must never run while the reply-writer lock is held.
        if deliver:
            self._deliver()
        return outcome

    def capture_once(self) -> dict[str, object]:
        """Inspect retained matching output once, including explicitly retried failures."""
        if self.config.outbound_mode == "disabled":
            return {"captured": [], "errors": []}
        records = self._load_request_records()
        nonces = self._output_markers(records, retry_failed=True)
        if not nonces and self.config.reply_mode != "tagged":
            return {"captured": [], "errors": []}
        stream = self.open_output(nonces)
        try:
            for snapshot in stream.wait(0.25):
                return self.capture_event(snapshot)
            return {"captured": [], "errors": []}
        finally:
            stream.close()

    def status(self) -> dict[str, object]:
        """Return durable request phases without accessing Chat or the harness."""
        records = self._load_request_records()
        request_usage = dict(self._request_usage or self._request_population(records))
        aux_usage = self.validate_aux_population()
        active_subscriptions = len(self._output_requests(records, retry_failed=True))
        usage = {"items": 0, "bytes": 0, "pending_items": 0, "pending_bytes": 0,
                 "migration_required": 0}
        for record in records:
            if record.get("reply_storage") != 2:
                usage["migration_required"] += 1
                continue
            count, sent, total_bytes, pending_bytes = self._reply_summary(record)
            usage["items"] += count
            usage["bytes"] += total_bytes
            usage["pending_items"] += count - sent
            usage["pending_bytes"] += pending_bytes
        return {"space": self.config.space, "target": asdict(self.config.target),
                "agent_name": self.config.agent_name,
                "ack_reaction": (self.config.ack_reaction
                                 if self.config.outbound_mode == "enabled" else None),
                "reply_mode": self.config.reply_mode, "outbound_mode": self.config.outbound_mode,
                "request_storage": {
                    "usage": request_usage,
                    "limits": {"records": _MAX_REQUEST_RECORDS,
                               "source_bytes": _MAX_REQUEST_SOURCE_BYTES,
                               "encoded_bytes": _MAX_REQUEST_ENCODED_BYTES,
                               "message_bytes": _MAX_MESSAGE_SOURCE_BYTES,
                               "encoded_record_bytes": _MAX_REQUEST_FILE_BYTES},
                    "last_refusal": (_read(self.state / "request-limit.json")
                                     if (self.state / "request-limit.json").exists() else None)},
                "aux_storage": {
                    "usage": aux_usage,
                    "limits": {"feedback_records": _MAX_FEEDBACK_RECORDS,
                               "feedback_bytes": _MAX_FEEDBACK_BYTES,
                               "deferred_records": _MAX_DEFERRED_RECORDS,
                               "deferred_bytes": _MAX_DEFERRED_BYTES,
                               "queue_records": _MAX_REQUEST_RECORDS + _MAX_FEEDBACK_RECORDS,
                               "queue_bytes": _MAX_QUEUE_BYTES,
                               "queue_artifact_bytes": _MAX_QUEUE_ARTIFACT_BYTES},
                    "last_refusal": (_read(self.state / "population-limit.json")
                                     if (self.state / "population-limit.json").exists() else None)},
                "reply_subscriptions": {"active": active_subscriptions,
                                        "limit": _MAX_OUTPUT_SUBSCRIPTIONS},
                "reply_storage": {
                    "usage": usage,
                    "limits": {"state_items": _MAX_STATE_REPLY_ITEMS,
                               "state_bytes": _MAX_STATE_REPLY_BYTES,
                               "request_items": _MAX_REQUEST_REPLY_ITEMS,
                               "request_bytes": _MAX_REQUEST_REPLY_BYTES,
                               "pending_items": _MAX_PENDING_REPLY_ITEMS,
                               "pending_bytes": _MAX_PENDING_REPLY_BYTES}},
                "input_observer": _read(self.state / "input.json") if (self.state / "input.json").exists() else None,
                "output_observer": _read(self.state / "output.json") if (self.state / "output.json").exists() else None,
                "requests": records}

    def validate_continuous_output(self) -> None:
        """Refuse unsupported or oversized subscriptions before external access."""
        records = self._load_request_records()
        if self.config.outbound_mode == "disabled":
            return
        legacy = [get_str(record, "key", "request") for record in records
                  if (record.get("reply_closed_at") is None
                      and isinstance(record.get("reply_nonce"), str)
                      and ((record.get("reply_protocol") == 2
                            and record.get("phase") in (
                                "queued", "awaiting_reply", "delivery_uncertain",
                                "reply_pending", "replied"))
                           or (record.get("reply_protocol") not in (2, 3)
                               and record.get("phase") in (
                                   "queued", "awaiting_reply", "delivery_uncertain"))))]
        if legacy:
            listing = ", ".join(key[:12] for key in legacy[:5])
            suffix = "..." if len(legacy) > 5 else ""
            raise ValueError(
                "continuous output refuses active protocol-v2 requests "
                f"({listing}{suffix}); recover them with chat tick/the previous bridge, then run "
                "chat close --request KEY for each, or initialize a fresh state while retaining this one")
        _closing_patterns(self._output_markers(records, retry_failed=True))


def _run_bridge(
    bridge: Bridge, interval: float, prog: str, *, reconcile_interval: float = 300,
    observer_write_interval: float = _DEFAULT_OBSERVER_WRITE_INTERVAL,
) -> None:
    """Run one durable bridge owner with streaming or polling intake."""
    descriptor = _open_private_lock(str(bridge.state / ".run.lock"), "chat runner lock")
    previous = signal.getsignal(signal.SIGTERM)
    handle_signals = threading.current_thread() is threading.main_thread()
    def terminate(signum: int, frame: object) -> None:
        raise _ServiceTerminated
    try:
        fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        _audit_chat_temporaries(bridge.state, descriptor)
        bridge.migrate_reply_outboxes()
        bridge.validate_continuous_output()
        if handle_signals:
            signal.signal(signal.SIGTERM, terminate)
        if bridge.config.event_command:
            from agentctl.chat_runtime import run_streaming
            run_streaming(bridge, reconcile_interval=reconcile_interval, prog=prog,
                          observer_write_interval=observer_write_interval)
        else:
            _run_polling(bridge, interval, prog,
                         observer_write_interval=observer_write_interval)
    finally:
        if handle_signals:
            signal.signal(signal.SIGTERM, previous)
        os.close(descriptor)


def _run_polling(
    bridge: Bridge, interval: float, prog: str, *,
    observer_write_interval: float = _DEFAULT_OBSERVER_WRITE_INTERVAL,
) -> None:
    """Poll Chat on its schedule while blocking for terminal reply events between polls."""
    # Queue artifacts move between phases but their prompt text is immutable.
    # Rebuild once before the event loop; _deliver registers every later prompt
    # before enqueue, so output events never rescan the queue.
    bridge._prime_prompt_cache()
    observer = _OutputObserver(bridge.state / "output.json", observer_write_interval)
    stream: PaneOutputStream | None = None
    subscribed: _OutputSubscription | None = None
    subscription: _OutputSubscription | None = None
    refresh = True
    delivery_pending = False
    next_delivery = float("inf")
    next_poll = 0.0
    poll_delay = interval
    reconnect_at = 0.0
    reconnect_delay = 1.0
    errors = (OSError, ValueError, TypeError, HerdrRunError, subprocess.SubprocessError)
    try:
        while True:
            observer.flush()
            if time.monotonic() >= next_poll:
                try:
                    bridge.tick()
                except errors as exc:
                    print(f"{prog}: {exc}", file=sys.stderr, flush=True)
                    # Fast interactive polls back off to one minute. Conservative long polls keep
                    # doubling toward one day, so a broken or quota-limited adapter gets quieter
                    # instead of retrying forever at the ordinary hourly cadence.
                    failure_limit = (
                        _SHORT_FAILURE_BACKOFF_LIMIT
                        if interval <= _SHORT_FAILURE_BACKOFF_LIMIT
                        else _MAX_POLL_INTERVAL
                    )
                    poll_delay = max(interval, min(failure_limit, poll_delay * 2))
                else:
                    poll_delay = interval
                next_poll = time.monotonic() + poll_delay
                next_delivery = time.monotonic() + 300
                refresh = True
            if delivery_pending and time.monotonic() >= next_delivery:
                try:
                    bridge._deliver()
                except errors as exc:
                    print(f"{prog}: delivery: {exc}", file=sys.stderr, flush=True)
                next_delivery = time.monotonic() + 300
                refresh = True
            if refresh:
                # One validated snapshot for both marker sets and open/reopen.
                # Empty waits and observer flushes reuse the immutable result.
                records = bridge._load_request_records()
                delivery_pending = (any(record.get("phase") in ("received", "queued")
                                        for record in records)
                                    or bool(bridge._feedback_pending_records()))
                subscription = bridge._output_subscription(
                    records, watch_delivery=delivery_pending)
                refresh = False
            assert subscription is not None
            watching = subscription.watching
            if subscription != subscribed:
                if stream is not None:
                    stream.close()
                stream = None
                subscribed = subscription
                reconnect_at = 0.0
                reconnect_delay = 1.0
                if not watching:
                    observer.observe("idle", None)
            if watching and stream is None and time.monotonic() >= reconnect_at:
                try:
                    stream = bridge._open_cached_output(subscription)
                except errors as exc:
                    print(f"{prog}: output subscription: {exc}", file=sys.stderr, flush=True)
                    observer.observe("retrying", str(exc)[:2000])
                    reconnect_at = time.monotonic() + reconnect_delay
                    reconnect_delay = min(60.0, reconnect_delay * 2)
                else:
                    observer.observe("connected", None)
                    reconnect_delay = 1.0
            deadline = next_poll
            if delivery_pending:
                deadline = min(deadline, next_delivery)
            if watching and stream is None:
                deadline = min(deadline, reconnect_at)
            if observer.next_write is not None:
                deadline = min(deadline, observer.next_write)
            timeout = max(0.0, deadline - time.monotonic())
            if stream is None:
                time.sleep(timeout)
                continue
            try:
                for snapshot in stream.wait(timeout):
                    if isinstance(snapshot, PaneAgentStatus) and delivery_pending:
                        info = resolve_target(bridge.client, bridge.config.target)
                        if info.pane_id != snapshot.pane_id:
                            raise HerdrUnavailable("chat status event belongs to a different coordinator pane")
                        if info.status in ("idle", "done"):
                            next_delivery = 0.0
                    outcome = bridge._capture_event(snapshot, records=records)
                    refresh = True
                    for error in as_sequence(outcome["errors"], "capture errors"):
                        print(f"{prog}: output capture: {error}", file=sys.stderr, flush=True)
            except errors as exc:
                print(f"{prog}: output subscription: {exc}", file=sys.stderr, flush=True)
                observer.observe("retrying", str(exc)[:2000])
                stream.close()
                stream = None
                reconnect_at = time.monotonic() + reconnect_delay
                reconnect_delay = min(60.0, reconnect_delay * 2)
    finally:
        if stream is not None:
            stream.close()


def _request_is_enqueued(state: Path, key: str, record: dict[str, object]) -> bool:
    """Recognize delivery while the owner's saved request phase lags behind it."""
    queue_id = get_str(record, "queue_id", "request")
    if record.get("key") != key or re.fullmatch(r"[0-9]{20}-" + key, queue_id) is None:
        raise ValueError("invalid request queue identity")
    root = state / "queue"
    _validate_existing_queue(str(root))
    # Delivery moves artifacts forward through these directories. A concurrent
    # rename between lookup and open should continue at the later location.
    for phase in ("inbox", "inflight", "processed", "failed"):
        try:
            queued = _read(root / phase / f"{queue_id}.json")
        except AgentDeliveryError as exc:
            if (isinstance(exc.__cause__, FileNotFoundError)
                    or "changed while it was read:" in str(exc)):
                continue
            raise
        if queued.get("id") != queue_id or not get_str(queued, "text", "queued request"):
            raise ValueError("queue artifact does not match the request")
        return True
    return False


def _wake_reply_owner(state: Path, key: str) -> None:
    """Best-effort notification after the reply artifact is already durable."""
    wake_path = state / ".wake.sock"
    try:
        metadata = wake_path.lstat()
        if stat.S_ISSOCK(metadata.st_mode) and metadata.st_uid == os.getuid():
            with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as wake:
                wake.setblocking(False)
                wake.sendto(f"reply:{key}".encode("ascii"), str(wake_path))
    except OSError:
        # A stopped or starting runner finds the artifact during recovery.
        pass


def submit_reply(state: Path, key: str, text: str) -> None:
    """Commit one final answer locally; the bridge alone has transport credentials."""
    saved = as_mapping(_read(state / "bridge.json").get("config"), "saved chat config")
    if Config.parse(saved).outbound_mode == "disabled":
        raise ValueError("outbound Chat is disabled; reply artifacts are refused")
    if re.fullmatch(r"[0-9a-f]{64}", key) is None:
        raise ValueError("invalid request key")
    body_bytes = len(text.encode("utf-8"))
    if not text.strip() or body_bytes > 30000:
        raise ValueError("reply must contain 1-30000 UTF-8 bytes")
    _private(state / "submissions")
    path = state / "submissions" / f"{key}.json"
    descriptor = _open_private_lock(
        str(state / ".submissions.lock"), "chat reply submission lock")
    try:
        # Serialize local creators without taking the owner lock: output capture
        # must be able to race this create-only artifact, then deterministically
        # adopt/shift it. The bounded scan is a CLI operation, never a service
        # inner-loop operation, and prevents a stopped runner leaking disk.
        fcntl.flock(descriptor, fcntl.LOCK_EX)
        with atomic_write_recovery(_chat_atomic_policy(state)):
            pass
        if not _atomic_audit_marker(state):
            _audit_scattered_temporaries([(state / "submissions", 0)])
        record = _read(state / "requests" / f"{key}.json")
        if record.get("reply_storage") != 2:
            raise ValueError("reply outbox requires chat tick/run migration before file submission")
        count = Bridge._reply_summary_count(record, "reply_item_count")
        sent = Bridge._reply_summary_count(record, "reply_sent_count")
        if count:
            first = _read(_reply_item_path(state, key, 0, sent=sent > 0))
            if first.get("text") != text:
                raise ValueError("a different reply already exists for this request")
        elif path.exists():
            if _read(path).get("text") != text:
                raise ValueError("a different reply already exists for this request")
        else:
            if record.get("reply_closed_at") is not None:
                raise ValueError("reply capture is closed for this request")
            phase = record.get("phase")
            if (phase not in ("queued", "awaiting_reply", "reply_pending", "delivery_uncertain")
                    and not (phase == "received" and _request_is_enqueued(state, key, record))):
                raise ValueError("request is not awaiting a reply")

            submission_items = 0
            queued_submission_bytes = 0
            for submission_path in (state / "submissions").glob("*.json"):
                submission = _read(submission_path)
                if submission.get("request") != submission_path.stem:
                    raise ValueError("reply submission key does not match its path")
                submission_text = get_str(submission, "text", "reply submission")
                saved_submission_bytes = len(submission_text.encode("utf-8"))
                if not submission_text.strip() or saved_submission_bytes > 30_000:
                    raise ValueError("saved reply submission is outside the supported byte range")
                submission_items += 1
                queued_submission_bytes += saved_submission_bytes
            limits = (
                (submission_items + 1 > _MAX_PENDING_REPLY_ITEMS,
                 f"local reply submission count limit {_MAX_PENDING_REPLY_ITEMS} reached"),
                (queued_submission_bytes + body_bytes > _MAX_PENDING_REPLY_BYTES,
                 f"local reply submission byte limit {_MAX_PENDING_REPLY_BYTES} reached"),
            )
            reason = next((message for failed, message in limits if failed), None)
            if reason is not None:
                raise ValueError(reason)
            document: dict[str, object] = {"request": key, "text": text}
            try:
                _create_chat_json(state, path, document)
            except FileExistsError:
                if _read(path).get("text") != text:
                    raise ValueError("a different reply already exists for this request") from None
    finally:
        os.close(descriptor)
    _wake_reply_owner(state, key)


def close_replies(state: Path, key: str) -> str:
    """Explicitly stop output subscriptions without deleting durable history."""
    run_lock = _open_private_lock(str(state / ".run.lock"), "chat runner lock")
    bridge_lock = -1
    try:
        fcntl.flock(run_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        _audit_chat_temporaries(state, run_lock)
        bridge_lock = _open_private_lock(str(state / ".bridge.lock"), "chat bridge lock")
        fcntl.flock(bridge_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        path = _request_path(state, key)
        record = _read(path)
        if record.get("key") != path.stem:
            raise ValueError("saved request key does not match its file")
        if (record.get("reply_protocol") not in (None, 2, 3)
                or not isinstance(record.get("reply_nonce"), str)):
            raise ValueError("request has no closeable tagged-reply protocol")
        if record.get("reply_closed_at") is None:
            record["reply_closed_at"] = _utc()
            record["reply_close_reason"] = "operator"
            _write(path, record)
        return path.stem
    finally:
        if bridge_lock >= 0:
            os.close(bridge_lock)
        os.close(run_lock)


def read_reply_history(state: Path, key: str) -> dict[str, object]:
    """Read every retained reply body for an explicit deep audit."""
    bridge = Bridge(state)
    descriptor = _open_private_lock(str(state / ".bridge.lock"), "chat bridge lock")
    try:
        fcntl.flock(descriptor, fcntl.LOCK_SH)
        path = _request_path(state, key)
        record = _read(path)
        if record.get("reply_storage") != 2:
            raise ValueError("reply history requires chat tick/run migration first")
        items = bridge._all_reply_items(record)
        count, sent, total_bytes, pending_bytes = bridge._reply_summary(record)
        return {"request": path.stem, "count": count, "sent": sent,
                "total_bytes": total_bytes, "pending_bytes": pending_bytes,
                "items": items}
    finally:
        os.close(descriptor)


def _launch_config(
    document: dict[str, object], client: _LaunchClient, *, harness: str,
    model: str | None, agent_label: str | None,
) -> Config:
    """Bind reusable Chat authority to the shell pane invoking ``launch``."""
    pane_id = os.environ.get("HERDR_PANE_ID")
    workspace_id = os.environ.get("HERDR_WORKSPACE_ID")
    if os.environ.get("HERDR_ENV") != "1" or not pane_id or not workspace_id:
        raise ValueError("chat launch must run inside the Herdr shell pane that will host the coordinator")
    info = client.pane_info(pane_id)
    if info.workspace_id != workspace_id:
        raise ValueError("current Herdr pane and workspace environment do not match")
    if info.agent is not None:
        raise ValueError("current Herdr pane already hosts an agent; run chat launch from a shell pane")
    executable = shutil.which(harness)
    if executable is None:
        raise ValueError(f"cannot find {harness!r} on PATH")
    configured = dict(document)
    configured["target"] = {
        "pane_id": pane_id,
        "session_agent": None,
        "session_value": None,
        "expected_agent": harness,
        "expected_workspace": client.workspace_label(workspace_id),
        "expected_cwd": info.cwd,
    }
    # This coordinator is owned by the invoking shell rather than agentctl's
    # named-session registry. The exact pane/workspace/cwd assertions remain.
    configured.pop("agent_name", None)
    if agent_label is not None:
        configured["agent_label"] = agent_label
    elif model is not None:
        configured["agent_label"] = model
    elif not configured.get("agent_label"):
        configured["agent_label"] = harness
    return Config.parse(configured)


class _LinuxSigset(ctypes.Structure):
    _fields_ = [("values", ctypes.c_ulong * 16)]


class _LinuxSigaction(ctypes.Structure):
    _fields_ = [
        ("handler", ctypes.c_void_p),
        ("mask", _LinuxSigset),
        ("flags", ctypes.c_int),
        ("restorer", ctypes.c_void_p),
    ]


_SA_NOCLDWAIT = 2


def _require_waitable_sigchld_children(context: str) -> None:
    """Require exact Linux SIGCHLD state that preserves child identity and status."""
    action = _LinuxSigaction()
    libc = ctypes.CDLL(None, use_errno=True)
    libc.sigaction.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.POINTER(_LinuxSigaction)]
    libc.sigaction.restype = ctypes.c_int
    if int(libc.sigaction(signal.SIGCHLD, None, ctypes.byref(action))) != 0:
        code = ctypes.get_errno()
        raise OSError(code, os.strerror(code))
    if action.handler not in (None, 0) or action.flags & _SA_NOCLDWAIT:
        raise ValueError(
            f"{context} requires SIGCHLD=SIG_DFL with SA_NOCLDWAIT clear",
        )


def _require_waitable_launch_children() -> None:
    """Refuse launch when direct children could disappear without a wait status."""
    if (not hasattr(os, "pidfd_open") or not hasattr(os, "P_PIDFD")
            or not hasattr(signal, "pidfd_send_signal")):
        raise ValueError("chat launch supervision requires Linux process descriptors")
    _require_waitable_sigchld_children("chat launch")
    probes: list[int] = []
    try:
        # Prove enough descriptor headroom before either product process can
        # exist. Activation gates still fail closed if a competing thread wins
        # the later allocation race.
        probes.append(os.pidfd_open(os.getpid()))
        probes.append(os.pidfd_open(os.getpid()))
    except OSError as exc:
        raise ValueError(f"chat launch process descriptors are unavailable: {exc}") from exc
    finally:
        for descriptor in probes:
            os.close(descriptor)


@dataclass
class _LaunchChild:
    name: str
    process: subprocess.Popen[bytes]
    pidfd: int
    identity: _CommandAnchorIdentity
    activation_write: int | None
    group_leader: bool
    observed_returncode: int | None = None
    reaped: bool = False


def _wait_launch_fd(descriptor: int, timeout: float | None) -> bool:
    selector = selectors.DefaultSelector()
    try:
        selector.register(descriptor, selectors.EVENT_READ)
        return bool(selector.select(timeout))
    finally:
        selector.close()


def _wait_launch_gate_ready(name: str, descriptor: int, pidfd: int) -> None:
    """Require the inert gate to arm its parent-death lifeline before return."""
    selector = selectors.DefaultSelector()
    try:
        selector.register(descriptor, selectors.EVENT_READ, "ready")
        selector.register(pidfd, selectors.EVENT_READ, "exited")
        events = selector.select(_LAUNCH_GATE_READY_TIMEOUT)
        kinds = {str(key.data) for key, _ in events}
        if "ready" not in kinds:
            if "exited" in kinds:
                raise ValueError(f"chat launch {name} gate exited before publishing readiness")
            raise TimeoutError(f"chat launch {name} gate did not publish readiness")
        readiness = os.read(descriptor, 2)
        if readiness != b"1":
            raise ValueError(f"chat launch {name} gate published invalid readiness")
        if "exited" in kinds or _wait_launch_fd(pidfd, 0):
            raise ValueError(f"chat launch {name} gate exited after publishing readiness")
    finally:
        selector.close()


def _cleanup_launch_gate_without_pidfd(
    name: str, process: subprocess.Popen[bytes], expected: _CommandAnchorIdentity,
) -> None:
    """Wake, kill, reap, and prove disappearance of one validated direct child."""
    _cleanup_unpinned_direct_child(process, expected, f"chat launch {name} gate")


def _spawn_launch_child(
    name: str,
    command: Sequence[str],
    *,
    group_leader: bool,
    stdin: int | IO[bytes] | None = None,
    stdout: int | IO[bytes] | None = None,
    stderr: int | IO[bytes] | None = None,
) -> _LaunchChild:
    activation_read, activation_write = os.pipe2(os.O_CLOEXEC)
    try:
        readiness_read, readiness_write = os.pipe2(os.O_CLOEXEC)
    except BaseException:
        os.close(activation_read)
        os.close(activation_write)
        raise
    gate = Path(__file__).with_name("_launch_gate.py")
    process: subprocess.Popen[bytes] | None = None
    pidfd: int | None = None
    expected: _CommandAnchorIdentity | None = None
    try:
        process = subprocess.Popen(
            (
                sys.executable,
                str(gate),
                str(os.getpid()),
                str(activation_read),
                str(readiness_write),
                *command,
            ),
            stdin=stdin,
            stdout=stdout,
            stderr=stderr,
            start_new_session=group_leader,
            pass_fds=(activation_read, readiness_write),
        )
        os.close(activation_read)
        activation_read = -1
        os.close(readiness_write)
        readiness_write = -1
        expected = _command_process_identity(process.pid)
        if (expected is None or expected.ppid != os.getpid()
                or expected.state in ("X", "Z")
                or (group_leader and (
                    expected.pgrp != process.pid or expected.session != process.pid
                ))):
            raise ValueError(f"chat launch {name} gate did not publish a stable child identity")
        try:
            pidfd = os.pidfd_open(process.pid)
        except OSError as exc:
            raise ValueError(
                f"chat launch could not obtain the {name} process descriptor: {exc}",
            ) from exc
        observed = _command_process_identity(process.pid)
        if (not _same_command_process(expected, observed) or observed is None
                or observed.ppid != os.getpid() or observed.state in ("X", "Z")
                or (group_leader and (
                    observed.pgrp != process.pid or observed.session != process.pid
                ))):
            raise ValueError(f"chat launch {name} identity changed before activation")
        _wait_launch_gate_ready(name, readiness_read, pidfd)
        os.close(readiness_read)
        readiness_read = -1
        ready_identity = _command_process_identity(process.pid)
        if (not _same_command_process(observed, ready_identity) or ready_identity is None
                or ready_identity.ppid != os.getpid() or ready_identity.state in ("X", "Z")
                or (group_leader and (
                    ready_identity.pgrp != process.pid or ready_identity.session != process.pid
                ))):
            raise ValueError(f"chat launch {name} identity changed after readiness")
        return _LaunchChild(
            name, process, pidfd, ready_identity, activation_write, group_leader,
        )
    except BaseException:
        if activation_write >= 0:
            os.close(activation_write)
            activation_write = -1
        if readiness_read >= 0:
            os.close(readiness_read)
            readiness_read = -1
        try:
            if process is not None:
                # Closing activation normally makes the inert gate exit 125.
                # If a post-spawn validation fault caught a stopped gate, use
                # its already-pinned pidfd rather than leaking it or falling
                # back to a numeric PID. An allocation failure gets one retry
                # after closing the pipe freed a descriptor.
                if pidfd is None and expected is not None:
                    observed = _command_process_identity(process.pid)
                    if (_same_command_process(expected, observed) and observed is not None
                            and observed.ppid == os.getpid() and observed.state != "X"):
                        try:
                            pidfd = os.pidfd_open(process.pid)
                        except OSError:
                            pass
                if pidfd is not None:
                    if not _wait_launch_fd(pidfd, 1):
                        try:
                            signal.pidfd_send_signal(pidfd, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                        if not _wait_launch_fd(pidfd, 5):
                            raise RuntimeError(
                                f"chat launch {name} gate survived exact-handle cleanup",
                            )
                    process.wait(timeout=1)
                    if (expected is not None and _same_command_process(
                        expected, _command_process_identity(process.pid),
                    )):
                        raise RuntimeError(f"chat launch {name} gate remained after exact reap")
                elif expected is not None:
                    _cleanup_launch_gate_without_pidfd(name, process, expected)
                else:
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired as exc:
                        raise RuntimeError(
                            f"chat launch {name} gate did not fail closed",
                        ) from exc
        finally:
            if pidfd is not None:
                os.close(pidfd)
        raise
    finally:
        if activation_read >= 0:
            os.close(activation_read)
        if readiness_write >= 0:
            os.close(readiness_write)
        if process is None:
            if activation_write >= 0:
                os.close(activation_write)
            if readiness_read >= 0:
                os.close(readiness_read)


def _activate_launch_child(child: _LaunchChild) -> None:
    descriptor = child.activation_write
    if descriptor is None:
        raise ValueError(f"chat launch {child.name} gate was already activated")
    try:
        while True:
            try:
                written = os.write(descriptor, b"1")
                break
            except InterruptedError:
                continue
        if written != 1:
            raise ValueError(f"chat launch {child.name} gate rejected activation")
    finally:
        os.close(descriptor)
        child.activation_write = None


def _peek_launch_status(child: _LaunchChild) -> int:
    if child.observed_returncode is not None:
        return child.observed_returncode
    status = os.waitid(os.P_PIDFD, child.pidfd, os.WEXITED | os.WNOWAIT)
    if status is None:
        raise RuntimeError(f"chat launch {child.name} pidfd became ready without a wait status")
    if status.si_code == os.CLD_EXITED:
        result = status.si_status
    elif status.si_code in (os.CLD_KILLED, os.CLD_DUMPED):
        result = -status.si_status
    else:
        raise RuntimeError(f"chat launch {child.name} produced an unsupported wait status")
    child.observed_returncode = result
    return result


def _reap_launch_child(child: _LaunchChild) -> int:
    if child.reaped:
        if child.observed_returncode is None:
            raise RuntimeError(f"chat launch {child.name} was reaped without a status")
        return child.observed_returncode
    expected = _peek_launch_status(child)
    actual = child.process.wait(timeout=1)
    child.reaped = True
    if actual != expected:
        raise RuntimeError(
            f"chat launch {child.name} wait status changed from {expected} to {actual}",
        )
    return actual


def _signal_launch_child(child: _LaunchChild, signum: int) -> None:
    try:
        signal.pidfd_send_signal(child.pidfd, signum)
    except ProcessLookupError:
        pass


def _stop_launch_child(child: _LaunchChild) -> int:
    if child.reaped:
        return _reap_launch_child(child)
    if child.activation_write is not None:
        os.close(child.activation_write)
        child.activation_write = None
    if not _wait_launch_fd(child.pidfd, 0):
        _signal_launch_child(child, signal.SIGTERM)
        if not _wait_launch_fd(child.pidfd, 10):
            _signal_launch_child(child, signal.SIGKILL)
            if not _wait_launch_fd(child.pidfd, 5):
                raise RuntimeError(f"chat launch {child.name} did not exit after SIGKILL")
    return _reap_launch_child(child)


def _validated_launch_group(child: _LaunchChild) -> None:
    observed = _command_process_identity(child.identity.pid)
    if (child.reaped or not child.group_leader
            or not _same_command_process(child.identity, observed) or observed is None
            or observed.pgrp != child.identity.pid or observed.session != child.identity.pid
            or observed.state == "X"):
        _signal_launch_child(child, signal.SIGKILL)
        if _wait_launch_fd(child.pidfd, 2):
            try:
                _reap_launch_child(child)
            except (ChildProcessError, OSError, RuntimeError, subprocess.SubprocessError):
                pass
        raise RuntimeError("chat launch bridge group identity changed before cleanup")


def _signal_launch_group(child: _LaunchChild, signum: int) -> None:
    _validated_launch_group(child)
    try:
        os.killpg(child.identity.pid, signum)
    except ProcessLookupError:
        pass


def _stop_launch_group(child: _LaunchChild) -> int:
    if child.reaped:
        return _reap_launch_child(child)
    if child.activation_write is not None:
        os.close(child.activation_write)
        child.activation_write = None
    ready = _wait_launch_fd(child.pidfd, 0)
    if not ready:
        _signal_launch_group(child, signal.SIGTERM)
        ready = _wait_launch_fd(child.pidfd, 10)
    # Retain the unreaped group leader as a numeric PGID pin while every
    # remaining same-group descendant receives the final bounded kill.
    _signal_launch_group(child, signal.SIGKILL)
    if not ready and not _wait_launch_fd(child.pidfd, 5):
        _signal_launch_child(child, signal.SIGKILL)
        if not _wait_launch_fd(child.pidfd, 2):
            raise RuntimeError("chat launch bridge did not exit after group SIGKILL")
    return _reap_launch_child(child)


def _close_launch_child(child: _LaunchChild) -> None:
    if child.activation_write is not None:
        os.close(child.activation_write)
        child.activation_write = None
    try:
        if not child.reaped:
            _signal_launch_child(child, signal.SIGKILL)
            if not _wait_launch_fd(child.pidfd, 2):
                raise RuntimeError(f"chat launch {child.name} survived final exact-handle cleanup")
            _reap_launch_child(child)
    finally:
        os.close(child.pidfd)


def _wait_launch_processes(
    bridge: _LaunchChild, coordinator: _LaunchChild, terminate_fd: int,
) -> tuple[str, int]:
    """Block on exact child-exit descriptors, preferring coordinator completion."""
    selector = selectors.DefaultSelector()
    try:
        selector.register(bridge.pidfd, selectors.EVENT_READ, "bridge")
        selector.register(coordinator.pidfd, selectors.EVENT_READ, "coordinator")
        selector.register(terminate_fd, selectors.EVENT_READ, "terminated")
        while True:
            ready = selector.select()
            names = {str(key.data) for key, _ in ready}
            if "terminated" in names:
                return "terminated", 0
            if "coordinator" in names:
                return "coordinator", _reap_launch_child(coordinator)
            if "bridge" in names:
                # The former 5 Hz supervisor observed exits in 200 ms batches
                # and checked the coordinator first. Preserve that terminal
                # coalescing envelope without polling while both are alive.
                selector.unregister(bridge.pidfd)
                trailing = selector.select(_LAUNCH_EXIT_COALESCE_SECONDS)
                trailing_names = {str(key.data) for key, _ in trailing}
                if "terminated" in trailing_names:
                    return "terminated", 0
                if "coordinator" in trailing_names:
                    return "coordinator", _reap_launch_child(coordinator)
                return "bridge", _peek_launch_status(bridge)
    finally:
        selector.close()


def _open_launch_log(path: Path) -> int:
    flags = (
        os.O_WRONLY | os.O_CREAT
        | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
    )
    descriptor = os.open(path, flags, 0o600)
    try:
        metadata = os.fstat(descriptor)
        if (not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid()
                or stat.S_IMODE(metadata.st_mode) & 0o077 or metadata.st_nlink != 1):
            raise ValueError(f"unsafe Chat launch log: {path}")
        # Validate first: O_TRUNC would damage a planted hard link before its
        # link count or permissions could be refused.
        os.ftruncate(descriptor, 0)
        return descriptor
    except BaseException:
        os.close(descriptor)
        raise


def _drain_launch_log(source: int, path: Path, failures: list[str]) -> None:
    """Drain a child pipe into two bounded one-MiB retained segments."""
    previous = path.with_name(path.name + ".1")
    destination = -1
    size = 0
    try:
        # Truncate both fixed names before accepting output, so legacy append-only
        # logs cannot survive a new launch as unaccounted disk.
        previous_descriptor = _open_launch_log(previous)
        os.close(previous_descriptor)
        destination = _open_launch_log(path)
        while True:
            block = os.read(source, 64 << 10)
            if not block:
                return
            offset = 0
            while offset < len(block):
                if size == _MAX_LAUNCH_LOG_SEGMENT_BYTES:
                    os.fsync(destination)
                    os.close(destination)
                    destination = -1
                    os.replace(path, previous)
                    _fsync_dir(str(path.parent))
                    destination = _open_launch_log(path)
                    size = 0
                count = min(len(block) - offset, _MAX_LAUNCH_LOG_SEGMENT_BYTES - size)
                written = os.write(destination, block[offset:offset + count])
                if written <= 0:
                    raise OSError("short Chat launch log write")
                offset += written
                size += written
    except (OSError, ValueError) as exc:
        failures.append(str(exc)[:2000])
        # Logging must never backpressure the bridge after a local file failure.
        try:
            while os.read(source, 64 << 10):
                pass
        except OSError:
            pass
    finally:
        if destination >= 0:
            try:
                os.fsync(destination)
            except OSError:
                pass
            os.close(destination)
        os.close(source)


def _read_launch_log_suffix(path: Path, limit: int) -> bytes:
    """Read at most the requested private-file suffix, never its full history."""
    try:
        descriptor = os.open(
            path, os.O_RDONLY | getattr(os, "O_CLOEXEC", 0)
            | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0),
        )
    except FileNotFoundError:
        return b""
    try:
        metadata = os.fstat(descriptor)
        if (not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid()
                or stat.S_IMODE(metadata.st_mode) & 0o077 or metadata.st_nlink != 1):
            raise ValueError(f"unsafe Chat launch log: {path}")
        count = min(limit, metadata.st_size)
        os.lseek(descriptor, metadata.st_size - count, os.SEEK_SET)
        data = bytearray()
        while len(data) < count:
            block = os.read(descriptor, count - len(data))
            if not block:
                break
            data.extend(block)
        return bytes(data)
    finally:
        os.close(descriptor)


def _launch_log_detail(path: Path, limit: int = 2000) -> str:
    current = _read_launch_log_suffix(path, limit)
    if len(current) < limit:
        previous = _read_launch_log_suffix(
            path.with_name(path.name + ".1"), limit - len(current))
        current = previous + current
    return current.decode("utf-8", errors="replace").strip()


def _launch_here(
    state: Path, config_path: Path, *, harness: str, model: str | None,
    resume: str | None, harness_args: Sequence[str], agent_label: str | None,
    after: str | None, interval: float, reconcile_interval: float, prog: str,
    observer_write_interval: float = _DEFAULT_OBSERVER_WRITE_INTERVAL,
) -> int:
    """Run one coordinator and its bridge for the lifetime of this Herdr pane."""
    _require_waitable_launch_children()
    previous = signal.getsignal(signal.SIGTERM)
    handle_signals = threading.current_thread() is threading.main_thread()
    termination_requested: list[int] = []
    def terminate(signum: int, frame: object) -> None:
        if not termination_requested:
            termination_requested.append(signum)
        try:
            os.write(terminate_write, b"1")
        except (BlockingIOError, OSError):
            pass
    document = read_json(config_path, CONFIG)[0]
    client = HerdrClient()
    config = _launch_config(document, client, harness=harness, model=model, agent_label=agent_label)
    command = [harness, *harness_arguments(harness, model=model, resume=resume, extra=harness_args)]
    Bridge.initialize(state, config, after=after)
    log_path = state.absolute() / "bridge.log"
    log_read, log_write = os.pipe2(os.O_CLOEXEC)
    log_failures: list[str] = []
    log_thread: threading.Thread | None = None
    try:
        terminate_read, terminate_write = os.pipe2(os.O_CLOEXEC | os.O_NONBLOCK)
    except BaseException:
        os.close(log_read)
        os.close(log_write)
        raise
    bridge_command = [sys.executable, str(Path(__file__).resolve()), "run", "--state", str(state.absolute()),
                      "--interval", str(interval), "--reconcile-interval", str(reconcile_interval),
                      "--observer-write-interval", str(observer_write_interval)]
    bridge: _LaunchChild | None = None
    coordinator: _LaunchChild | None = None
    result: int | None = None
    failure: BaseException | None = None
    cleanup_fault: BaseException | None = None
    try:
        if handle_signals:
            signal.signal(signal.SIGTERM, terminate)
        try:
            bridge = _spawn_launch_child(
                "bridge", bridge_command, group_leader=True,
                stdin=subprocess.DEVNULL, stdout=log_write, stderr=subprocess.STDOUT,
            )
            os.close(log_write)
            log_write = -1
            log_thread = threading.Thread(
                target=_drain_launch_log, args=(log_read, log_path, log_failures),
                name="chat-launch-log", daemon=True,
            )
            log_thread.start()
            log_read = -1
            if not termination_requested:
                _activate_launch_child(bridge)
            if not termination_requested:
                print(
                    f"{prog}: bridge state {state.absolute()} (log {log_path}); "
                    f"starting {shlex.join(command)}",
                    file=sys.stderr, flush=True,
                )
                coordinator = _spawn_launch_child(
                    "coordinator", command, group_leader=False,
                )
            if coordinator is not None and not termination_requested:
                _activate_launch_child(coordinator)
            if coordinator is not None and not termination_requested:
                completed, child_result = _wait_launch_processes(
                    bridge, coordinator, terminate_read,
                )
                if completed == "coordinator":
                    result = child_result
                elif completed == "bridge":
                    bridge_result = child_result
                    try:
                        detail = _launch_log_detail(log_path)
                    except (OSError, ValueError):
                        detail = ""
                    if not detail and log_failures:
                        detail = "launch log unavailable: " + log_failures[-1]
                    raise ValueError(
                        f"chat bridge exited {bridge_result} while the coordinator was running"
                        + (f": {detail}" if detail else ""),
                    )
                elif completed != "terminated":
                    raise ValueError("chat launch process supervisor returned an invalid child identity")
        except BaseException as exc:
            failure = exc
        finally:
            if coordinator is not None and not coordinator.reaped:
                try:
                    _stop_launch_child(coordinator)
                except BaseException as exc:
                    cleanup_fault = exc
            if bridge is not None and not bridge.reaped:
                try:
                    _stop_launch_group(bridge)
                except BaseException as exc:
                    if cleanup_fault is None:
                        cleanup_fault = exc
            for child in (coordinator, bridge):
                if child is not None:
                    try:
                        _close_launch_child(child)
                    except BaseException as exc:
                        if cleanup_fault is None:
                            cleanup_fault = exc
    finally:
        try:
            for descriptor in (log_write, log_read):
                if descriptor >= 0:
                    os.close(descriptor)
            if log_thread is not None:
                log_thread.join(timeout=5)
        finally:
            try:
                if handle_signals:
                    signal.signal(signal.SIGTERM, previous)
            finally:
                os.close(terminate_read)
                os.close(terminate_write)
    if cleanup_fault is not None:
        raise cleanup_fault
    if termination_requested:
        raise _ServiceTerminated
    if failure is not None:
        raise failure
    if result is None:
        raise RuntimeError("chat launch completed without an authoritative coordinator status")
    return result


def run_cli(argv: Sequence[str] | None = None, *, prog: str = "agentctl chat",
         default_state: Path = Path(".agentctl/.chat")) -> int:
    """Initialize, inspect, run, or reply through a coordinator bridge."""
    parser = argparse.ArgumentParser(
        prog=prog,
        description=(
            "Message a Codex or Claude coordinator in Herdr from one Google Chat space.\n"
            "Authorized messages enter a durable queue and receive a configurable reaction ACK\n"
            "(default: 🤖). Bracketed final replies are harvested from terminal output and sent durably."
        ),
    epilog=f"""Examples:
  {prog} launch --config chat.json --model gpt-6-astra
  {prog} init --config chat.json
  {prog} run --interval 10
  {prog} status
  {prog} history --request "$REQUEST_KEY"
  {prog} context --request "$REQUEST_KEY" --limit 10
  {prog} reply --request "$REQUEST_KEY" --file answer.txt

Use '{prog} quickstart' for setup, '{prog} userguide' for the full guide,
and '{prog} COMMAND --help' for command-specific options.
""",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        allow_abbrev=False,
    )
    parser.add_argument("--version", action="version", version=f"{prog} {__version__}",
                        help="print the installed version and exit")
    parser.add_argument("--userguide", action="store_true", help="print the complete guide and exit (alias for userguide)")
    parser.add_argument("--state", type=Path, default=default_state, metavar="DIR",
                        help=f"private bridge state directory (default: {default_state}); also accepted after the command")
    commands = parser.add_subparsers(dest="command", title="commands", metavar="COMMAND")
    descriptions = {
        "launch": "Launch a coordinator in this Herdr pane and bridge it to Chat for its lifetime.",
        "init": "Create private bridge state from configuration and check the pinned target.",
        "tick": "Poll one page, ACK accepted messages, deliver ready prompts, and inspect retained tagged replies once.",
        "run": "Stream Chat events when event_command is configured, otherwise poll; deliver prompts, ACKs, and tagged replies.",
        "status": "Print saved target, request phases, ACK attempts, and retry errors as JSON; no network or harness access.",
        "history": "Explicitly read all retained reply bodies for one request; cost is linear in its history.",
        "context": "Read a page of messages before a saved request in its thread; no live harness or state changes required.",
        "reply": "Durably queue one final answer from a UTF-8 file for the bridge to post in its originating thread.",
        "close": "Stop reply capture for one request without deleting its durable request or reply history.",
        "quickstart": "Print the shortest setup path and an example configuration.",
        "userguide": "Print the complete setup, authentication, adapter, and recovery guide.",
    }
    subparsers: dict[str, argparse.ArgumentParser] = {}
    examples = {"launch": "--config chat.json --model gpt-6-astra", "init": "--config chat.json", "tick": "", "run": "--interval 10", "status": "", "history": '--request "$REQUEST_KEY"',
                "reply": '--request "$REQUEST_KEY" --file answer.txt', "quickstart": "", "userguide": "",
                "context": '--request "$REQUEST_KEY" --limit 10', "close": '--request "$REQUEST_KEY"'}
    for name, description in descriptions.items():
        command = commands.add_parser(name, help=description, description=description,
            epilog=f"Example: {prog} {name} {examples[name]}".rstrip(), allow_abbrev=False)
        if name not in ("quickstart", "userguide"):
            command.add_argument("--state", type=Path, default=argparse.SUPPRESS, metavar="DIR",
                                 help=f"private bridge state directory (default: {default_state})")
        subparsers[name] = command
    subparsers["init"].add_argument("--config", type=Path, required=True, metavar="JSON_FILE",
        help="JSON configuration with the space, allowed senders, pinned target, and authentication")
    subparsers["init"].add_argument("--after", metavar="RFC3339_TIME",
        help="earliest message time, including timezone (default: current time; no history replay)")
    launch = subparsers["launch"]
    launch.add_argument("--config", type=Path, required=True, metavar="JSON_FILE",
        help="reusable Chat authority and transport config; launch supplies the current pane target")
    launch.add_argument("--harness", choices=("codex", "claude"), default="codex",
        help="native coordinator harness to run in this pane (default: codex)")
    launch.add_argument("--model", metavar="MODEL",
        help="native model name and default Chat reply label (default: harness configuration)")
    launch.add_argument("--resume", metavar="SESSION",
        help="resume an explicit native conversation instead of starting a new one")
    launch.add_argument("--harness-arg", action="append", default=[], metavar="ARG",
        help="literal harness argument; repeat and use = for flags")
    launch.add_argument("--agent-label", metavar="LABEL",
        help="Chat reply prefix label (default: model, config agent_label, or harness)")
    launch.add_argument("--after", metavar="RFC3339_TIME",
        help="earliest message time, including timezone (default: launch time; no history replay)")
    launch.add_argument("--interval", type=float, default=_DEFAULT_POLL_INTERVAL, metavar="SECONDS",
        help=("poll delay in seconds, 0.1–86400 (default: 3600); failures never shorten it; "
              "ignored by event-command intake"))
    launch.add_argument("--reconcile-interval", type=float, default=300, metavar="SECONDS",
        help="event-stream recovery scan interval, 10–86400 seconds (default: 300)")
    launch.add_argument("--observer-write-interval", type=float,
        default=_DEFAULT_OBSERVER_WRITE_INTERVAL, metavar="SECONDS",
        help=("minimum durable observer-state write interval, 60–3600 seconds (default: 60); "
              "cursor advances remain immediate and status flaps are coalesced"))
    subparsers["run"].add_argument("--interval", type=float, default=_DEFAULT_POLL_INTERVAL, metavar="SECONDS",
        help=("delay after each poll cycle in seconds, 0.1–86400 (default: 3600); used without "
              "event_command; failures double toward 60 seconds or one day and never shorten it"))
    subparsers["run"].add_argument("--reconcile-interval", type=float, default=300, metavar="SECONDS",
        help="REST recovery scan interval with event_command, 10–86400 seconds (default: 300); push messages never wait for this scan")
    subparsers["run"].add_argument("--observer-write-interval", type=float,
        default=_DEFAULT_OBSERVER_WRITE_INTERVAL, metavar="SECONDS",
        help=("minimum durable observer-state write interval, 60–3600 seconds (default: 60); "
              "cursor advances remain immediate and status flaps are coalesced"))
    subparsers["context"].add_argument("--request", required=True, metavar="KEY_OR_PREFIX",
        help="saved request's hexadecimal key or unique prefix of at least 12 characters; fixes thread and cutoff")
    subparsers["context"].add_argument("--limit", type=int, default=10, metavar="COUNT",
        help="maximum prior messages per page, 1–200 (default: 10); nearest prior messages, displayed chronologically")
    subparsers["context"].add_argument("--cursor", metavar="TOKEN",
        help="opaque cursor from the preceding context result to read older messages; keep request and limit unchanged")
    subparsers["reply"].add_argument("--request", required=True, metavar="KEY",
        help="64-character hexadecimal request key supplied in the coordinator's reply instructions")
    subparsers["reply"].add_argument("--file", type=Path, required=True, metavar="UTF8_FILE",
        help="file containing the final answer, 1–30000 UTF-8 bytes; identical retries are safe")
    subparsers["close"].add_argument("--request", required=True, metavar="KEY_OR_PREFIX",
        help="tagged request key or unique hexadecimal prefix; history is retained")
    subparsers["history"].add_argument("--request", required=True, metavar="KEY_OR_PREFIX",
        help="request key or unique hexadecimal prefix; reads every retained reply body")
    args = parser.parse_args(argv)
    if args.userguide or args.command == "userguide":
        from importlib.resources import files
        sys.stdout.write((files("agentctl") / "CHAT_USER_GUIDE.md").read_text(encoding="utf-8"))
        return 0
    if args.command == "quickstart":
        print(f"""Google Chat a coordinator running in Herdr

1. In Herdr, create a shell tab in the workspace where the coordinator and its
   subagents should live.
2. Configure user OAuth access to read/send Google Chat messages and create
   reactions. Set the access token in HERDR_CHAT_TOKEN, or use token_command.
3. Save reusable chat.json authority and transport settings:
   {{
     "space": "spaces/SPACE_ID",
     "allowed_senders": ["users/YOUR_USER_ID"],
     "agent_label": "codex-coordinator",
     "ack_reaction": "🤖"
   }}
   Make it private with: chmod 600 chat.json
4. From that shell tab, run one command:
   {prog} launch --config chat.json --model gpt-6-astra
   It discovers and pins the current pane, runs the coordinator there, and owns
   the bridge for the coordinator's lifetime. Its subagents default to the same
   Herdr workspace.
5. Send a message in the configured space. The reaction acknowledges durable
   intake; the agent brackets its final answer with the unique tags in its prompt.
   The daemon captures that block and posts it in the same thread. No reply tool
   call or file write is required from the coordinator.

Set ack_reaction to a different Unicode emoji, or null/"" to disable ACKs.
Optional reaction_user: "users/OAUTH_USER_ID" lets the public REST adapter
reconcile an existing reaction after a lost create response. This is the
credential's user, which can differ from an allowed sender.

The default reply_mode is "tagged". Set it to "file" for explicit reply files.
Set outbound_mode to "disabled" for durable inbound delivery with no reactions,
reply capture, reply artifacts, or Chat sends.
Herdr output subscriptions wake the bridge independently of Chat's poll interval;
Herdr 0.8 checks its match predicates internally every 100 milliseconds.
Thread replies include a command to read the prior ten messages when needed.

For push intake, configure event_command as an adapter argv array. It receives
one subscribe request and emits newline-delimited message/control events.
In this mode ACKs, prompts, and replies run independently; --reconcile-interval
(default 300 seconds) controls REST recovery checks, not message intake.
Use transport_socket for a persistent private Unix adapter, or transport_command
for a per-request command. The built-in REST transport needs neither.

While `launch` is active, inspect delivery and reaction retry errors from another
pane with '{prog} status'. For service-managed deployments, use the separate
'{prog} init' and '{prog} run'
commands. See '{prog} userguide' for exact scopes, lifecycle, and adapter protocols.""")
        return 0
    if args.command is None:
        parser.print_help()
        return 0
    try:
        if args.command == "launch":
            if not _MIN_POLL_INTERVAL <= args.interval <= _MAX_POLL_INTERVAL:
                raise ValueError("interval must be between 0.1 and 86400 seconds")
            if not 10 <= args.reconcile_interval <= 86400:
                raise ValueError("reconcile interval must be between 10 and 86400 seconds")
            if not _MIN_OBSERVER_WRITE_INTERVAL <= args.observer_write_interval <= _MAX_OBSERVER_WRITE_INTERVAL:
                raise ValueError("observer write interval must be between 60 and 3600 seconds")
            return _launch_here(args.state, args.config, harness=args.harness, model=args.model,
                                resume=args.resume, harness_args=args.harness_arg,
                                agent_label=args.agent_label, after=args.after, interval=args.interval,
                                reconcile_interval=args.reconcile_interval, prog=prog,
                                observer_write_interval=args.observer_write_interval)
        if args.command == "init":
            config = Config.parse(read_json(args.config, CONFIG)[0])
            client = HerdrClient()
            resolve_target(_NamedClient(client, config.agent_name) if config.agent_name else client, config.target)
            Bridge.initialize(args.state, config, after=args.after)
            print(json.dumps({"state": str(args.state.absolute()), "space": config.space}))
        elif args.command == "reply":
            submit_reply(args.state, args.request, read_text(args.file, REPLY_INPUT))
            print(json.dumps({"outcome": "reply_queued", "request": args.request}))
        elif args.command == "close":
            key = close_replies(args.state, args.request)
            print(json.dumps({"outcome": "reply_capture_closed", "request": key}))
        elif args.command == "history":
            print(json.dumps(read_reply_history(args.state, args.request), indent=2))
        elif args.command == "context":
            from agentctl.chat_context import read_context
            from agentctl.chat_socket import SocketTransport
            config = Config.parse(as_mapping(_read(args.state / "bridge.json")["config"], "saved config"))
            path = _request_path(args.state, args.request)
            record = _read(path)
            if record.get("key") != path.stem:
                raise ValueError("saved request key does not match its file")
            source = as_mapping(record.get("message"), "saved source message")
            if not get_str(source, "id", "saved source message").startswith(config.space + "/messages/"):
                raise ValueError("saved request belongs to a different configured space")
            transport: Transport = (SocketTransport(config.transport_socket) if config.transport_socket
                                    else CommandTransport(config.transport_command) if config.transport_command
                                    else GoogleChatTransport(config.token_env, config.token_command, config.reaction_user))
            print(json.dumps(read_context(transport, source, limit=args.limit, cursor=args.cursor), indent=2))
        else:
            bridge = Bridge(args.state)
            if args.command == "status":
                print(json.dumps(bridge.status(), indent=2))
            elif args.command == "tick":
                descriptor = _open_private_lock(str(bridge.state / ".run.lock"), "chat runner lock")
                try:
                    fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    _audit_chat_temporaries(bridge.state, descriptor)
                    bridge.tick()
                    bridge.capture_once()
                finally:
                    os.close(descriptor)
                print(json.dumps(bridge.status(), indent=2))
            else:
                if not _MIN_POLL_INTERVAL <= args.interval <= _MAX_POLL_INTERVAL:
                    raise ValueError("interval must be between 0.1 and 86400 seconds")
                if not 10 <= args.reconcile_interval <= 86400:
                    raise ValueError("reconcile interval must be between 10 and 86400 seconds")
                if not _MIN_OBSERVER_WRITE_INTERVAL <= args.observer_write_interval <= _MAX_OBSERVER_WRITE_INTERVAL:
                    raise ValueError("observer write interval must be between 60 and 3600 seconds")
                _run_bridge(bridge, args.interval, prog, reconcile_interval=args.reconcile_interval,
                            observer_write_interval=args.observer_write_interval)
        return 0
    except (OSError, ValueError, TypeError, HerdrRunError, subprocess.SubprocessError) as exc:
        print(f"{prog}: {exc}", file=sys.stderr)
        return 1
    except _ServiceTerminated:
        return 0
    except KeyboardInterrupt:
        return 130


def main(argv: Sequence[str] | None = None) -> int:
    """Compatibility entry point retaining herdr-chat's state directory."""
    return run_cli(argv, prog="herdr-chat", default_state=Path(".herdr-chat"))


if __name__ == "__main__":
    raise SystemExit(main() if Path(sys.argv[0]).name == "herdr-chat" else run_cli())
