"""Bounded, blocking subscriptions to retained Herdr terminal output.

The bridge blocks on a Unix socket instead of periodically reading a pane. Herdr
0.8 internally polls subscription predicates every 100 milliseconds. Each output
predicate matches one individual rendered line and emits on a false-to-true edge.
The bridge therefore subscribes to each request's exact next protocol-v3 closing
marker and rebuilds only after that ordinal is durably captured. Events contain a
retained snapshot, not a lossless terminal stream or assistant message.

There is no replay cursor. Reconnecting evaluates the current snapshot again.
Callers must deduplicate replies durably and reject incomplete bracketed replies;
terminal scrollback eviction and alternate-screen redraw cannot be recovered here.
"""

from __future__ import annotations

import json
import math
import select
import socket
import time
import uuid
from collections.abc import Sequence
from dataclasses import dataclass

from agentctl.errors import HerdrUnavailable
from agentctl.jsonx import as_mapping


class OutputStreamError(HerdrUnavailable):
    """An explicit subscription failure; ordinary wait timeouts are not errors."""

    def __init__(self, code: str, detail: str) -> None:
        self.code = code
        super().__init__(f"{code}: {detail}")


@dataclass(frozen=True)
class PaneOutputSnapshot:
    """One matched snapshot whose pane identity and text encoding were checked."""

    pane_id: str
    text: str
    # A plain-text CLI read has no truncation metadata; socket events always do.
    truncated: bool | None
    # Herdr 0.8 can report revision zero for different snapshots. It is not a cursor.
    revision: int | None = None


@dataclass(frozen=True)
class PaneAgentStatus:
    """A settled-state hint requiring a fresh identity check and output read."""

    pane_id: str
    status: str


class PaneOutputStream:
    """Subscribe immediately, then block with ``wait`` until output or a deadline.

    ``pattern`` is a regular expression Herdr matches against individual rendered
    lines. ``lines`` bounds the requested retained snapshot; ``max_frame_bytes`` independently caps
    each JSON wire frame. Connection and subscription acknowledgement share the
    ``connect_timeout`` deadline, in seconds. ``wake`` may interrupt ``wait`` from
    another thread; other methods must be called by the single owning thread.

    ``watch_settled`` adds idle/done status subscriptions. Their events are recheck
    hints, not proof of a completed reply. An empty output pattern is allowed only
    in this mode, keeping status recovery alive after a rejected output match.

    EOF, malformed data, rejected subscriptions, and frame overflow are surfaced.
    The caller chooses reconnect backoff and calls ``reconnect``; reconnection never
    replays input or presumes a disappeared reply succeeded.
    """

    def __init__(
        self,
        socket_path: str,
        pane_id: str,
        pattern: str | Sequence[str],
        *,
        lines: int = 4000,
        connect_timeout: float = 5.0,
        max_frame_bytes: int = 2 * 1024 * 1024,
        watch_settled: bool = False,
    ) -> None:
        patterns = (pattern,) if isinstance(pattern, str) and pattern else tuple(pattern)
        if any(not isinstance(item, str) or not item for item in patterns):
            raise ValueError("output patterns must be nonempty strings")
        if not socket_path or not pane_id or (not patterns and not watch_settled):
            raise ValueError("socket path and pane id must be nonempty; an empty pattern requires settled-state watching")
        if not math.isfinite(connect_timeout) or connect_timeout <= 0:
            raise ValueError("connection timeout must be finite and positive")
        if lines <= 0 or max_frame_bytes <= 0:
            raise ValueError("snapshot lines and frame byte limit must be positive")
        self._path, self._pane, self._patterns = socket_path, pane_id, patterns
        self._watch_settled = watch_settled
        self._lines, self._connect_timeout = lines, connect_timeout
        self._max_frame = max_frame_bytes
        self._socket: socket.socket | None = None
        self._buffer = bytearray()
        self._request_id = ""
        self._acknowledged = False
        self._closed = False
        self._wake_read, self._wake_write = socket.socketpair()
        self._wake_read.setblocking(False)
        self._wake_write.setblocking(False)
        try:
            self.reconnect()
        except BaseException:
            self.close()
            raise

    def _disconnect(self) -> None:
        if self._socket is not None:
            self._socket.close()
            self._socket = None
        self._buffer.clear()
        self._acknowledged = False

    def reconnect(self) -> None:
        """Replace the connection and await acknowledgement within the deadline.

        Any retained match is received by the next ``wait`` call. No cursor or
        server history is assumed. A failed attempt leaves the stream disconnected.
        """
        if self._closed:
            raise OutputStreamError("stream_closed", "cannot reconnect a closed output stream")
        self._disconnect()
        deadline = time.monotonic() + self._connect_timeout
        connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._socket = connection
        self._request_id = f"agentctl:chat-output:{uuid.uuid4().hex}"
        subscriptions: list[dict[str, object]] = []
        for pattern in self._patterns:
            subscriptions.append({
                "type": "pane.output_matched",
                "pane_id": self._pane,
                "source": "recent_unwrapped",
                "lines": self._lines,
                "strip_ansi": True,
                "match": {"type": "regex", "value": pattern},
            })
        if self._watch_settled:
            subscriptions.extend({"type": "pane.agent_status_changed", "pane_id": self._pane,
                                  "agent_status": status} for status in ("idle", "done"))
        request = {
            "id": self._request_id,
            "method": "events.subscribe",
            "params": {"subscriptions": subscriptions},
        }
        try:
            connection.settimeout(self._connect_timeout)
            connection.connect(self._path)
            connection.settimeout(max(0.001, deadline - time.monotonic()))
            connection.sendall((json.dumps(request, ensure_ascii=False) + "\n").encode())
            connection.setblocking(False)
            while not self._acknowledged:
                frame = self._next_frame(deadline)
                if frame is None:
                    raise OutputStreamError("subscription_timeout", "Herdr did not acknowledge the subscription")
                snapshot = self._decode(frame)
                if snapshot is not None:
                    raise OutputStreamError("invalid_frame", "received output before subscription acknowledgement")
        except OutputStreamError:
            self._disconnect()
            raise
        except OSError as exc:
            self._disconnect()
            raise OutputStreamError("connection_failed", str(exc)) from exc

    def _next_frame(self, deadline: float) -> bytes | None:
        connection = self._socket
        if connection is None:
            raise OutputStreamError("stream_disconnected", "reconnect the output stream before waiting")
        while True:
            newline = self._buffer.find(b"\n")
            if newline >= 0:
                if newline > self._max_frame:
                    raise OutputStreamError("frame_limit_exceeded", "Herdr output frame exceeds the byte limit")
                frame = bytes(self._buffer[:newline])
                del self._buffer[:newline + 1]
                return frame
            if len(self._buffer) > self._max_frame:
                raise OutputStreamError("frame_limit_exceeded", "Herdr output frame exceeds the byte limit")
            remaining = max(0.0, deadline - time.monotonic())
            readable, _, _ = select.select([connection, self._wake_read], [], [], remaining)
            if not readable:
                return None
            if self._wake_read in readable:
                try:
                    while self._wake_read.recv(4096):
                        pass
                except BlockingIOError:
                    pass
                return None
            try:
                chunk = connection.recv(min(65536, self._max_frame + 1 - len(self._buffer)))
            except BlockingIOError:
                # Readiness can be stale by the time a nonblocking socket is
                # consumed; remain inside the original bounded deadline.
                continue
            if not chunk:
                reason = "Herdr closed the output subscription"
                if self._buffer:
                    reason += " in the middle of a frame"
                raise OutputStreamError("stream_eof", reason)
            self._buffer.extend(chunk)

    def _decode(self, frame: bytes) -> PaneOutputSnapshot | PaneAgentStatus | None:
        try:
            document = as_mapping(json.loads(frame.decode("utf-8")), "Herdr subscription frame")
            if "error" in document:
                error = as_mapping(document["error"], "Herdr subscription error")
                code, message = error.get("code"), error.get("message")
                if not isinstance(code, str) or not isinstance(message, str):
                    raise ValueError("error frame has no code or message")
                raise OutputStreamError(code, message)
            if "result" in document:
                result = as_mapping(document["result"], "Herdr subscription response")
                if (self._acknowledged or document.get("id") != self._request_id
                        or result.get("type") != "subscription_started"):
                    raise ValueError("unexpected subscription acknowledgement")
                self._acknowledged = True
                return None
            if not self._acknowledged:
                raise ValueError("unexpected subscription event")
            if document.get("event") == "pane.agent_status_changed":
                if not self._watch_settled:
                    raise ValueError("received an unrequested agent status event")
                data = as_mapping(document.get("data"), "Herdr agent status event")
                if data.get("pane_id") != self._pane:
                    raise ValueError("agent status belongs to a different pane")
                status = data.get("agent_status")
                if not isinstance(status, str) or status not in ("idle", "done"):
                    raise ValueError("agent status is not a requested settled state")
                return PaneAgentStatus(self._pane, status)
            if not self._patterns or document.get("event") != "pane.output_matched":
                raise ValueError("unexpected subscription event")
            data = as_mapping(document.get("data"), "Herdr output event")
            read = as_mapping(data.get("read"), "Herdr output snapshot")
            if data.get("pane_id") != self._pane or read.get("pane_id") != self._pane:
                raise ValueError("snapshot belongs to a different pane")
            if read.get("source") != "recent_unwrapped" or read.get("format") != "text":
                raise ValueError("snapshot has a different source or format")
            text, truncated, revision = read.get("text"), read.get("truncated"), read.get("revision")
            if not isinstance(text, str) or not isinstance(truncated, bool):
                raise ValueError("snapshot text or truncation flag is invalid")
            # JSON accepts escaped lone surrogates, but a reply must remain valid
            # UTF-8 when persisted and sent to the chat transport.
            text.encode("utf-8")
            if revision is not None and (type(revision) is not int or revision < 0):
                raise ValueError("snapshot revision is invalid")
            return PaneOutputSnapshot(self._pane, text, truncated, revision)
        except (UnicodeError, json.JSONDecodeError, TypeError, ValueError) as exc:
            raise OutputStreamError("invalid_frame", str(exc)) from exc

    def wait(self, timeout: float) -> tuple[PaneOutputSnapshot | PaneAgentStatus, ...]:
        """Block for at most ``timeout`` seconds, returning one output/status event.

        An empty tuple means the deadline elapsed or ``wake`` was called. Partial
        frames survive this ordinary timeout. EOF and protocol errors disconnect
        the stream and raise ``OutputStreamError`` instead of mimicking a timeout.
        """
        if not math.isfinite(timeout) or timeout < 0:
            raise ValueError("wait timeout must be finite and nonnegative")
        if self._closed:
            raise OutputStreamError("stream_closed", "cannot wait on a closed output stream")
        try:
            frame = self._next_frame(time.monotonic() + timeout)
            if frame is None:
                return ()
            snapshot = self._decode(frame)
            if snapshot is None:
                raise OutputStreamError("invalid_frame", "unexpected acknowledgement during output wait")
            return (snapshot,)
        except OutputStreamError:
            self._disconnect()
            raise
        except OSError as exc:
            self._disconnect()
            raise OutputStreamError("connection_failed", str(exc)) from exc

    def wake(self) -> None:
        """Interrupt the owning thread's wait, without discarding pending output."""
        try:
            self._wake_write.send(b"x")
        except (BlockingIOError, OSError):
            pass

    def close(self) -> None:
        """Release both the subscription and local wakeup descriptors; idempotent."""
        if not self._closed:
            self._closed = True
            self._disconnect()
            self._wake_read.close()
            self._wake_write.close()
