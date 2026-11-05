"""Real Unix-socket framing, reconnect recovery, and bounded output subscriptions."""

from __future__ import annotations

import json
import queue
import socket
import threading
import time
from collections.abc import Callable, Iterator
from contextlib import contextmanager
from pathlib import Path

import pytest

from agentctl.chat_output import OutputStreamError, PaneAgentStatus, PaneOutputSnapshot, PaneOutputStream
from agentctl.jsonx import as_mapping


_PANE = "w1:p2"
_PATTERN = r"^\s*</GCHAT_REPLY_request_nonce>\s*$"
_Handler = Callable[[socket.socket, dict[str, object]], None]


def _ack(connection: socket.socket, request: dict[str, object]) -> None:
    connection.sendall(_wire({"id": request["id"], "result": {"type": "subscription_started"}}))


def _wire(document: object) -> bytes:
    return (json.dumps(document, ensure_ascii=False) + "\n").encode()


def _event(text: str = "<GCHAT_REPLY_request_nonce>\nready 🤖\n</GCHAT_REPLY_request_nonce>") -> dict[str, object]:
    return {"event": "pane.output_matched", "data": {
        "pane_id": _PANE, "matched_line": "</GCHAT_REPLY_request_nonce>",
        "read": {"pane_id": _PANE, "workspace_id": "w1", "tab_id": "w1:t1",
                 "source": "recent_unwrapped", "format": "text", "text": text,
                 "revision": 0, "truncated": False},
    }}


def _status_event(status: str = "idle", pane: str = _PANE) -> dict[str, object]:
    return {"event": "pane.agent_status_changed", "data": {
        "pane_id": pane, "workspace_id": "w1", "agent": "codex", "agent_status": status,
    }}


def _next_snapshot(stream: PaneOutputStream, timeout: float = 1) -> PaneOutputSnapshot:
    events = stream.wait(timeout)
    assert len(events) == 1 and isinstance(events[0], PaneOutputSnapshot)
    return events[0]


class _Server:
    def __init__(self, path: Path, handler: _Handler) -> None:
        self.path = str(path)
        self.handler = handler
        self.accepted: queue.Queue[tuple[socket.socket, dict[str, object]]] = queue.Queue()
        self.errors: list[BaseException] = []
        self.connections: list[socket.socket] = []
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(self.path)
        self.listener.listen()
        self.listener.settimeout(0.1)
        self.stop = threading.Event()
        self.worker = threading.Thread(target=self._run)
        self.worker.start()

    def _run(self) -> None:
        try:
            while not self.stop.is_set():
                try:
                    connection, _ = self.listener.accept()
                except TimeoutError:
                    continue
                self.connections.append(connection)
                connection.settimeout(2)
                frame = bytearray()
                while b"\n" not in frame:
                    chunk = connection.recv(65536)
                    if not chunk:
                        raise AssertionError("subscription closed before sending its request")
                    frame.extend(chunk)
                request = as_mapping(json.loads(frame), "test request")
                self.handler(connection, request)
                self.accepted.put((connection, request))
        except BaseException as exc:
            self.errors.append(exc)

    def connection(self) -> tuple[socket.socket, dict[str, object]]:
        return self.accepted.get(timeout=2)

    def close(self) -> None:
        self.stop.set()
        self.worker.join(timeout=3)
        self.listener.close()
        for connection in self.connections:
            connection.close()
        assert not self.worker.is_alive()
        assert not self.errors


@contextmanager
def _server(tmp_path: Path, handler: _Handler = _ack) -> Iterator[_Server]:
    server = _Server(tmp_path / "h.sock", handler)
    try:
        yield server
    finally:
        server.close()


@contextmanager
def _stream(server: _Server, *, max_frame_bytes: int = 2 * 1024 * 1024,
            watch_settled: bool = False, pattern: str = _PATTERN) -> Iterator[PaneOutputStream]:
    stream = PaneOutputStream(server.path, _PANE, pattern,
                              max_frame_bytes=max_frame_bytes, watch_settled=watch_settled)
    try:
        yield stream
    finally:
        stream.close()


def test_subscription_request_and_retained_snapshot(tmp_path: Path) -> None:
    with _server(tmp_path) as server, _stream(server) as stream:
        connection, request = server.connection()
        assert request["method"] == "events.subscribe"
        params = as_mapping(request["params"], "params")
        assert params == {"subscriptions": [{"type": "pane.output_matched", "pane_id": _PANE,
                           "source": "recent_unwrapped", "lines": 4000, "strip_ansi": True,
                           "match": {"type": "regex", "value": _PATTERN}}]}
        connection.sendall(_wire(_event()))
        result = stream.wait(1)
        assert result == (PaneOutputSnapshot(_PANE, "<GCHAT_REPLY_request_nonce>\nready 🤖\n</GCHAT_REPLY_request_nonce>", False, 0),)
        assert stream.wait(0) == ()


def test_each_exact_next_marker_is_an_independent_line_subscription(tmp_path: Path) -> None:
    patterns = (
        r"^</CHAT_REPLY_AAAAAAAAAAAAAAAAAAAAAA_1>$",
        r"^</CHAT_REPLY_BBBBBBBBBBBBBBBBBBBBBB_7>$",
    )
    with _server(tmp_path) as server:
        stream = PaneOutputStream(server.path, _PANE, patterns, watch_settled=True)
        try:
            _, request = server.connection()
            params = as_mapping(request["params"], "params")
            subscriptions = params["subscriptions"]
            assert isinstance(subscriptions, list)
            output = subscriptions[:2]
            assert [as_mapping(as_mapping(item, "subscription")["match"], "match")["value"]
                    for item in output] == list(patterns)
            assert all(as_mapping(item, "subscription")["type"] == "pane.output_matched"
                       for item in output)
            assert [as_mapping(item, "subscription")["type"] for item in subscriptions[2:]] == [
                "pane.agent_status_changed", "pane.agent_status_changed",
            ]
        finally:
            stream.close()


def test_ack_and_snapshot_in_same_socket_write(tmp_path: Path) -> None:
    def handler(connection: socket.socket, request: dict[str, object]) -> None:
        connection.sendall(_wire({"id": request["id"], "result": {"type": "subscription_started"}}) + _wire(_event()))

    with _server(tmp_path, handler) as server, _stream(server) as stream:
        assert _next_snapshot(stream, 0).revision == 0


def test_partial_utf8_frame_survives_timeout(tmp_path: Path) -> None:
    with _server(tmp_path) as server, _stream(server) as stream:
        connection, _ = server.connection()
        payload = _wire(_event())
        split = payload.index("🤖".encode()) + 1
        connection.sendall(payload[:split])
        assert stream.wait(0.01) == ()
        connection.sendall(payload[split:])
        assert "🤖" in _next_snapshot(stream).text


def test_nonblocking_socket_readiness_race_retries_within_deadline(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    with _server(tmp_path) as server, _stream(server) as stream:
        connection, _ = server.connection()
        original_recv = socket.socket.recv
        blocked = False

        def recv(current: socket.socket, size: int, flags: int = 0) -> bytes:
            nonlocal blocked
            if not blocked and not current.getblocking():
                blocked = True
                raise BlockingIOError
            return original_recv(current, size, flags)

        monkeypatch.setattr(socket.socket, "recv", recv)
        connection.sendall(_wire(_event()))
        assert "ready" in _next_snapshot(stream).text
        assert blocked


def test_multiple_frames_are_preserved_in_order(tmp_path: Path) -> None:
    with _server(tmp_path) as server, _stream(server) as stream:
        connection, _ = server.connection()
        connection.sendall(_wire(_event("first")) + _wire(_event("second")))
        assert _next_snapshot(stream).text == "first"
        assert _next_snapshot(stream, 0).text == "second"


def test_timeout_is_not_eof_and_wake_interrupts_blocking_wait(tmp_path: Path) -> None:
    with _server(tmp_path) as server, _stream(server) as stream:
        server.connection()
        assert stream.wait(0.01) == ()
        started = threading.Event()
        result: list[tuple[PaneOutputSnapshot | PaneAgentStatus, ...]] = []

        def wait() -> None:
            started.set()
            result.append(stream.wait(30))

        worker = threading.Thread(target=wait)
        worker.start()
        assert started.wait(timeout=2)
        stream.wake()
        worker.join(timeout=2)
        assert not worker.is_alive()
        assert result == [()]


@pytest.mark.parametrize("partial", [b"", b'{"event":'])
def test_eof_is_explicit_and_reconnect_replays_retained_match(tmp_path: Path, partial: bytes) -> None:
    with _server(tmp_path) as server, _stream(server) as stream:
        connection, request = server.connection()
        connection.sendall(partial)
        connection.close()
        with pytest.raises(OutputStreamError) as error:
            stream.wait(1)
        assert error.value.code == "stream_eof"
        with pytest.raises(OutputStreamError, match="stream_disconnected"):
            stream.wait(0)
        stream.reconnect()
        replacement, new_request = server.connection()
        assert new_request["id"] != request["id"]
        replacement.sendall(_wire(_event()))
        assert "ready" in _next_snapshot(stream).text


@pytest.mark.parametrize("payload", [b"not-json\n", b"\xff\n", b"[]\n", b"{}\n"])
def test_invalid_wire_frames_fail_closed(tmp_path: Path, payload: bytes) -> None:
    with _server(tmp_path) as server, _stream(server) as stream:
        connection, _ = server.connection()
        connection.sendall(payload)
        with pytest.raises(OutputStreamError) as error:
            stream.wait(1)
        assert error.value.code == "invalid_frame"


@pytest.mark.parametrize("field,value", [("pane_id", "w2:p1"), ("source", "visible"),
                                         ("format", "ansi"), ("text", 123),
                                         ("truncated", None), ("revision", True),
                                         ("revision", -1)])
def test_wrong_snapshot_identity_and_schema_are_rejected(tmp_path: Path, field: str, value: object) -> None:
    with _server(tmp_path) as server, _stream(server) as stream:
        connection, _ = server.connection()
        event = _event()
        data = as_mapping(event["data"], "event")
        read = as_mapping(data["read"], "snapshot")
        read[field] = value
        data["read"] = read
        event["data"] = data
        connection.sendall(_wire(event))
        with pytest.raises(OutputStreamError, match="invalid_frame"):
            stream.wait(1)


def test_snapshot_truncation_is_reported_and_revision_may_be_absent(tmp_path: Path) -> None:
    with _server(tmp_path) as server, _stream(server) as stream:
        connection, _ = server.connection()
        event = _event()
        data = as_mapping(event["data"], "event")
        read = as_mapping(data["read"], "snapshot")
        read["truncated"] = True
        del read["revision"]
        data["read"] = read
        event["data"] = data
        connection.sendall(_wire(event))
        result = _next_snapshot(stream)
        assert result.truncated and result.revision is None


@pytest.mark.parametrize("newline", [b"", b"\n"])
def test_oversize_frame_has_explicit_limit_error(tmp_path: Path, newline: bytes) -> None:
    with _server(tmp_path) as server, _stream(server, max_frame_bytes=256) as stream:
        connection, _ = server.connection()
        connection.sendall(b"x" * 257 + newline)
        with pytest.raises(OutputStreamError) as error:
            stream.wait(1)
        assert error.value.code == "frame_limit_exceeded"


def test_subscription_rejection_preserves_server_reason(tmp_path: Path) -> None:
    def handler(connection: socket.socket, request: dict[str, object]) -> None:
        connection.sendall(_wire({"id": request["id"], "error": {"code": "pane_not_found", "message": "pane is gone"}}))

    with _server(tmp_path, handler) as server:
        with pytest.raises(OutputStreamError) as error:
            PaneOutputStream(server.path, _PANE, _PATTERN)
        assert error.value.code == "pane_not_found"


@pytest.mark.parametrize("frame", ["wrong-id", "output-first", "wrong-result"])
def test_subscription_ack_is_required_and_correlated(tmp_path: Path, frame: str) -> None:
    def handler(connection: socket.socket, request: dict[str, object]) -> None:
        document = (_event() if frame == "output-first" else {
            "id": "other" if frame == "wrong-id" else request["id"],
            "result": {"type": "other" if frame == "wrong-result" else "subscription_started"},
        })
        connection.sendall(_wire(document))

    with _server(tmp_path, handler) as server:
        with pytest.raises(OutputStreamError, match="invalid_frame"):
            PaneOutputStream(server.path, _PANE, _PATTERN)


def test_missing_ack_times_out_without_hanging(tmp_path: Path) -> None:
    with _server(tmp_path, lambda connection, request: None) as server:
        started = time.monotonic()
        with pytest.raises(OutputStreamError) as error:
            PaneOutputStream(server.path, _PANE, _PATTERN, connect_timeout=0.05)
        assert error.value.code == "subscription_timeout"
        assert time.monotonic() - started < 2


def test_close_is_idempotent_and_terminal(tmp_path: Path) -> None:
    with _server(tmp_path) as server, _stream(server) as stream:
        server.connection()
        stream.close()
        stream.close()
        stream.wake()
        with pytest.raises(OutputStreamError, match="stream_closed"):
            stream.wait(0)
        with pytest.raises(OutputStreamError, match="stream_closed"):
            stream.reconnect()


def test_combined_subscription_handles_output_then_requested_settled_status(tmp_path: Path) -> None:
    with _server(tmp_path) as server, _stream(server, watch_settled=True) as stream:
        connection, request = server.connection()
        params = as_mapping(request["params"], "params")
        subscriptions = params["subscriptions"]
        assert isinstance(subscriptions, list)
        assert subscriptions[1:] == [
            {"type": "pane.agent_status_changed", "pane_id": _PANE, "agent_status": "idle"},
            {"type": "pane.agent_status_changed", "pane_id": _PANE, "agent_status": "done"},
        ]
        connection.sendall(_wire(_event()) + _wire(_status_event()) + _wire(_status_event("done")))
        assert _next_snapshot(stream).text.endswith("</GCHAT_REPLY_request_nonce>")
        assert stream.wait(0) == (PaneAgentStatus(_PANE, "idle"),)
        assert stream.wait(0) == (PaneAgentStatus(_PANE, "done"),)


def test_status_only_subscription_has_no_output_predicate(tmp_path: Path) -> None:
    with _server(tmp_path) as server, _stream(server, watch_settled=True, pattern="") as stream:
        connection, request = server.connection()
        params = as_mapping(request["params"], "params")
        assert params["subscriptions"] == [
            {"type": "pane.agent_status_changed", "pane_id": _PANE, "agent_status": "idle"},
            {"type": "pane.agent_status_changed", "pane_id": _PANE, "agent_status": "done"},
        ]
        connection.sendall(_wire(_status_event()))
        assert stream.wait(1) == (PaneAgentStatus(_PANE, "idle"),)
        connection.sendall(_wire(_event()))
        with pytest.raises(OutputStreamError, match="invalid_frame"):
            stream.wait(1)


def test_empty_pattern_without_status_watch_is_rejected() -> None:
    with pytest.raises(ValueError, match="empty pattern requires"):
        PaneOutputStream("unused.sock", _PANE, "")


@pytest.mark.parametrize("status,pane", [("working", _PANE), ("blocked", _PANE),
                                        ("unknown", _PANE), ("idle", "w1:p9")])
def test_unrequested_status_or_pane_is_refused(tmp_path: Path, status: str, pane: str) -> None:
    with _server(tmp_path) as server, _stream(server, watch_settled=True) as stream:
        connection, _ = server.connection()
        connection.sendall(_wire(_status_event(status, pane)))
        with pytest.raises(OutputStreamError, match="invalid_frame"):
            stream.wait(1)


def test_status_frames_are_rejected_when_watching_is_disabled(tmp_path: Path) -> None:
    with _server(tmp_path) as server, _stream(server) as stream:
        connection, _ = server.connection()
        connection.sendall(_wire(_status_event()))
        with pytest.raises(OutputStreamError, match="unrequested agent status"):
            stream.wait(1)


def test_current_idle_status_after_ack_is_retained_for_wait(tmp_path: Path) -> None:
    def handler(connection: socket.socket, request: dict[str, object]) -> None:
        connection.sendall(_wire({"id": request["id"], "result": {"type": "subscription_started"}})
                           + _wire(_status_event()))

    with _server(tmp_path, handler) as server, _stream(server, watch_settled=True) as stream:
        assert stream.wait(0) == (PaneAgentStatus(_PANE, "idle"),)
