"""Strict encoded bounds for Chat configuration, state, REST, and logs."""

from __future__ import annotations

import io
import os
import subprocess
import threading
from pathlib import Path
from urllib.request import Request

import pytest

import agentctl.agent as agent_module
import agentctl.chat as chat_module
import agentctl.chat_storage as storage
from agentctl.chat import Config, GoogleChatTransport
from agentctl.chat_runtime import _Runtime
from agentctl.chat_storage import (
    CONFIG, OUTPUT, REPLY_INPUT, ArtifactClass,
    encoded_json, read_json, read_text, write_json,
)
from agentctl.errors import AgentDeliveryError
from tests.test_agentctl_chat_runtime import Rig, _message


def _config(**changes: object) -> dict[str, object]:
    document: dict[str, object] = {
        "space": "spaces/test",
        "allowed_senders": ["users/owner"],
        "target": {
            "pane_id": "w1:p1", "expected_agent": "codex",
            "expected_workspace": "workspace", "expected_cwd": "/work/project",
        },
        "agent_label": "coordinator",
    }
    document.update(changes)
    return document


def test_artifact_writer_accepts_cap_and_refuses_cap_plus_one(tmp_path: Path) -> None:
    document: dict[str, object] = {"value": "escaped \x00 data"}
    encoded = encoded_json(document, ArtifactClass("probe", 1 << 20))
    path = tmp_path / "probe.json"
    write_json(path, document, ArtifactClass("probe", len(encoded)))
    assert read_json(path, ArtifactClass("probe", len(encoded))) == (document, len(encoded))
    with pytest.raises(ValueError, match="encoded file limit"):
        write_json(tmp_path / "large.json", document, ArtifactClass("probe", len(encoded) - 1))
    assert not (tmp_path / "large.json").exists()


@pytest.mark.parametrize("document", [{"value": float("nan")}, {"value": float("inf")}])
def test_artifact_writer_refuses_nonfinite_before_creating_file(
    tmp_path: Path, document: dict[str, object],
) -> None:
    path = tmp_path / "invalid.json"
    with pytest.raises(ValueError, match="finite JSON"):
        write_json(path, document, ArtifactClass("probe", 1024))
    assert not path.exists()


def test_artifact_writer_refuses_excessive_depth_before_creating_file(tmp_path: Path) -> None:
    nested: object = 0
    for _ in range(66):
        nested = {"value": nested}
    path = tmp_path / "deep.json"
    with pytest.raises(ValueError, match="finite JSON"):
        write_json(path, {"value": nested}, ArtifactClass("probe", 1 << 20))
    assert not path.exists()


@pytest.mark.parametrize(
    ("path", "expected"),
    [
        (Path("/state/bridge.json"), storage.BRIDGE),
        (Path("/state/input.json"), storage.INPUT),
        (Path("/state/output.json"), storage.OUTPUT),
        (Path("/state/requests/key.json"), storage.REQUEST),
        (Path("/state/feedback/key.json"), storage.FEEDBACK),
        (Path("/state/deferred/key.json"), storage.DEFERRED),
        (Path("/state/queue/inbox/key.json"), storage.QUEUE),
        (Path("/state/replies/items/aa/bb/key/pending/0000/000000.json"), storage.REPLY),
        (Path("/state/submissions/key.json"), storage.SUBMISSION),
        (Path("/state/reply-receipts/aa/key.json"), storage.RECEIPT),
        (Path("/state/replies/key.json"), storage.LEGACY_REPLY),
        (Path("/items/queue/state/replies/key.json"), storage.LEGACY_REPLY),
        (Path("/reply-receipts/state/replies/items/aa/bb/key/pending/0000/000000.json"),
         storage.REPLY),
    ],
)
def test_chat_path_artifact_classes_have_explicit_caps(
    path: Path, expected: ArtifactClass,
) -> None:
    assert storage.artifact_for_path(path) == expected


@pytest.mark.parametrize(
    "encoded",
    [
        b'{"value":1,"value":2}',
        b'{"value":NaN}',
        b'{"value":1e9999}',
        (b'{"x":' * 66) + b"0" + (b"}" * 66),
    ],
    ids=("duplicate", "nonfinite-token", "nonfinite-overflow", "depth"),
)
def test_artifact_reader_rejects_noncanonical_json(tmp_path: Path, encoded: bytes) -> None:
    path = tmp_path / "invalid.json"
    path.write_bytes(encoded)
    path.chmod(0o600)
    with pytest.raises(AgentDeliveryError):
        read_json(path, ArtifactClass("probe", len(encoded) + 1))


def test_duplicate_key_diagnostic_does_not_echo_an_unbounded_key(tmp_path: Path) -> None:
    key = "x" * 20_000
    encoded = ('{"' + key + '":1,"' + key + '":2}').encode()
    path = tmp_path / "duplicate.json"
    path.write_bytes(encoded)
    path.chmod(0o600)
    with pytest.raises(AgentDeliveryError) as caught:
        read_json(path, ArtifactClass("probe", len(encoded)))
    assert len(str(caught.value).encode("utf-8")) < 1000


def test_artifact_reader_refuses_symlink_hardlink_fifo_and_device(tmp_path: Path) -> None:
    artifact = ArtifactClass("probe", 1024)
    source = tmp_path / "source.json"
    source.write_text("{}", encoding="utf-8")
    source.chmod(0o600)
    symlink = tmp_path / "symlink.json"
    symlink.symlink_to(source)
    hardlink = tmp_path / "hardlink.json"
    os.link(source, hardlink)
    fifo = tmp_path / "fifo.json"
    os.mkfifo(fifo, 0o600)
    for path in (symlink, hardlink, fifo, Path(os.devnull)):
        with pytest.raises(AgentDeliveryError):
            read_json(path, artifact)


def test_reply_input_accepts_ordinary_mode_at_cap_and_refuses_cap_plus_one(
    tmp_path: Path,
) -> None:
    at_cap = tmp_path / "at-cap.txt"
    at_cap.write_bytes(b"x" * REPLY_INPUT.max_bytes)
    at_cap.chmod(0o644)
    assert read_text(at_cap, REPLY_INPUT) == "x" * REPLY_INPUT.max_bytes

    over_cap = tmp_path / "over-cap.txt"
    over_cap.write_bytes(b"x" * (REPLY_INPUT.max_bytes + 1))
    over_cap.chmod(0o644)
    with pytest.raises(AgentDeliveryError, match="exceeds its 30000-byte limit"):
        read_text(over_cap, REPLY_INPUT)


def test_reply_input_refuses_huge_file_before_reading(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    path = tmp_path / "huge.txt"
    path.write_bytes(b"x" * (1 << 20))
    path.chmod(0o644)

    def unexpected_read(descriptor: int, count: int) -> bytes:
        del descriptor, count
        raise AssertionError("an initially oversized input must not be read")

    monkeypatch.setattr(os, "read", unexpected_read)
    with pytest.raises(AgentDeliveryError, match="exceeds its 30000-byte limit"):
        read_text(path, REPLY_INPUT)


def test_reply_input_refuses_symlink_hardlink_fifo_and_device_without_blocking(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    source = tmp_path / "source.txt"
    source.write_text("answer", encoding="utf-8")
    source.chmod(0o644)
    symlink = tmp_path / "symlink.txt"
    symlink.symlink_to(source)
    hardlink = tmp_path / "hardlink.txt"
    os.link(source, hardlink)
    fifo = tmp_path / "reply.fifo"
    os.mkfifo(fifo, 0o600)

    original_open = os.open
    fifo_opened_nonblocking = False

    def checked_open(path: str | bytes | os.PathLike[str], flags: int) -> int:
        nonlocal fifo_opened_nonblocking
        if path == fifo:
            assert flags & os.O_NONBLOCK
            assert flags & os.O_NOFOLLOW
            fifo_opened_nonblocking = True
        return original_open(path, flags)

    monkeypatch.setattr(os, "open", checked_open)
    for path in (symlink, hardlink, fifo, Path(os.devnull)):
        with pytest.raises(AgentDeliveryError):
            read_text(path, REPLY_INPUT)
    assert fifo_opened_nonblocking


def test_reply_input_refuses_invalid_utf8(tmp_path: Path) -> None:
    path = tmp_path / "invalid.txt"
    path.write_bytes(b"valid prefix\xff")
    path.chmod(0o644)
    with pytest.raises(AgentDeliveryError, match="cannot read Chat reply input"):
        read_text(path, REPLY_INPUT)


def test_reply_input_refuses_group_or_world_writable_file(tmp_path: Path) -> None:
    path = tmp_path / "shared-write.txt"
    path.write_text("answer", encoding="utf-8")
    path.chmod(0o666)
    with pytest.raises(AgentDeliveryError, match="writable by another account"):
        read_text(path, REPLY_INPUT)


def test_reply_input_refuses_open_file_growth(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    path = tmp_path / "growing.txt"
    path.write_text("answer", encoding="utf-8")
    path.chmod(0o644)
    original_read = os.read
    grew = False

    def grow_after_first_read(descriptor: int, count: int) -> bytes:
        nonlocal grew
        block = original_read(descriptor, count)
        if not grew:
            grew = True
            append = os.open(path, os.O_WRONLY | os.O_APPEND)
            try:
                os.write(append, b"x" * (REPLY_INPUT.max_bytes + 1))
            finally:
                os.close(append)
        return block

    monkeypatch.setattr(os, "read", grow_after_first_read)
    with pytest.raises(AgentDeliveryError, match="exceeds its 30000-byte limit"):
        read_text(path, REPLY_INPUT)
    assert grew and path.stat().st_size > REPLY_INPUT.max_bytes


def test_reply_input_refuses_unlinked_open_inode_after_path_replacement(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    path = tmp_path / "answer.txt"
    path.write_text("original answer", encoding="utf-8")
    path.chmod(0o644)
    replacement = tmp_path / "replacement.txt"
    replacement.write_bytes(b"x" * (REPLY_INPUT.max_bytes + 1))
    replacement.chmod(0o644)
    original_fstat = os.fstat
    replaced = False

    def replace_after_first_fstat(descriptor: int) -> os.stat_result:
        nonlocal replaced
        metadata = original_fstat(descriptor)
        if not replaced:
            replaced = True
            os.replace(replacement, path)
        return metadata

    monkeypatch.setattr(os, "fstat", replace_after_first_fstat)
    with pytest.raises(AgentDeliveryError, match="changed while it was read"):
        read_text(path, REPLY_INPUT)
    assert replaced and path.stat().st_size > REPLY_INPUT.max_bytes


@pytest.mark.parametrize("reader", ["queue-json", "state-json", "cli-text"])
@pytest.mark.parametrize("remove_link", [False, True])
def test_bounded_read_refuses_hardlink_created_during_read_even_if_removed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, reader: str, remove_link: bool,
) -> None:
    path = tmp_path / ("input.txt" if reader == "cli-text" else "input.json")
    path.write_bytes(b"answer" if reader == "cli-text" else b'{"value":"answer"}')
    path.chmod(0o600)
    original_metadata = path.stat()
    extra_link = tmp_path / "raced-link"
    original_read = os.read
    linked = False

    def race_after_read(descriptor: int, count: int) -> bytes:
        nonlocal linked
        block = original_read(descriptor, count)
        if not linked:
            linked = True
            os.link(path, extra_link)
            if remove_link:
                extra_link.unlink()
        return block

    monkeypatch.setattr(os, "read", race_after_read)
    with pytest.raises(AgentDeliveryError, match="changed while it was read"):
        if reader == "queue-json":
            agent_module._read_bounded_queue_json(
                str(path), "queue race test", require_private=True, max_artifact_bytes=1024)
        elif reader == "state-json":
            read_json(path, ArtifactClass("state race test", 1024))
        else:
            read_text(path, REPLY_INPUT)
    assert linked and path.stat().st_nlink == (1 if remove_link else 2)
    assert path.stat().st_ctime_ns != original_metadata.st_ctime_ns
    assert path.stat().st_mtime_ns == original_metadata.st_mtime_ns


def test_reply_cli_bounds_file_before_submission(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    request = "a" * 64
    at_cap = tmp_path / "at-cap.txt"
    at_cap.write_bytes(b"x" * REPLY_INPUT.max_bytes)
    at_cap.chmod(0o644)
    submitted: list[tuple[Path, str, str]] = []

    def submit(state: Path, key: str, text: str) -> None:
        submitted.append((state, key, text))

    monkeypatch.setattr(chat_module, "submit_reply", submit)
    state = tmp_path / "state"
    assert chat_module.run_cli([
        "reply", "--state", str(state), "--request", request, "--file", str(at_cap),
    ]) == 0
    assert submitted == [(state, request, "x" * REPLY_INPUT.max_bytes)]

    over_cap = tmp_path / "over-cap.txt"
    over_cap.write_bytes(b"x" * (REPLY_INPUT.max_bytes + 1))
    over_cap.chmod(0o644)
    assert chat_module.run_cli([
        "reply", "--state", str(state), "--request", request, "--file", str(over_cap),
    ]) == 1
    assert submitted == [(state, request, "x" * REPLY_INPUT.max_bytes)]


def test_external_config_requires_private_file_and_enforces_cap(tmp_path: Path) -> None:
    path = tmp_path / "chat.json"
    path.write_text("{}", encoding="utf-8")
    with pytest.raises(AgentDeliveryError, match="is not private"):
        read_json(path, CONFIG)
    path.chmod(0o600)
    path.write_bytes(b" " * (CONFIG.max_bytes + 1))
    with pytest.raises(AgentDeliveryError, match="exceeds max_artifact_bytes"):
        read_json(path, CONFIG)


def test_oversized_saved_observer_state_is_refused(tmp_path: Path) -> None:
    path = tmp_path / "output.json"
    path.write_bytes(b'{"error":"' + b"x" * OUTPUT.max_bytes + b'"}')
    path.chmod(0o600)
    with pytest.raises(AgentDeliveryError, match="exceeds max_artifact_bytes"):
        chat_module._OutputObserver(path, 60)


@pytest.mark.parametrize(
    "change",
    [
        {"allowed_senders": [f"users/u{index}" for index in range(257)]},
        {"allowed_senders": ["users/owner", "users/owner"]},
        {"allowed_senders": ["users/" + "x" * 300]},
        {"event_command": ["event"] * 129},
        {"event_command": ["x" * (8192 + 1)]},
        {"event_command": ["x" * 8192] * 17},
        {"agent_label": "x" * (512 << 10)},
    ],
    ids=(
        "sender-count", "sender-duplicate", "sender-item", "argv-count", "argv-item",
        "argv-aggregate", "config-aggregate",
    ),
)
def test_config_population_and_byte_limits(change: dict[str, object]) -> None:
    with pytest.raises(ValueError):
        Config.parse(_config(**change))


def test_config_builds_cached_constant_time_sender_index() -> None:
    config = Config.parse(_config(allowed_senders=["users/owner", "users/operator"]))
    first = config.allowed_sender_set
    assert first == frozenset(("users/owner", "users/operator"))
    assert config.allowed_sender_set is first


def test_malformed_disallowed_message_cannot_advance_cursor(tmp_path: Path) -> None:
    rig = Rig(tmp_path / "state")
    try:
        malformed = _message("bad", sender="users/stranger")
        malformed["unexpected"] = {"nested": ["data"]}
        with pytest.raises(ValueError, match="unsupported fields: unexpected"):
            rig.runtime._input_event({
                "type": "message", "message": malformed, "cursor": "must-not-advance",
            })
        assert not (rig.state / "input.json").exists()
        assert list((rig.state / "requests").glob("*.json")) == []
    finally:
        rig.finish()


def test_malformed_disallowed_poll_message_cannot_advance_cursor(tmp_path: Path) -> None:
    rig = Rig(tmp_path / "state")
    try:
        checkpoint = chat_module._read(rig.state / "bridge.json")
        before = dict(checkpoint)
        malformed = _message("bad-poll", sender="users/stranger")
        malformed["unexpected"] = True
        with pytest.raises(ValueError, match="unsupported fields: unexpected"):
            rig.bridge._ingest_result(
                {"messages": [malformed], "cursor": "must-not-advance"}, checkpoint)
        assert checkpoint == before
        assert chat_module._read(rig.state / "bridge.json") == before
        assert list((rig.state / "requests").glob("*.json")) == []
    finally:
        rig.finish()


def test_message_schema_error_does_not_echo_an_unbounded_field_population() -> None:
    message = _message("bounded-error")
    message.update({f"extra-{index}": index for index in range(10_000)})
    with pytest.raises(ValueError) as caught:
        chat_module.Bridge._validate_message_shape(message)
    assert str(caught.value).endswith("10000 fields")
    assert len(str(caught.value)) < 100


class _Http:
    def __init__(self, payload: bytes) -> None:
        self.payload = payload
        self.calls = 0

    def __call__(self, request: Request, *, timeout: float) -> io.BytesIO:
        del request, timeout
        self.calls += 1
        return io.BytesIO(self.payload)


def test_rest_response_token_and_cursor_limits(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(chat_module, "_MAX_REST_RESPONSE_BYTES", 64)
    http = _Http(b"{" + b"x" * 64)
    monkeypatch.setattr(chat_module, "urlopen", http)
    monkeypatch.setenv("CHAT_BOUND_TOKEN", "token")
    transport = GoogleChatTransport("CHAT_BOUND_TOKEN")
    request: dict[str, object] = {
        "action": "poll", "space": "spaces/test",
        "after": "2026-01-01T00:00:00Z", "cursor": None,
    }
    with pytest.raises(ValueError, match="response exceeds 64 bytes"):
        transport(request)
    assert http.calls == 1

    monkeypatch.setenv("CHAT_BOUND_TOKEN", "x" * ((16 << 10) + 1))
    with pytest.raises(ValueError, match="access token exceeds"):
        transport(request)
    assert http.calls == 1

    monkeypatch.setenv("CHAT_BOUND_TOKEN", "token")
    request["cursor"] = "x" * ((8 << 10) + 1)
    with pytest.raises(ValueError, match="cursor must not exceed"):
        transport(request)
    assert http.calls == 1


@pytest.mark.parametrize(
    "payload",
    [
        b'{"messages":[],"messages":[]}',
        b'{"value":NaN}',
        b'{"value":1e9999}',
        (b'{"x":' * 66) + b"0" + (b"}" * 66),
    ],
    ids=("duplicate", "nonfinite-token", "nonfinite-overflow", "depth"),
)
def test_rest_response_refuses_noncanonical_json(
    monkeypatch: pytest.MonkeyPatch, payload: bytes,
) -> None:
    monkeypatch.setattr(chat_module, "urlopen", _Http(payload))
    with pytest.raises(ValueError, match="valid bounded JSON object"):
        GoogleChatTransport._http("token", "spaces/test/messages", {})


def test_token_helper_uses_credential_sized_stdout_bound(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    observed: list[int] = []

    def run(
        command: object, *, timeout: float, input_text: str | None = None,
        stdout_limit: int | None = None,
    ) -> subprocess.CompletedProcess[str]:
        del command, timeout, input_text
        assert stdout_limit is not None
        observed.append(stdout_limit)
        return subprocess.CompletedProcess(("token",), 0, "credential\n", "")

    def http(
        token: str, path: str, params: dict[str, str], data: bytes | None = None,
    ) -> dict[str, object]:
        del token, path, params, data
        return {"messages": []}

    monkeypatch.setattr(chat_module, "_run_command", run)
    monkeypatch.setattr(GoogleChatTransport, "_http", staticmethod(http))
    transport = GoogleChatTransport(token_command=("token-helper",))
    assert transport({
        "action": "poll", "space": "spaces/test",
        "after": "2026-01-01T00:00:00Z", "cursor": None,
    }) == {"messages": [], "cursor": None}
    assert observed == [(16 << 10) + 2]


def test_saved_poll_cursor_is_bounded_before_adapter_request(tmp_path: Path) -> None:
    rig = Rig(tmp_path / "state")
    try:
        with pytest.raises(ValueError, match="saved poll cursor must not exceed"):
            rig.bridge._poll_request({
                "after": "2026-01-01T00:00:00Z", "high_water": "2026-01-01T00:00:00Z",
                "started_at": "2026-01-01T00:00:00Z",
                "cursor": "x" * ((8 << 10) + 1),
            })
        saved = chat_module._read(rig.state / "bridge.json")
        saved["cursor"] = "x" * ((8 << 10) + 1)
        chat_module._write(rig.state / "bridge.json", saved)
        with pytest.raises(ValueError, match="saved poll cursor must not exceed"):
            chat_module.Bridge(rig.state, rig.harness, rig.transport)
    finally:
        rig.finish()


def test_saved_stream_cursor_is_bounded_before_runtime_resources(tmp_path: Path) -> None:
    rig = Rig(tmp_path / "state")
    try:
        chat_module._write(rig.state / "input.json", {
            "cursor": "x" * ((8 << 10) + 1), "state": "connected", "error": None,
        })
        with pytest.raises(ValueError, match="saved stream cursor must not exceed"):
            _Runtime(rig.bridge, 300, "test", threading.Event())
    finally:
        rig.finish()


def test_launch_log_rotation_bounds_files_and_returns_only_suffix(tmp_path: Path) -> None:
    source, destination = os.pipe()
    failures: list[str] = []
    path = tmp_path / "bridge.log"
    payload = bytes(index % 251 for index in range((2 << 20) + (256 << 10)))
    worker = threading.Thread(
        target=chat_module._drain_launch_log, args=(source, path, failures), daemon=True)
    worker.start()
    offset = 0
    while offset < len(payload):
        offset += os.write(destination, payload[offset:offset + (64 << 10)])
    os.close(destination)
    worker.join(timeout=10)
    assert not worker.is_alive() and failures == []
    previous = path.with_name("bridge.log.1")
    assert path.stat().st_size <= 1 << 20
    assert previous.stat().st_size <= 1 << 20
    assert path.stat().st_size + previous.stat().st_size <= 2 << 20
    expected = payload[-2000:].decode("utf-8", errors="replace").strip()
    assert chat_module._launch_log_detail(path) == expected


def test_launch_log_refuses_hardlink_before_truncation_and_keeps_draining(
    tmp_path: Path,
) -> None:
    victim = tmp_path / "victim"
    victim.write_bytes(b"must survive")
    victim.chmod(0o600)
    path = tmp_path / "bridge.log"
    os.link(victim, path)
    source, destination = os.pipe()
    failures: list[str] = []
    worker = threading.Thread(
        target=chat_module._drain_launch_log, args=(source, path, failures), daemon=True)
    worker.start()
    os.write(destination, b"diagnostic")
    os.close(destination)
    worker.join(timeout=5)
    assert not worker.is_alive()
    assert failures and "unsafe Chat launch log" in failures[0]
    assert victim.read_bytes() == b"must survive"
