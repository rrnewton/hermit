"""Push intake with independent acknowledgement, harness, and reply workers.

Only the event-loop owner mutates request records. Blocking network operations
return results to that owner; the harness worker owns only the existing durable
delivery queue. A saved stream cursor follows durable intake, never a pipe read.
"""

from __future__ import annotations

import hashlib
import fcntl
import os
import queue
import re
import socket
import stat
import sys
import threading
import time
from collections import Counter
from collections.abc import Callable
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path

from agentctl.agent import _fsync_dir, _open_private_lock, drain, enqueue, resolve_target
from agentctl.chat import (
    _DEFAULT_OBSERVER_WRITE_INTERVAL, _MAX_QUEUE_ARTIFACT_BYTES, Bridge, _OutputObserver,
    _OutputSubscription, _chat_atomic_policy, _cursor, _private, _read,
    _timestamp, _utc, _write,
)
from agentctl.chat_input import EventCommandStream
from agentctl.chat_output import PaneAgentStatus, PaneOutputSnapshot, PaneOutputStream
from agentctl.errors import HerdrUnavailable
from agentctl.jsonx import as_mapping, as_sequence, get_str


_MAX_RECONCILE_PAGES = 64
_MAX_RECONCILE_SECONDS = 60.0
_RECONCILE_HEALTH_WRITE_INTERVAL = 3600.0
_SENDABLE_REPLY_PHASES = frozenset({
    "awaiting_reply", "delivery_uncertain", "reply_pending", "replied",
})


@dataclass(frozen=True)
class _Notice:
    kind: str
    value: object = None


@dataclass(frozen=True)
class _Completion:
    lane: str
    key: str
    result: object
    error: Exception | None
    finished_at: str


@dataclass
class _ReconcileWindow:
    started_at: float
    pages: int
    cursors: set[str]


class _InputObserver:
    """Persist cursors immediately and coalesce advisory connection state."""

    def __init__(self, path: Path, document: dict[str, object], write_interval: float) -> None:
        self.path = path
        self.document = document
        self.write_interval = write_interval
        self.reconcile_health_write_interval = _RECONCILE_HEALTH_WRITE_INTERVAL
        self._persisted = self._logical(document)
        self._persisted_reconciled_at = document.get("reconciled_at")
        self._pending = False
        self._next_write: float | None = None
        self._next_health_write: float | None = None
        updated_at = document.get("updated_at")
        if path.exists() and isinstance(updated_at, str):
            try:
                age = (_timestamp(_utc()) - _timestamp(updated_at)).total_seconds()
            except (TypeError, ValueError):
                age = write_interval
            self._next_write = time.monotonic() + max(0.0, write_interval - max(0.0, age))
            if self._valid_reconciled_at(self._persisted_reconciled_at):
                # A future durable anchor means the wall clock moved backward.
                # Wait a full monotonic interval rather than allowing clock
                # oscillation to turn each reconciliation into a disk write.
                health_age = max(0.0, age)
                self._next_health_write = time.monotonic() + max(
                    0.0, self.reconcile_health_write_interval - health_age)

    @staticmethod
    def _valid_reconciled_at(value: object) -> bool:
        if not isinstance(value, str):
            return False
        try:
            _timestamp(value)
        except (TypeError, ValueError):
            return False
        return True

    @staticmethod
    def _logical(document: dict[str, object]) -> tuple[object, object, object]:
        return (document.get("state"), document.get("error"),
                document.get("reconcile_error"))

    @property
    def next_write(self) -> float | None:
        return self._next_write if self._pending else None

    def _persist(self, candidate: dict[str, object], now: float) -> None:
        saved = dict(candidate, updated_at=_utc())
        _write(self.path, saved)
        self.document.clear()
        self.document.update(saved)
        self._persisted = self._logical(self.document)
        self._persisted_reconciled_at = self.document.get("reconciled_at")
        self._pending = False
        self._next_write = now + self.write_interval
        self._next_health_write = (
            now + self.reconcile_health_write_interval
            if self._valid_reconciled_at(self._persisted_reconciled_at)
            else None)

    def status(self, **fields: object) -> bool:
        candidate = dict(self.document)
        candidate.update(fields)
        logical = self._logical(candidate)
        now = time.monotonic()
        if not self.path.exists():
            self._persist(candidate, now)
            return True
        reconciled_at = candidate.get("reconciled_at")
        health_changed = reconciled_at != self._persisted_reconciled_at
        if (health_changed and self._valid_reconciled_at(reconciled_at)
                and (self._next_health_write is None or now >= self._next_health_write)):
            # First successful health must survive a crash even when it also
            # clears a recently persisted reconciliation error. Later due
            # health writes likewise fold any pending advisory state.
            self._persist(candidate, now)
            return True
        if logical == self._persisted:
            if health_changed and not self._valid_reconciled_at(reconciled_at):
                self._persist(candidate, now)
                return True
            self.document.update(fields)
            self._pending = False
            return False
        if self._next_write is None or now >= self._next_write:
            self._persist(candidate, now)
            return True
        self.document.update(fields)
        self._pending = True
        return False

    def commit(self, **fields: object) -> None:
        """Durably advance a replay cursor, irrespective of advisory rate limits."""
        candidate = dict(self.document)
        candidate.update(fields)
        self._persist(candidate, time.monotonic())

    def flush(self) -> bool:
        if not self._pending or self._next_write is None:
            return False
        now = time.monotonic()
        if now < self._next_write:
            return False
        self._persist(self.document, now)
        return True


def _post(events: queue.Queue[_Notice], notice: _Notice, stop: threading.Event) -> None:
    while not stop.is_set():
        try:
            events.put(notice, timeout=0.1)
            return
        except queue.Full:
            continue


def _validate_input_event(event: dict[str, object]) -> tuple[str, str | None]:
    """Validate one exact event shape before filtering or cursor persistence."""
    kind = event.get("type")
    schemas: dict[str, tuple[set[str], set[str]]] = {
        "message": ({"type", "message"}, {"type", "message", "cursor"}),
        "heartbeat": ({"type"}, {"type", "cursor"}),
        "checkpoint": ({"type", "cursor"}, {"type", "cursor"}),
        "gap": ({"type"}, {"type", "cursor", "reason"}),
    }
    if not isinstance(kind, str) or kind not in schemas:
        raise ValueError("stream event type must be message, heartbeat, checkpoint, or gap")
    required, allowed = schemas[kind]
    missing = required - set(event)
    extra = set(event) - allowed
    if missing:
        raise ValueError(
            f"{kind} stream event is missing required fields: " + ", ".join(sorted(missing)))
    if extra:
        if len(extra) == 1:
            field = next(iter(extra))
            detail = field if len(field.encode("utf-8")) <= 128 else "one oversized field"
        else:
            detail = f"{len(extra)} fields"
        raise ValueError(
            f"{kind} stream event contains unsupported fields: " + detail)
    if kind == "message":
        as_mapping(event.get("message"), "stream message")
    reason = event.get("reason")
    if "reason" in event and (
        not isinstance(reason, str) or not reason or len(reason.encode("utf-8")) > 2000
    ):
        raise ValueError("stream gap reason must contain 1-2000 UTF-8 bytes")
    cursor = event.get("cursor")
    if "cursor" in event and cursor is None:
        raise ValueError("stream cursor must contain 1-8192 UTF-8 bytes")
    if cursor is not None and (
        not isinstance(cursor, str) or not cursor or len(cursor.encode("utf-8")) > 8192
    ):
        raise ValueError("stream cursor must contain 1-8192 UTF-8 bytes")
    return kind, cursor


class _InputPump:
    def __init__(self, bridge: Bridge, events: queue.Queue[_Notice], stop: threading.Event) -> None:
        self.bridge, self.events, self.stop = bridge, events, stop
        self.stream: EventCommandStream | None = None
        self.thread = threading.Thread(target=self._run, name="chat-input", daemon=True)

    def _run(self) -> None:
        delay = 1.0
        while not self.stop.is_set():
            try:
                path = self.bridge.state / "input.json"
                cursor = _cursor(
                    _read(path).get("cursor") if path.exists() else None,
                    "saved stream cursor",
                )
                self.stream = EventCommandStream(self.bridge.config.event_command,
                    {"action": "subscribe", "space": self.bridge.config.space, "cursor": cursor})
                _post(self.events, _Notice("input_connected"), self.stop)
                connected = time.monotonic()
                generation_cursor = cursor
                while not self.stop.is_set():
                    for event in self.stream.wait(30):
                        kind, event_cursor = _validate_input_event(event)
                        if (kind == "heartbeat"
                                and (event_cursor is None or event_cursor == generation_cursor)):
                            continue
                        _post(self.events, _Notice("input", event), self.stop)
                        if isinstance(event_cursor, str) and event_cursor:
                            generation_cursor = event_cursor
                    if time.monotonic() - connected >= 30:
                        delay = 1.0
            except Exception as exc:
                _post(self.events, _Notice("input_error", str(exc)), self.stop)
            finally:
                if self.stream is not None:
                    self.stream.close()
                    self.stream = None
            self.stop.wait(delay)
            delay = min(60, delay * 2)

    def close(self) -> None:
        stream = self.stream
        if stream is not None:
            stream.wake()
        self.thread.join(timeout=10)


class _LocalWakePump:
    """Wake the durable owner after another process commits a reply artifact."""

    def __init__(self, state: Path, events: queue.Queue[_Notice], stop: threading.Event) -> None:
        self.path = state / ".wake.sock"
        try:
            metadata = self.path.lstat()
        except FileNotFoundError:
            pass
        else:
            if not stat.S_ISSOCK(metadata.st_mode) or metadata.st_uid != os.getuid():
                raise ValueError("chat wake endpoint is not a socket owned by this account")
            self.path.unlink()
        self.socket = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
        self.socket.bind(str(self.path))
        os.chmod(self.path, 0o600)
        metadata = self.path.lstat()
        self.identity = (metadata.st_dev, metadata.st_ino)
        self.events, self.stop = events, stop
        self.thread = threading.Thread(target=self._run, name="chat-local-wake", daemon=True)

    def _run(self) -> None:
        while not self.stop.is_set():
            try:
                frame = self.socket.recv(256)
            except OSError:
                return
            if not frame:
                continue
            try:
                text = frame.decode("ascii")
            except UnicodeError:
                continue
            if re.fullmatch(r"reply:[0-9a-f]{64}", text):
                _post(self.events, _Notice("local_reply", text[6:]), self.stop)

    def close(self) -> None:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as wake:
                wake.setblocking(False)
                wake.sendto(b"", str(self.path))
        except OSError:
            pass
        if self.thread.ident is not None:
            self.thread.join(timeout=10)
        self.socket.close()
        try:
            metadata = self.path.lstat()
            if (metadata.st_dev, metadata.st_ino) == self.identity:
                self.path.unlink()
        except FileNotFoundError:
            pass


class _OutputPump:
    def __init__(self, bridge: Bridge, events: queue.Queue[_Notice], stop: threading.Event) -> None:
        self.bridge, self.events, self.stop = bridge, events, stop
        self.desired: _OutputSubscription | None = None
        self.changed = threading.Event()
        self.stream: PaneOutputStream | None = None
        self.thread = threading.Thread(target=self._run, name="chat-output", daemon=True)

    def update(self, desired: _OutputSubscription) -> None:
        if desired != self.desired:
            self.desired = desired
            self.changed.set()
            stream = self.stream
            if stream is not None:
                stream.wake()

    def _run(self) -> None:
        subscribed: _OutputSubscription | None = None
        delay = 1.0
        try:
            while not self.stop.is_set():
                self.changed.clear()
                desired = self.desired
                subscription_changed = desired != subscribed
                if subscription_changed:
                    if self.stream is not None:
                        self.stream.close()
                    self.stream = None
                    subscribed = desired
                    delay = 1.0
                if desired is None or not desired.watching:
                    if subscription_changed:
                        _post(self.events, _Notice("output_idle"), self.stop)
                    self.changed.wait(30)
                    continue
                try:
                    if self.stream is None:
                        self.stream = self.bridge._open_cached_output(desired)
                        _post(self.events, _Notice("output_connected"), self.stop)
                    if self.stop.is_set() or self.changed.is_set():
                        continue
                    for event in self.stream.wait(30):
                        _post(self.events, _Notice("output", event), self.stop)
                    delay = 1.0
                except Exception as exc:
                    _post(self.events, _Notice("output_error", str(exc)), self.stop)
                    if self.stream is not None:
                        self.stream.close()
                    self.stream = None
                    self.changed.wait(delay)
                    delay = min(60, delay * 2)
        finally:
            if self.stream is not None:
                self.stream.close()
                self.stream = None

    def close(self) -> None:
        self.changed.set()
        stream = self.stream
        if stream is not None:
            stream.wake()
        self.thread.join(timeout=10)


class _Runtime:
    def __init__(self, bridge: Bridge, reconcile_interval: float, prog: str,
                 stop: threading.Event,
                 observer_write_interval: float = _DEFAULT_OBSERVER_WRITE_INTERVAL) -> None:
        self.bridge, self.prog, self.stop = bridge, prog, stop
        self.reconcile_interval = reconcile_interval
        self.input_path = bridge.state / "input.json"
        self.input_state: dict[str, object] = (
            _read(self.input_path) if self.input_path.exists() else {"cursor": None})
        _cursor(self.input_state.get("cursor"), "saved stream cursor")
        self.events: queue.Queue[_Notice] = queue.Queue(maxsize=64)
        self.input = _InputPump(bridge, self.events, stop)
        self.output = _OutputPump(bridge, self.events, stop)
        self.output_observer = _OutputObserver(bridge.state / "output.json",
                                              observer_write_interval)
        self.pools = {"ack": ThreadPoolExecutor(4, "chat-ack"),
                      "send": ThreadPoolExecutor(2, "chat-send"),
                      "drain": ThreadPoolExecutor(1, "chat-delivery"),
                      "output": ThreadPoolExecutor(2, "chat-inspect"),
                      "poll": ThreadPoolExecutor(1, "chat-reconcile")}
        self.jobs: set[tuple[str, str]] = set()
        self.retry: dict[tuple[str, str], tuple[float, float]] = {}
        self.next_poll = 0.0
        self.next_drain = 0.0
        self.delivery_tentative: set[str] = set()
        self.delivery_prompts: dict[str, str] = {}
        self.delivery_inflight: dict[str, str] = {}
        self.poll_checkpoint: dict[str, object] | None = None
        self.reconcile_window: _ReconcileWindow | None = None
        self.reconcile_requested = True
        self.output_pending: dict[str, PaneOutputSnapshot | PaneAgentStatus] = {}
        self.output_inflight: dict[str, PaneOutputSnapshot | PaneAgentStatus] = {}
        self.send_inflight: dict[str, str] = {}
        self.send_cursor: str | None = None
        self.input_observer = _InputObserver(self.input_path, self.input_state,
                                             observer_write_interval)
        self.deferred = bridge.state / "deferred"
        _private(self.deferred)
        self.records: dict[str, tuple[Path, dict[str, object]]] = {}
        self.record_order: list[str] = []
        self.pending_texts: Counter[str] = Counter()
        self.pending_by_key: dict[str, Counter[str]] = {}
        self.reply_items_by_key: dict[str, list[dict[str, object]]] = {}
        self.reply_ids: Counter[str] = Counter()
        self.reply_ids_by_key: dict[str, set[str]] = {}
        self.deferred_messages: dict[Path, dict[str, object]] = {}
        self.feedback_pending: dict[str, tuple[str, bool]] = {}
        self.feedback_dirty = False
        self.deferred_dirty = False
        self._reload_records()
        self.recovery_interval = 300.0
        self.next_recovery = time.monotonic() + self.recovery_interval
        self.delivery_pending = False
        self.local = _LocalWakePump(bridge.state, self.events, stop)

    def _start(self, lane: str, key: str, operation: Callable[[], object]) -> None:
        self.jobs.add((lane, key))
        def execute() -> None:
            result: object = None
            error: Exception | None = None
            try:
                result = operation()
            except Exception as exc:
                error = exc
            _post(self.events, _Notice("complete", _Completion(lane, key, result, error, _utc())), self.stop)
        # Always post from the worker. add_done_callback may run immediately on
        # this owner thread, deadlocking it when the bounded mailbox is full.
        self.pools[lane].submit(execute)

    def _transport(self, lane: str, key: str, request: dict[str, object]) -> None:
        def operation() -> object:
            if lane == "send":
                resolve_target(self.bridge.client, self.bridge.config.target)
            return self.bridge._transport_request(request)
        self._start(lane, key, operation)

    def _input_status(self, **fields: object) -> None:
        self.input_observer.status(**fields)

    def _log(self, message: str) -> None:
        print(f"{self.prog}: {message}", file=sys.stderr, flush=True)

    def _records(self) -> list[tuple[Path, dict[str, object]]]:
        return [self.records[key] for key in self.record_order]

    def _send_candidate(self, key: str) -> bool:
        cached = self.records.get(key)
        return bool(
            self.bridge.config.outbound_mode == "enabled"
            and cached is not None
            and cached[1].get("phase") in _SENDABLE_REPLY_PHASES
            and self.pending_by_key.get(key)
            and ("send", key) not in self.jobs
        )

    def _send_ready(self, key: str, now: float) -> bool:
        if not self._send_candidate(key):
            return False
        retry = self.retry.get(("send", key))
        return retry is None or now >= retry[0]

    def _ordered_record_keys(self, now: float, keys: set[str] | None = None) -> list[str]:
        """Prefer fresh durable sends, round-robin after the last started key."""
        order = self.record_order
        if self.send_cursor in order:
            offset = order.index(self.send_cursor) + 1
            order = order[offset:] + order[:offset]
        selected = order if keys is None else [key for key in order if key in keys]

        def send_rank(key: str) -> int:
            if not self._send_candidate(key):
                return 2
            retry = self.retry.get(("send", key))
            if retry is None:
                return 0
            return 1 if now >= retry[0] else 2

        buckets: tuple[list[str], list[str], list[str]] = ([], [], [])
        for key in selected:
            buckets[send_rank(key)].append(key)
        return [key for bucket in buckets for key in bucket]

    def _reload_records(self) -> None:
        # Admit bounded requests one at a time, then derive all Q/F/D indexes
        # from one stable auxiliary scan. Lock order is bridge -> delivery;
        # the queue worker never acquires the bridge lock.
        descriptor = _open_private_lock(
            str(self.bridge.state / ".bridge.lock"), "chat bridge lock")
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX)
            loaded = self.bridge._load_request_records()
            snapshot = self.bridge.validate_aux_snapshot(records=loaded)
            # A worker may still be waiting to enqueue an owner-registered
            # prompt. The stable disk snapshot cannot include it yet; preserve
            # its bounded in-flight authority before processing a fast echo.
            for identifier, prompt in self.delivery_inflight.items():
                self.bridge._remember_prompt(identifier, prompt)
            records = [(self.bridge.state / "requests" / (get_str(record, "key", "request") + ".json"), record)
                       for record in loaded]
            records.sort(key=lambda item: get_str(item[1], "received_at", "request"))
            self.bridge._rebuild_reply_usage([record for _, record in records])
            recovered: dict[str, list[dict[str, object]]] = {}
            for path, record in records:
                key = get_str(record, "key", "request")
                items = self.bridge._recover_reply_state(path, record)
                self.bridge._adopt_submission(path, record, items)
                recovered[key] = items
        finally:
            os.close(descriptor)
        self.records = {get_str(record, "key", "request"): (path, record)
                        for path, record in records}
        self.record_order = [get_str(record, "key", "request") for _, record in records]
        self.pending_texts.clear()
        self.pending_by_key.clear()
        self.reply_items_by_key = recovered
        self.reply_ids.clear()
        self.reply_ids_by_key.clear()
        for key in self.record_order:
            self._refresh_reply_index(key, recovered[key])
        self.feedback_pending = dict(snapshot.feedback_pending)
        self.feedback_dirty = bool(self.feedback_pending)
        self.deferred_messages = dict(snapshot.deferred_messages)
        self.deferred_dirty = bool(self.deferred_messages)

    def _admit_reconciled_records(
        self, admitted: list[tuple[Path, dict[str, object]]],
    ) -> set[str]:
        """Recover and cache only records durably admitted by one poll page."""
        if not admitted:
            return set()
        descriptor = _open_private_lock(
            str(self.bridge.state / ".bridge.lock"), "chat bridge lock")
        selected: set[str] = set()
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX)
            for path, expected in admitted:
                record, _ = self.bridge._read_bounded_record(path)
                self.bridge._validate_request_record_schema(record)
                if (get_str(record, "key", "request") != get_str(expected, "key", "request")
                        or as_mapping(record.get("message"), "saved source message")
                        != as_mapping(expected.get("message"), "admitted source message")):
                    raise ValueError("admitted request identity changed before cache admission")
                items = self.bridge._recover_reply_state(path, record)
                self.bridge._adopt_submission(path, record, items)
                key = get_str(record, "key", "request")
                self._cache_record(path, record)
                self._refresh_reply_index(key, items)
                selected.add(key)
        finally:
            os.close(descriptor)
        return selected

    def _defer_reconciliation(self, reason: str, now: float) -> None:
        """End one bounded page window and retry it with ordinary backoff."""
        job = ("poll", "page")
        delay = min(60, self.retry.get(job, (0, 0.5))[1] * 2)
        self.retry[job] = (now + delay, delay)
        self.reconcile_window = None
        self.poll_checkpoint = None
        self.reconcile_requested = True
        self._input_status(reconcile_error=reason[:2000])
        self._log(f"poll: {reason}")

    def _start_reconciliation_page(self) -> None:
        now = time.monotonic()
        window = self.reconcile_window
        if window is not None:
            if window.pages >= _MAX_RECONCILE_PAGES:
                self._defer_reconciliation(
                    f"reconciliation page budget {_MAX_RECONCILE_PAGES} exhausted", now)
                return
            if now - window.started_at >= _MAX_RECONCILE_SECONDS:
                self._defer_reconciliation(
                    f"reconciliation time budget {_MAX_RECONCILE_SECONDS:g}s exhausted", now)
                return
        checkpoint = _read(self.bridge.state / "bridge.json")
        if window is None:
            initial = _cursor(checkpoint.get("cursor"), "saved poll cursor")
            window = _ReconcileWindow(
                started_at=now, pages=0,
                cursors=set() if initial is None else {initial},
            )
            self.reconcile_window = window
        self.poll_checkpoint = checkpoint
        self.reconcile_requested = False
        self._transport("poll", "page", self.bridge._poll_request(checkpoint))

    def _cache_record(self, path: Path, record: dict[str, object]) -> None:
        key = get_str(record, "key", "request")
        if key not in self.records:
            self.records[key] = (path, record)
            received = get_str(record, "received_at", "request")
            index = len(self.record_order)
            while index > 0:
                prior = self.records[self.record_order[index - 1]][1]
                if get_str(prior, "received_at", "request") <= received:
                    break
                index -= 1
            self.record_order.insert(index, key)
        else:
            self.records[key] = (path, record)

    def _refresh_reply_index(
        self, key: str, items: list[dict[str, object]] | None = None,
    ) -> list[dict[str, object]]:
        prior = self.pending_by_key.pop(key, Counter())
        for text, count in prior.items():
            remaining = self.pending_texts[text] - count
            if remaining > 0:
                self.pending_texts[text] = remaining
            else:
                del self.pending_texts[text]
        for identifier in self.reply_ids_by_key.pop(key, set()):
            self.reply_ids[identifier] -= 1
            if not self.reply_ids[identifier]:
                del self.reply_ids[identifier]
        cached = self.records.get(key)
        if cached is None:
            self.reply_items_by_key.pop(key, None)
            return []
        if self.bridge.config.outbound_mode == "disabled":
            self.reply_items_by_key[key] = []
            return []
        if items is None:
            items = self.reply_items_by_key.get(key, [])
        pending: Counter[str] = Counter()
        for item in items:
            pending[self._outbound_text(item)] += 1
        self.pending_by_key[key] = pending
        self.reply_ids_by_key[key] = set()
        self.pending_texts.update(pending)
        self.reply_items_by_key[key] = items
        return items

    def _outbound_text(self, reply: dict[str, object]) -> str:
        text = get_str(reply, "text", "reply")
        prefix = f"[{self.bridge.config.agent_label}]"
        return text if text.startswith(prefix) else f"{prefix} {text}"

    def _possible_echo(self, message: dict[str, object]) -> bool:
        if message.get("sender") not in self.bridge.config.allowed_sender_set:
            return False
        return message.get("text") in self.pending_texts

    def _accept_message(self, message: dict[str, object]) -> Path | None:
        self.bridge._validate_message(message)
        identifier = get_str(message, "id", "stream message")
        if re.fullmatch(re.escape(self.bridge.config.space) + r"/messages/[A-Za-z0-9_.-]+", identifier) is None:
            raise ValueError("stream message belongs to another space")
        key = hashlib.sha256(identifier.encode()).hexdigest()
        accepted: Path | None = None
        if self._possible_echo(message):
            # Deferral also commits input before its stream cursor advances.
            # Require the same normalized schema as ordinary accepted messages.
            self.bridge._validate_message_source(message)
            source_bytes = self.bridge._validate_message_content(message)
            deferred_path = self.deferred / f"{key}.json"
            # A prior rename may have committed before its fsync failed. Even
            # an existing-path replay must repair invalidated byte/count usage.
            usage = self.bridge._aux_usage or self.bridge.validate_aux_population()
            if deferred_path.exists():
                if _read(deferred_path) != message:
                    raise ValueError("replayed deferred message changed its durable content")
            else:
                proposed = dict(usage)
                proposed["deferred_records"] += 1
                proposed["deferred_bytes"] += source_bytes
                reason = self.bridge._aux_limit_reason(proposed)
                if reason is not None:
                    self.bridge._record_aux_limit(reason, proposed)
                    raise ValueError(
                        reason + "; rotate to a fresh Chat state after draining accepted work")
                try:
                    _write(deferred_path, message)
                except BaseException:
                    self.bridge._aux_usage = None
                    raise
                self.bridge._aux_usage = proposed
            self.deferred_messages[deferred_path] = message
            self.deferred_dirty = True
        else:
            path = self.bridge.state / "requests" / f"{key}.json"
            existed = path.exists()
            self.bridge._ingest_result({"messages": [message]},
                                       records=[record for _, record in self._records()])
            if not existed and path.exists():
                accepted = path
        self.next_drain = 0.0
        return accepted

    def _input_event(self, event: dict[str, object]) -> tuple[str, str | None]:
        kind, cursor = _validate_input_event(event)
        if kind == "message":
            path = self._accept_message(as_mapping(event.get("message"), "stream message"))
            if path is not None:
                # The request is durable. Start its independent workers before
                # cursor persistence can stall; replay retains this request and
                # the delivery queue's existing identities after a crash.
                record = _read(path)
                self._cache_record(path, record)
                accepted_key = get_str(record, "key", "request")
                now = time.monotonic()
                pending, prompt = self._stage_request(path, record, now)
                self._start_delivery(pending, [] if prompt is None else [prompt], now)
            else:
                accepted_key = None
        elif kind == "gap":
            self.reconcile_requested = True
            accepted_key = None
        else:
            accepted_key = None
        # Any message is durable (or excluded by configured authority) before its cursor advances.
        fields: dict[str, object] = {"state": "connected", "error": None}
        if cursor is not None:
            fields["cursor"] = cursor
        if kind == "message":
            fields.update(last_event_at=_utc(), last_message_at=_utc())
        if kind == "gap":
            fields.update(last_event_at=_utc(), last_gap_at=_utc())
        if cursor is not None and cursor != self.input_state.get("cursor"):
            self.input_observer.commit(**fields)
        elif kind != "heartbeat":
            self._input_status(**fields)
        return str(kind), accepted_key

    def _stage_request(self, path: Path, record: dict[str, object],
                       now: float) -> tuple[bool, tuple[str, str] | None]:
        queue_root = self.bridge.state / "queue"
        pending = False
        prompt: tuple[str, str] | None = None
        key = get_str(record, "key", "request")
        queue_id = get_str(record, "queue_id", "request")
        changed = False
        # A local wake normally selects this key immediately. Checking its one
        # exact submission path also closes the narrow startup/test race without
        # scanning any directory or reply history.
        submission = self.bridge.state / "submissions" / f"{key}.json"
        if submission.exists():
            descriptor = _open_private_lock(
                str(self.bridge.state / ".bridge.lock"), "chat bridge lock")
            try:
                fcntl.flock(descriptor, fcntl.LOCK_EX)
                record = _read(path)
                items = self.reply_items_by_key.get(key, [])
                self.bridge._adopt_submission(path, record, items)
            finally:
                os.close(descriptor)
            self._cache_record(path, record)
            self._refresh_reply_index(key, items)
        if record["phase"] == "received":
            if any((queue_root / phase / f"{queue_id}.json").exists()
                   for phase in ("inbox", "inflight", "processed", "failed")):
                record.update(phase="queued", queued_at=_utc())
                changed = True
            else:
                prompt = (queue_id, self.bridge._prompt(record))
                pending = True
        if record["phase"] == "queued":
            if (queue_root / "processed" / f"{queue_id}.json").exists():
                record.update(phase="awaiting_reply", delivery_confirmed_at=_utc())
                changed = True
            elif (queue_root / "failed" / f"{queue_id}.json").exists():
                record["phase"] = "delivery_uncertain"
                changed = True
            else:
                pending = True
        if "ack" not in record:
            source = as_mapping(record["message"], "source")
            record["ack"] = self.bridge._ack_record(get_str(source, "id", "source"),
                                                    completed_legacy=record["phase"] == "replied")
            changed = True
        ack = as_mapping(record["ack"], "ack")
        if self.bridge.config.outbound_mode == "disabled" and (
            ack.get("state") != "disabled" or ack.get("emoji") is not None
            or ack.get("error") is not None or ack.get("next_retry_at") is not None
        ):
            ack.update(state="disabled", emoji=None, error=None, next_retry_at=None)
            record["ack"] = ack
            changed = True
        if changed:
            _write(path, record)
        retry = ack.get("next_retry_at")
        due = not isinstance(retry, str) or _timestamp(retry) <= datetime.now(timezone.utc)
        if (self.bridge.config.outbound_mode == "enabled"
                and ack["state"] == "pending" and due and ("ack", key) not in self.jobs
                and sum(lane == "ack" for lane, _ in self.jobs) < 4):
            self._ack_start(path, record)
        if (self._send_ready(key, now)
                and sum(lane == "send" for lane, _ in self.jobs) < 2):
            items = self.reply_items_by_key.get(key, [])
            reply = items[0] if items else None
            if reply is not None:
                self._send_start(path, record, reply)
        return pending, prompt

    def _start_delivery(self, pending: bool, to_enqueue: list[tuple[str, str]], now: float) -> None:
        self.bridge._reserve_queue_prompts(to_enqueue)
        for identifier, prompt in to_enqueue:
            previous = self.delivery_prompts.get(identifier)
            if previous is not None and previous != prompt:
                raise ValueError("delivery identity was reused with different prompt text")
            self.delivery_prompts[identifier] = prompt
        self.delivery_pending = self.delivery_pending or pending or bool(self.delivery_prompts)
        if self.delivery_pending and ("drain", "queue") not in self.jobs and now >= self.next_drain:
            self.next_drain = now + self.recovery_interval
            self.delivery_pending = False
            batch = dict(self.delivery_prompts)
            self.delivery_prompts.clear()
            self.delivery_inflight = batch
            # Register before scheduling the worker: a fast prompt echo must
            # not beat owner-side cache visibility. Failed tentative entries
            # are exact-checked when this worker completes.
            for identifier, prompt in batch.items():
                if self.bridge._remember_prompt(identifier, prompt):
                    self.delivery_tentative.add(identifier)
            # enqueue shares the delivery lock with drain. Both belong in the
            # harness lane: a slow prompt must never hold up durable intake/ACK.
            def deliver() -> object:
                queue_root = self.bridge.state / "queue"
                # `drain` types prompts in the order they were queued. A request's
                # queue ID begins with its message's creation time, so queuing the
                # batch in ID order types its requests in the order their messages
                # were created, then its `feedback-` notices.
                for identifier, prompt in sorted(batch.items()):
                    enqueue(str(queue_root), prompt, message_id=identifier,
                            max_artifact_bytes=_MAX_QUEUE_ARTIFACT_BYTES,
                            atomic_policy=_chat_atomic_policy(self.bridge.state))
                return drain(self.bridge.client, self.bridge.config.target,
                             str(queue_root), ready_timeout=0,
                             max_artifact_bytes=_MAX_QUEUE_ARTIFACT_BYTES,
                             atomic_policy=_chat_atomic_policy(self.bridge.state))
            self._start("drain", "queue", deliver)

    def _stage(self, keys: set[str] | None = None) -> None:
        now = time.monotonic()
        if self.deferred_dirty:
            ready = [(path, message) for path, message in self.deferred_messages.items()
                     if not self._possible_echo(message)]
            if ready:
                # One population preflight for the entire bounded batch. Do not
                # unlink any source until all request writes have succeeded.
                self.bridge._ingest_result(
                    {"messages": [message for _, message in ready]},
                    records=[record for _, record in self._records()])
            for path, message in ready:
                request_path = self.bridge.state / "requests" / path.name
                if request_path.exists():
                    record = _read(request_path)
                    key = get_str(record, "key", "request")
                    self._cache_record(request_path, record)
                    self._refresh_reply_index(key)
                    if keys is not None:
                        keys.add(key)
                try:
                    path.unlink()
                    _fsync_dir(str(self.deferred))
                except BaseException:
                    self.bridge._aux_usage = None
                    raise
                del self.deferred_messages[path]
                usage = self.bridge._aux_usage
                if usage is not None:
                    usage["deferred_records"] -= 1
                    usage["deferred_bytes"] -= self.bridge._message_source_bytes(message)
                self.next_drain = 0.0
            self.deferred_dirty = False
        pending = False
        to_enqueue: list[tuple[str, str]] = []
        selected = [self.records[key] for key in self._ordered_record_keys(now, keys)]
        for path, record in selected:
            waiting, prompt = self._stage_request(path, record, now)
            pending = pending or waiting
            if prompt is not None:
                to_enqueue.append(prompt)
        if self.feedback_dirty:
            for identifier, (feedback_text, queued) in self.feedback_pending.items():
                pending = True
                if not queued:
                    to_enqueue.append((identifier, feedback_text))
            self.feedback_dirty = False
        self._start_delivery(pending, to_enqueue, now)
        if ((self.reconcile_requested or now >= self.next_poll) and ("poll", "page") not in self.jobs
                and now >= self.retry.get(("poll", "page"), (0, 1))[0]):
            self._start_reconciliation_page()
        loaded = [record for _, record in self._records()]
        desired = self.bridge._output_subscription(
            loaded, watch_delivery=self.delivery_pending or ("drain", "queue") in self.jobs)
        self.output.update(desired)
        for key, event in tuple(self.output_pending.items()):
            if ("output", key) not in self.jobs and now >= self.retry.get(("output", key), (0, 1))[0]:
                del self.output_pending[key]
                self.output_inflight[key] = event
                def inspect(event: PaneOutputSnapshot | PaneAgentStatus = event) -> object:
                    return self._inspect_output(event)
                self._start("output", key, inspect)

    def _inspect_output(self, event: PaneOutputSnapshot | PaneAgentStatus) -> PaneOutputSnapshot | None:
        info = resolve_target(self.bridge.client, self.bridge.config.target)
        if info.pane_id != event.pane_id:
            raise HerdrUnavailable("chat output event belongs to a different coordinator pane")
        if self.bridge.config.outbound_mode == "disabled":
            return None
        if isinstance(event, PaneOutputSnapshot):
            return event
        if info.status not in ("idle", "done"):
            return None
        text = self.bridge.client.read(info.pane_id, source="recent-unwrapped", lines=4000)
        if len(text.encode("utf-8")) > 2 * 1024 * 1024:
            raise ValueError("retained chat output exceeds the 2 MiB capture limit")
        return PaneOutputSnapshot(info.pane_id, text, None)

    def _ack_start(self, path: Path, record: dict[str, object]) -> None:
        ack = as_mapping(record["ack"], "ack")
        attempts = ack.get("attempts", 0)
        if type(attempts) is not int or attempts < 0:
            raise ValueError("ack attempts must be a nonnegative integer")
        attempts += 1
        now = datetime.now(timezone.utc)
        ack.update(attempts=attempts, last_attempt_at=now.isoformat().replace("+00:00", "Z"),
                   next_retry_at=(now + timedelta(seconds=min(60, 3 * 2 ** min(attempts - 1, 5))))
                   .isoformat().replace("+00:00", "Z"))
        record["ack"] = ack
        _write(path, record)
        source = as_mapping(record["message"], "source")
        self._transport("ack", get_str(record, "key", "request"), {"action": "react",
            "space": self.bridge.config.space, "message": source["id"], "emoji": ack["emoji"],
            "request_id": ack["request_id"], "attempt": attempts})

    def _send_start(self, path: Path, record: dict[str, object], reply: dict[str, object]) -> None:
        descriptor = _open_private_lock(
            str(self.bridge.state / ".bridge.lock"), "chat bridge lock")
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX)
            self.bridge._reply_started(path, record, reply)
        finally:
            os.close(descriptor)
        source = as_mapping(record["message"], "source")
        key = get_str(record, "key", "request")
        self.send_cursor = key
        self.send_inflight[key] = get_str(reply, "reply_key", "reply")
        self._transport("send", key, {"action": "send", "space": self.bridge.config.space,
            "thread": source["thread"], "request_id": reply["request_id"], "text": self._outbound_text(reply)})

    def _complete(self, item: _Completion) -> set[str] | None:
        job = (item.lane, item.key)
        self.jobs.discard(job)
        error = item.error
        selected: set[str] | None = {item.key}
        poll_terminal = False
        if item.lane in ("ack", "send"):
            path = self.bridge.state / "requests" / f"{item.key}.json"
            cached = self.records.get(item.key)
            record = _read(path) if cached is None else cached[1]
            self._cache_record(path, record)
            identifier: str | None = None
            if error is None:
                try:
                    identifier = get_str(as_mapping(item.result, "transport result"), "id", "transport result")
                    source = as_mapping(record["message"], "source")
                    prefix = str(source["id"]) + "/reactions/" if item.lane == "ack" else self.bridge.config.space + "/messages/"
                    if re.fullmatch(re.escape(prefix) + r"[A-Za-z0-9_.-]+", identifier) is None:
                        raise ValueError("transport result belongs outside the requested resource")
                except (ValueError, TypeError) as exc:
                    error = exc
            if item.lane == "ack":
                ack = as_mapping(record["ack"], "ack")
                if error is None:
                    ack.update(state="acked", reaction_id=identifier, acked_at=item.finished_at,
                               error=None, next_retry_at=None)
                else:
                    ack["error"] = str(error)[:2000]
                record["ack"] = ack
                _write(path, record)
            else:
                reply_key = self.send_inflight.pop(item.key)
                pending = self.reply_items_by_key.get(item.key, [])
                reply = next((value for value in pending
                              if value.get("reply_key") == reply_key), None)
                if reply is None:
                    raise ValueError("completed send has no cached pending reply")
                descriptor = _open_private_lock(
                    str(self.bridge.state / ".bridge.lock"), "chat bridge lock")
                try:
                    fcntl.flock(descriptor, fcntl.LOCK_EX)
                    record = _read(path)
                    self.bridge._reply_complete(
                        path, record, reply, identifier, item.finished_at,
                        None if error is None else str(error)[:2000])
                finally:
                    os.close(descriptor)
                self._cache_record(path, record)
                if error is None:
                    pending.remove(reply)
                    self.deferred_dirty = bool(self.deferred_messages)
                self._refresh_reply_index(item.key, pending)
        elif item.lane == "output":
            selected = set()
            event = self.output_inflight.pop(item.key)
            self.next_drain = 0.0
            if error is None:
                if item.result is not None:
                    if not isinstance(item.result, PaneOutputSnapshot):
                        raise ValueError("invalid inspected output")
                    touched: set[str] = set()
                    feedback_prompts: list[tuple[str, str]] = []
                    result = self.bridge._capture_output(
                        item.result, deliver=False, verify_target=False,
                        records=[record for _, record in self._records()], touched=touched,
                        reply_items=self.reply_items_by_key,
                        feedback_prompts=feedback_prompts)
                    for identifier, prompt in feedback_prompts:
                        self.feedback_pending[identifier] = (prompt, False)
                    self.feedback_dirty = bool(feedback_prompts)
                    if result["errors"]:
                        self._log(f"output capture: {result['errors']}")
                    affected = touched
                    for raw_error in as_sequence(result["errors"], "capture errors"):
                        capture_error = as_mapping(raw_error, "capture error")
                        request = capture_error.get("request")
                        if isinstance(request, str):
                            affected.add(request)
                    for affected_key in affected:
                        path = self.bridge.state / "requests" / f"{affected_key}.json"
                        self._cache_record(path, _read(path))
                        self._refresh_reply_index(
                            affected_key, self.reply_items_by_key.get(affected_key, []))
                    selected.update(affected)
            else:
                self.output_pending.setdefault(item.key, event)
        elif item.lane == "poll":
            selected = set()
            if error is None:
                recover_durable_intake = False
                try:
                    result = as_mapping(item.result, "poll result")
                    messages = result.get("messages")
                    if not isinstance(messages, list):
                        raise ValueError("poll result messages must be a list")
                    checkpoint = self.poll_checkpoint
                    if checkpoint is None:
                        checkpoint = _read(self.bridge.state / "bridge.json")
                    window = self.reconcile_window
                    if window is None:
                        initial = _cursor(checkpoint.get("cursor"), "saved poll cursor")
                        window = _ReconcileWindow(
                            started_at=time.monotonic(), pages=0,
                            cursors=set() if initial is None else {initial},
                        )
                        self.reconcile_window = window
                    next_cursor = _cursor(result.get("cursor"), "poll result cursor")
                    if next_cursor is not None and next_cursor in window.cursors:
                        raise ValueError("poll adapter repeated a reconciliation cursor")
                    accepted: list[object] = []
                    for raw in messages:
                        message = as_mapping(raw, "polled message")
                        self.bridge._validate_message(message)
                        if self._possible_echo(message):
                            self._accept_message(message)
                        else:
                            accepted.append(raw)
                    result["messages"] = accepted
                    # Atomic request/checkpoint writes may reach durable storage
                    # before their fsync reports failure. From this point on,
                    # an exception requires one authoritative recovery pass.
                    recover_durable_intake = True
                    admitted = self.bridge._ingest_result(
                        result, checkpoint,
                        records=[record for _, record in self._records()])
                    selected.update(self._admit_reconciled_records(admitted))
                    window.pages += 1
                    now = time.monotonic()
                    if next_cursor is None:
                        poll_terminal = True
                        self.reconcile_window = None
                        self.next_poll = now + self.reconcile_interval
                    else:
                        window.cursors.add(next_cursor)
                        self.next_poll = now
                        if window.pages >= _MAX_RECONCILE_PAGES:
                            error = ValueError(
                                f"reconciliation page budget {_MAX_RECONCILE_PAGES} exhausted")
                        elif now - window.started_at >= _MAX_RECONCILE_SECONDS:
                            error = ValueError(
                                f"reconciliation time budget {_MAX_RECONCILE_SECONDS:g}s exhausted")
                    self.poll_checkpoint = None
                    self.next_drain = 0.0
                except Exception as exc:
                    error = exc
                    if recover_durable_intake:
                        # The happy path admits only this page's new records.
                        # Exceptional write ambiguity or a partial incremental
                        # admission must recover all durable authority now, not
                        # wait for the unrelated five-minute maintenance pass.
                        selected = None
                        # If authoritative recovery itself fails, propagate it
                        # and let the service restart. Continuing could consume
                        # an already-advanced cursor with a stale request cache.
                        self._reload_records()
        if error is not None:
            self._log(f"{item.lane}: {error}")
            if item.lane not in ("ack", "drain"):
                delay = min(60, self.retry.get(job, (0, 0.5))[1] * 2)
                self.retry[job] = (time.monotonic() + delay, delay)
            if item.lane == "poll":
                self.reconcile_window = None
                self.poll_checkpoint = None
                self.reconcile_requested = True
                self._input_status(reconcile_error=str(error)[:2000])
        else:
            if item.lane != "poll" or poll_terminal:
                self.retry.pop(job, None)
            if item.lane == "poll" and poll_terminal:
                self._input_status(reconcile_error=None, reconciled_at=item.finished_at)
        if item.lane == "drain":
            for identifier in self.delivery_tentative:
                self.bridge._forget_tentative_prompt(identifier)
            self.delivery_tentative.clear()
            for identifier, prompt in self.delivery_inflight.items():
                if not any((self.bridge.state / "queue" / phase / f"{identifier}.json").exists()
                           for phase in ("inbox", "inflight", "processed", "failed")):
                    self.delivery_prompts[identifier] = prompt
            self.delivery_inflight.clear()
            # A stage that ran while this worker was opening may have queued
            # the same durable identity again before its artifact appeared.
            # Exact-path reconciliation drops that stale in-memory duplicate.
            for identifier in tuple(self.delivery_prompts):
                if any((self.bridge.state / "queue" / phase / f"{identifier}.json").exists()
                       for phase in ("inbox", "inflight", "processed", "failed")):
                    del self.delivery_prompts[identifier]
            for identifier, (prompt, _) in tuple(self.feedback_pending.items()):
                phases = [phase for phase in ("inbox", "inflight", "processed", "failed")
                          if (self.bridge.state / "queue" / phase / f"{identifier}.json").exists()]
                if any(phase in ("processed", "failed") for phase in phases):
                    del self.feedback_pending[identifier]
                else:
                    self.feedback_pending[identifier] = (
                        prompt, any(phase in ("inbox", "inflight") for phase in phases))
            self.feedback_dirty = bool(self.feedback_pending)
            # The following full cached stage rechecks every request phase.
            # Do not retain a stale busy flag after the worker delivered it.
            self.delivery_pending = bool(self.delivery_prompts) or bool(self.feedback_pending)
            return None
        return selected

    def _handle(self, notice: _Notice) -> tuple[bool, set[str] | None]:
        if notice.kind == "input":
            kind, key = self._input_event(as_mapping(notice.value, "stream event"))
            if key is not None:
                return True, {key}
            return kind == "gap", set()
        elif notice.kind == "input_connected":
            self._input_status(state="connected", error=None, connected_at=_utc())
            self.reconcile_requested = True
            return True, set()
        elif notice.kind == "input_error":
            self._input_status(state="retrying", error=str(notice.value)[:2000])
            self._log(f"input subscription: {notice.value}")
            return False, set()
        elif notice.kind == "output":
            if not isinstance(notice.value, (PaneOutputSnapshot, PaneAgentStatus)):
                raise ValueError("invalid output event")
            key = "snapshot" if isinstance(notice.value, PaneOutputSnapshot) else "settled"
            self.output_pending[key] = notice.value
            return True, set()
        elif notice.kind in ("output_idle", "output_connected", "output_error"):
            states = {"output_idle": "idle", "output_connected": "connected",
                      "output_error": "retrying"}
            error = str(notice.value)[:2000] if notice.kind == "output_error" else None
            self.output_observer.observe(states[notice.kind], error)
            return False, set()
        elif notice.kind == "complete":
            if not isinstance(notice.value, _Completion):
                raise ValueError("invalid completion event")
            selected = self._complete(notice.value)
            if notice.value.lane in ("ack", "send"):
                return True, {notice.value.key}
            return True, selected
        elif notice.kind == "local_reply":
            if not isinstance(notice.value, str) or re.fullmatch(r"[0-9a-f]{64}", notice.value) is None:
                raise ValueError("invalid local reply wake")
            path = self.bridge.state / "requests" / f"{notice.value}.json"
            descriptor = _open_private_lock(
                str(self.bridge.state / ".bridge.lock"), "chat bridge lock")
            try:
                fcntl.flock(descriptor, fcntl.LOCK_EX)
                record = _read(path)
                items = self.bridge._recover_reply_state(path, record)
                self.bridge._adopt_submission(path, record, items)
            finally:
                os.close(descriptor)
            self._cache_record(path, record)
            self._refresh_reply_index(notice.value, items)
            return True, {notice.value}
        return False, set()

    def _due_keys(self, now: float) -> set[str]:
        due: set[str] = set()
        wall = datetime.now(timezone.utc)
        if (self.bridge.config.outbound_mode == "enabled"
                and sum(lane == "ack" for lane, _ in self.jobs) < 4):
            for key, (_, record) in self.records.items():
                ack = as_mapping(record.get("ack", {}), "ack")
                retry = ack.get("next_retry_at")
                if ack.get("state") == "pending" and ("ack", key) not in self.jobs and (
                    not isinstance(retry, str) or _timestamp(retry) <= wall
                ):
                    due.add(key)
        if (self.bridge.config.outbound_mode == "enabled"
                and sum(lane == "send" for lane, _ in self.jobs) < 2):
            capacity = 2 - sum(lane == "send" for lane, _ in self.jobs)
            for key in self._ordered_record_keys(now):
                if capacity <= 0:
                    break
                if self._send_ready(key, now):
                    due.add(key)
                    capacity -= 1
        return due

    def _next_deadline(self) -> float:
        now = time.monotonic()
        wall = datetime.now(timezone.utc)
        candidates = [self.next_recovery]
        if ("poll", "page") not in self.jobs:
            candidates.append(max(now, self.retry.get(("poll", "page"), (self.next_poll, 1))[0]
                                  if self.reconcile_requested else self.next_poll))
        if self.delivery_pending and ("drain", "queue") not in self.jobs:
            candidates.append(max(now, self.next_drain))
        if (self.bridge.config.outbound_mode == "enabled"
                and sum(lane == "ack" for lane, _ in self.jobs) < 4):
            for key, (_, record) in self.records.items():
                ack = as_mapping(record.get("ack", {}), "ack")
                if ack.get("state") == "pending" and ("ack", key) not in self.jobs:
                    retry = ack.get("next_retry_at")
                    if isinstance(retry, str):
                        candidates.append(now + max(0.0, (_timestamp(retry) - wall).total_seconds()))
                    else:
                        candidates.append(now)
        sends_full = (self.bridge.config.outbound_mode == "disabled"
                      or sum(lane == "send" for lane, _ in self.jobs) >= 2)
        if not sends_full:
            for key in self._ordered_record_keys(now):
                if self._send_candidate(key):
                    deadline = self.retry.get(("send", key), (now, 1))[0]
                    candidates.append(max(now, deadline))
        for job, (deadline, _) in self.retry.items():
            # Send retries are covered above only while their durable request is
            # actually sendable. A stale retry must not turn a queued request
            # into a zero-timeout service loop.
            if job[0] != "send" and job not in self.jobs:
                candidates.append(max(now, deadline))
        for observer in (self.input_observer, self.output_observer):
            if observer.next_write is not None:
                candidates.append(observer.next_write)
        return min(candidates)

    def run(self) -> None:
        resolve_target(self.bridge.client, self.bridge.config.target)
        self.input.thread.start()
        self.output.thread.start()
        self.local.thread.start()
        try:
            self._stage()
            while not self.stop.is_set():
                self.output_observer.flush()
                self.input_observer.flush()
                # ``threading.Event`` and ``queue.Queue`` cannot share one wait.
                # Bound the otherwise deadline-driven wait so an externally set
                # stop event is observed promptly without doing maintenance work.
                timeout = min(30.0, max(0.0, self._next_deadline() - time.monotonic()))
                try:
                    notices = [self.events.get(timeout=timeout)]
                except queue.Empty:
                    notices = []
                while True:
                    try:
                        notices.append(self.events.get_nowait())
                    except queue.Empty:
                        break
                needs_stage = False
                all_records = False
                keys: set[str] = set()
                for notice in notices:
                    needed, selected = self._handle(notice)
                    needs_stage = needs_stage or needed
                    if needed and selected is None:
                        all_records = True
                    elif needed and selected is not None:
                        keys.update(selected)
                now = time.monotonic()
                if now >= self.next_recovery:
                    self._reload_records()
                    self.feedback_dirty = True
                    self.deferred_dirty = True
                    self.next_recovery = now + self.recovery_interval
                    needs_stage = all_records = True
                due = self._due_keys(now)
                if due:
                    needs_stage = True
                    keys.update(due)
                if any(job[0] != "send" and now >= deadline and job not in self.jobs
                       for job, (deadline, _) in self.retry.items()):
                    needs_stage = True
                if (self.reconcile_requested or now >= self.next_poll or
                        (self.delivery_pending and now >= self.next_drain)):
                    needs_stage = True
                if needs_stage:
                    self._stage(None if all_records else keys)
        finally:
            self.stop.set()
            self.input.close()
            self.output.close()
            self.local.close()
            for pool in self.pools.values():
                pool.shutdown(wait=True, cancel_futures=True)


def run_streaming(bridge: Bridge, *, reconcile_interval: float = 300,
                  prog: str = "agentctl chat", stop: threading.Event | None = None,
                  observer_write_interval: float = _DEFAULT_OBSERVER_WRITE_INTERVAL) -> None:
    """Consume push events while ACK, prompt, and reply operations run independently."""
    _Runtime(bridge, reconcile_interval, prog, stop or threading.Event(),
             observer_write_interval).run()
