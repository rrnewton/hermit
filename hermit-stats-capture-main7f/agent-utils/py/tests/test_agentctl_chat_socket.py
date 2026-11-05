"""Private Unix transport boundaries, bounded calls, and independent operations."""

from __future__ import annotations

import json
import os
import queue
import socket
import threading
import time
from collections.abc import Callable, Iterator
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from dataclasses import asdict
from pathlib import Path

import pytest

from agentctl.agent import Target
from agentctl.chat import Bridge, CommandTransport, Config, GoogleChatTransport
from agentctl.chat_socket import SocketTransport
from agentctl.errors import HerdrUnavailable
from agentctl.jsonx import as_mapping


_Handler = Callable[[socket.socket, dict[str, object]], None]
_REQUEST: dict[str, object] = {
    "action": "send", "space": "spaces/test", "thread": "spaces/test/threads/one",
    "text": "[fixture-agent] Ready 🤖\nSecond line",
    "request_id": "f17fb68a-5597-49a9-a1ab-d14b26331b0e",
}
_RESPONSE: dict[str, object] = {"id": "spaces/test/messages/reply"}


def _wire(value: object) -> bytes:
    return (json.dumps(value, ensure_ascii=False) + "\n").encode()


def _reply(connection: socket.socket, request: dict[str, object]) -> None:
    connection.sendall(_wire(_RESPONSE))


class _Server:
    def __init__(self, directory: Path, handler: _Handler) -> None:
        directory.mkdir(mode=0o700)
        self.path = directory / "s"
        self.handler = handler
        self.requests: queue.Queue[dict[str, object]] = queue.Queue()
        self.errors: list[BaseException] = []
        self.connections: list[socket.socket] = []
        self.workers: list[threading.Thread] = []
        self.stop = threading.Event()
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(str(self.path))
        self.listener.listen()
        self.listener.settimeout(0.02)
        self.thread = threading.Thread(target=self._run)
        self.thread.start()

    def _run(self) -> None:
        try:
            while not self.stop.is_set():
                try:
                    connection, _ = self.listener.accept()
                except TimeoutError:
                    continue
                self.connections.append(connection)
                worker = threading.Thread(target=self._handle, args=(connection,))
                self.workers.append(worker)
                worker.start()
        except BaseException as exc:
            self.errors.append(exc)

    def _handle(self, connection: socket.socket) -> None:
        try:
            with connection:
                connection.settimeout(2)
                frame = bytearray()
                while b"\n" not in frame:
                    chunk = connection.recv(65536)
                    if not chunk:
                        # A peer-identity rejection closes before sending a request.
                        assert not frame
                        return
                    frame.extend(chunk)
                    assert len(frame) <= 2 * 1024 * 1024
                request = as_mapping(json.loads(frame), "test request")
                self.requests.put(request)
                self.handler(connection, request)
        except (BrokenPipeError, ConnectionResetError):
            # Timeouts and size limits deliberately terminate a response early.
            pass
        except BaseException as exc:
            self.errors.append(exc)

    def close(self) -> None:
        self.stop.set()
        self.thread.join(timeout=3)
        self.listener.close()
        for worker in self.workers:
            worker.join(timeout=3)
        assert not self.thread.is_alive()
        assert all(not worker.is_alive() for worker in self.workers)
        assert not self.errors


@contextmanager
def _server(tmp_path: Path, handler: _Handler = _reply) -> Iterator[_Server]:
    server = _Server(tmp_path / "p", handler)
    try:
        yield server
    finally:
        server.close()


def _config(**fields: object) -> Config:
    target = Target(pane_id="w1:p1", expected_agent="codex", expected_cwd="/work/project",
                    expected_workspace="project")
    document = as_mapping(json.loads(json.dumps(asdict(
        Config("spaces/test", ("users/owner",), target, "fixture-agent")))), "config")
    document.update(fields)
    return Config.parse(document)


def test_exact_utf8_request_and_one_connection_per_call(tmp_path: Path) -> None:
    with _server(tmp_path) as server:
        transport = SocketTransport(str(server.path))
        assert transport(_REQUEST) == _RESPONSE
        assert transport(_REQUEST) == _RESPONSE
        assert server.requests.get(timeout=1) == _REQUEST
        assert server.requests.get(timeout=1) == _REQUEST
        assert len(server.connections) == 2


@pytest.mark.parametrize("mode", [0o750, 0o707])
def test_parent_must_be_private_before_connecting(tmp_path: Path, mode: int) -> None:
    with _server(tmp_path) as server:
        server.path.parent.chmod(mode)
        with pytest.raises(HerdrUnavailable, match="private directory"):
            SocketTransport(str(server.path))(_REQUEST)
        assert server.requests.empty()


@pytest.mark.parametrize("kind", ["regular-file", "socket-symlink", "parent-symlink"])
def test_rejects_non_socket_and_symlink_endpoints(tmp_path: Path, kind: str) -> None:
    with _server(tmp_path) as server:
        if kind == "regular-file":
            path = server.path.parent / "ordinary"
            path.write_text("not a socket")
        elif kind == "socket-symlink":
            path = server.path.parent / "alias"
            path.symlink_to(server.path)
        else:
            alias = tmp_path / "alias"
            alias.symlink_to(server.path.parent, target_is_directory=True)
            path = alias / server.path.name
        with pytest.raises(HerdrUnavailable, match="private directory"):
            SocketTransport(str(path))(_REQUEST)
        assert server.requests.empty()


@pytest.mark.parametrize("wrong_owner", ["parent", "socket"])
def test_socket_and_parent_require_current_account(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, wrong_owner: str,
) -> None:
    with _server(tmp_path) as server:
        original = Path.lstat
        wrong_path = server.path.parent if wrong_owner == "parent" else server.path

        def lstat(path: Path) -> os.stat_result:
            info = original(path)
            if path == wrong_path:
                fields = list(info)
                fields[4] = os.getuid() + 1
                return os.stat_result(fields)
            return info

        monkeypatch.setattr(Path, "lstat", lstat)
        with pytest.raises(HerdrUnavailable, match="owned by this account"):
            SocketTransport(str(server.path))(_REQUEST)
        assert server.requests.empty()


@pytest.mark.skipif(not hasattr(socket, "SO_PEERCRED"), reason="peer credentials require SO_PEERCRED")
def test_peer_uid_is_checked_before_sending_credentials_or_text(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    with _server(tmp_path) as server:
        def wrong_peer(format: str, data: bytes) -> tuple[int, int, int]:
            assert format == "3i" and len(data) == 12
            return (os.getpid(), os.getuid() + 1, os.getgid())

        monkeypatch.setattr("agentctl.chat_socket.struct.unpack", wrong_peer)
        with pytest.raises(HerdrUnavailable, match="different account"):
            SocketTransport(str(server.path))(_REQUEST)
        assert server.requests.empty()


def test_adapter_error_remains_failure_without_echoing_private_diagnostics(tmp_path: Path) -> None:
    def fail(connection: socket.socket, request: dict[str, object]) -> None:
        connection.sendall(_wire({"error": "fixture-private-credential", "id": _RESPONSE["id"]}))

    with _server(tmp_path, fail) as server:
        with pytest.raises(HerdrUnavailable, match="reported failure") as caught:
            SocketTransport(str(server.path))(_REQUEST)
        assert "fixture-private-credential" not in str(caught.value)


@pytest.mark.parametrize("payload", [b"", b'{"id":"unfinished'])
def test_eof_before_complete_frame_is_unconfirmed(tmp_path: Path, payload: bytes) -> None:
    def incomplete(connection: socket.socket, request: dict[str, object]) -> None:
        connection.sendall(payload)

    with _server(tmp_path, incomplete) as server:
        with pytest.raises(HerdrUnavailable, match="before a complete response"):
            SocketTransport(str(server.path))(_REQUEST)


@pytest.mark.parametrize("payload", [b"not-json\n", b"[]\n", b"null\n", b"\xff\n", b"\n"])
def test_malformed_response_is_never_an_empty_success(tmp_path: Path, payload: bytes) -> None:
    def malformed(connection: socket.socket, request: dict[str, object]) -> None:
        connection.sendall(payload)

    with _server(tmp_path, malformed) as server:
        with pytest.raises((ValueError, TypeError, HerdrUnavailable)):
            SocketTransport(str(server.path))(_REQUEST)


def test_multiple_response_frames_are_refused(tmp_path: Path) -> None:
    def multiple(connection: socket.socket, request: dict[str, object]) -> None:
        connection.sendall(_wire(_RESPONSE) + _wire({"id": "spaces/test/messages/other"}))

    with _server(tmp_path, multiple) as server:
        with pytest.raises(HerdrUnavailable, match="more than one response"):
            SocketTransport(str(server.path))(_REQUEST)


def test_oversized_request_is_refused_before_connection(tmp_path: Path) -> None:
    with _server(tmp_path) as server:
        with pytest.raises(ValueError, match="request exceeds"):
            SocketTransport(str(server.path))({"text": "x" * (1024 * 1024)})
        assert server.requests.empty()
        assert not server.connections


def test_response_bound_applies_without_a_newline(tmp_path: Path) -> None:
    def oversized(connection: socket.socket, request: dict[str, object]) -> None:
        chunk = b"x" * 65536
        for _ in range(129):
            connection.sendall(chunk)

    with _server(tmp_path, oversized) as server:
        with pytest.raises(HerdrUnavailable, match="response exceeds"):
            SocketTransport(str(server.path))(_REQUEST)


def test_idle_socket_times_out_and_closes_connection(tmp_path: Path) -> None:
    closed = threading.Event()

    def idle(connection: socket.socket, request: dict[str, object]) -> None:
        assert connection.recv(1) == b""
        closed.set()

    with _server(tmp_path, idle) as server:
        with pytest.raises(TimeoutError):
            SocketTransport(str(server.path), timeout=0.05)(_REQUEST)
        assert closed.wait(1)


def test_slow_trickle_cannot_extend_total_response_deadline(tmp_path: Path) -> None:
    def trickle(connection: socket.socket, request: dict[str, object]) -> None:
        connection.sendall(b'{"id":"')
        for _ in range(30):
            time.sleep(0.02)
            connection.sendall(b"x")
        connection.sendall(b'"}\n')

    with _server(tmp_path, trickle) as server:
        start = time.monotonic()
        with pytest.raises(TimeoutError):
            SocketTransport(str(server.path), timeout=0.12)(_REQUEST)
        assert time.monotonic() - start < 0.5


def test_slow_ack_does_not_block_parallel_send(tmp_path: Path) -> None:
    ack_started, release_ack = threading.Event(), threading.Event()

    def independently(connection: socket.socket, request: dict[str, object]) -> None:
        if request["action"] == "react":
            ack_started.set()
            assert release_ack.wait(2)
            connection.sendall(_wire({"id": "spaces/test/messages/source/reactions/robot"}))
        else:
            connection.sendall(_wire(_RESPONSE))

    with _server(tmp_path, independently) as server, ThreadPoolExecutor(2) as workers:
        transport = SocketTransport(str(server.path), timeout=2)
        slow = workers.submit(transport, {"action": "react", "emoji": "🤖"})
        try:
            assert ack_started.wait(1)
            fast = workers.submit(transport, _REQUEST)
            assert fast.result(timeout=1) == _RESPONSE
            assert not slow.done()
        finally:
            release_ack.set()
        assert slow.result(timeout=1) == {"id": "spaces/test/messages/source/reactions/robot"}


def test_lost_response_does_not_retry_or_change_request_identity(tmp_path: Path) -> None:
    calls = 0

    def lose_first(connection: socket.socket, request: dict[str, object]) -> None:
        nonlocal calls
        calls += 1
        if calls > 1:
            connection.sendall(_wire(_RESPONSE))

    with _server(tmp_path, lose_first) as server:
        transport = SocketTransport(str(server.path))
        with pytest.raises(HerdrUnavailable, match="before a complete response"):
            transport(_REQUEST)
        assert calls == 1
        assert transport(_REQUEST) == _RESPONSE
        assert server.requests.get(timeout=1) == server.requests.get(timeout=1) == _REQUEST


@pytest.mark.parametrize("choice", ["rest", "command", "socket"])
def test_stream_config_coexists_with_each_output_transport(tmp_path: Path, choice: str) -> None:
    fields: dict[str, object] = {"event_command": ["/absolute/adapter", "argument with spaces"]}
    expected: type[GoogleChatTransport | CommandTransport | SocketTransport] = GoogleChatTransport
    if choice == "command":
        fields["transport_command"] = ["/absolute/transport"]
        expected = CommandTransport
    elif choice == "socket":
        fields["transport_socket"] = str(tmp_path / "adapter.sock")
        expected = SocketTransport
    config = _config(**fields)
    assert config.event_command == ("/absolute/adapter", "argument with spaces")
    state = tmp_path / "state"
    Bridge.initialize(state, config, after="2026-01-01T00:00:00Z")
    bridge = Bridge(state)
    assert isinstance(bridge.transport, expected)
    assert bridge.config == config


@pytest.mark.parametrize("fields", [
    {"transport_socket": "relative.sock"},
    {"transport_socket": ""},
    {"transport_socket": "/absolute/invalid\0socket"},
    {"transport_socket": 123},
    {"transport_socket": "/absolute/adapter.sock", "transport_command": ["adapter"]},
    {"event_command": "not an argv array"},
    {"event_command": ["adapter", ""]},
    {"event_command": [123]},
])
def test_invalid_stream_and_socket_configuration_is_refused(fields: dict[str, object]) -> None:
    with pytest.raises((ValueError, TypeError)):
        _config(**fields)


def test_empty_event_command_preserves_rest_polling_default() -> None:
    assert _config().event_command == ()
    assert _config(event_command=[]).event_command == ()
    assert _config(event_command=None).event_command == ()
    assert _config(transport_socket=None).transport_socket is None
