"""Operation-count regressions for cached subscriptions and bounded recovery."""

from __future__ import annotations

from collections import Counter
from collections.abc import Iterator, Sequence
from dataclasses import replace
from pathlib import Path
import threading
import time

import pytest

import agentctl.agent as agent_module
import agentctl.chat as chat_module
import agentctl.chat_runtime as runtime_module
from agentctl.chat import Bridge, _OutputSubscription, _read, _write
from agentctl.chat_output import PaneAgentStatus, PaneOutputSnapshot, PaneOutputStream
from agentctl.chat_runtime import _Notice
from agentctl.agent import AtomicWritePolicy, QueueResult, Target
from agentctl.client import HerdrClient
from agentctl.jsonx import as_mapping
from tests.test_agentctl_chat_runtime import Rig, _key, _message
from tests.test_agentctl_chat_tagged import _Clock, _FakeStream, _StopLoop, _block, _snapshot
from tests.test_herdr_chat import setup


@pytest.fixture
def rig(tmp_path: Path) -> Iterator[Rig]:
    instance = Rig(tmp_path / "bridge")
    try:
        yield instance
    finally:
        instance.finish()


class _BlockingStream(PaneOutputStream):
    def __init__(self, events: list[PaneOutputSnapshot | PaneAgentStatus] | None = None) -> None:
        self.events = [] if events is None else events
        self.notified = threading.Event()

    def wait(self, timeout: float) -> tuple[PaneOutputSnapshot | PaneAgentStatus, ...]:
        self.notified.wait(timeout)
        self.notified.clear()
        result = tuple(self.events)
        self.events.clear()
        return result

    def wake(self) -> None:
        self.notified.set()

    def close(self) -> None:
        self.wake()


def test_marker_advance_reopens_real_cached_path_without_request_path_operations(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    rig.accept()
    rig.until(lambda: rig.phase() == "awaiting_reply" and not rig.runtime.jobs)
    pane = rig.bridge.config.target.pane_id
    assert pane is not None
    opened: list[tuple[str, ...]] = []
    stream = _BlockingStream()

    def constructor(socket_path: str, pane_id: str, pattern: Sequence[str], *,
                    watch_settled: bool = False) -> PaneOutputStream:
        assert socket_path == "test-events" and pane_id == pane and watch_settled
        opened.append(tuple(pattern))
        return stream

    monkeypatch.setattr(rig.harness, "event_socket", lambda: "test-events")
    monkeypatch.setattr(chat_module, "PaneOutputStream", constructor)
    record = rig.runtime.records[_key()][1]
    nonce = str(record["reply_nonce"])
    event = PaneOutputSnapshot(pane, _block(record, "first"), False)
    original_stat, original_glob, original_read = Path.stat, Path.glob, chat_module._read

    def in_pump() -> bool:
        return threading.current_thread() is rig.runtime.output.thread

    def stat(path: Path, *, follow_symlinks: bool = True) -> object:
        assert not (in_pump() and "requests" in path.parts), "cached open touched request metadata"
        return original_stat(path, follow_symlinks=follow_symlinks)

    def glob(path: Path, pattern: str) -> Iterator[Path]:
        assert not (in_pump() and path == rig.state / "requests"), "cached open scanned request directory"
        return original_glob(path, pattern)

    def read(path: Path) -> dict[str, object]:
        assert not (in_pump() and path.parent == rig.state / "requests"), "cached open read a request"
        return original_read(path)

    with monkeypatch.context() as guarded:
        guarded.setattr(Path, "stat", stat)
        guarded.setattr(Path, "glob", glob)
        guarded.setattr(chat_module, "_read", read)
        rig.runtime.output.thread.start()
        rig.until(lambda: bool(opened))
        rig.runtime._handle(_Notice("output", event))
        rig.runtime._stage()
        rig.until(lambda: rig.phase() == "replied" and not rig.runtime.jobs and len(opened) == 2)
    assert opened[-1] == chat_module._closing_patterns((nonce + "_2",))


def test_cached_subscription_authority_and_public_validation_fail_closed(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    subscription = rig.bridge._output_subscription([])

    def forbidden() -> str:
        raise AssertionError("invalid authority reached the event socket")

    monkeypatch.setattr(rig.harness, "event_socket", forbidden)
    with pytest.raises(ValueError, match="not authorized"):
        rig.bridge._open_cached_output(replace(subscription, owner=Bridge(rig.state)))
    with pytest.raises(ValueError, match="outside the authorized"):
        rig.bridge._open_cached_output(replace(subscription, markers=("A" * 22 + "_1",)))
    with pytest.raises(ValueError, match="not authorized"):
        rig.bridge._open_cached_output(replace(subscription, watching=False))
    # Public/one-shot callers cannot reuse a prior cache to conceal corrupt disk.
    _write(rig.state / "requests" / ("a" * 64 + ".json"), {"key": "bad"})
    with pytest.raises(ValueError):
        rig.bridge.open_output(())


def _pending_requests(rig: Rig, count: int, per_request: int = 2) -> None:
    rig.bridge._ingest_result({"messages": [_message(f"request-{i}") for i in range(count)]})
    for index in range(count):
        path = rig.state / "requests" / f"{_key(f'request-{index}')}.json"
        record = _read(path)
        body_bytes = 0
        for ordinal in range(1, per_request + 1):
            body = f"answer-{index}-{ordinal}"
            body_bytes += len(body.encode())
            rig.bridge._create_reply_item(record, ordinal - 1, {
                "reply_key": str(ordinal - 1), "reply_ordinal": ordinal,
                "request_id": rig.bridge._reply_identity(record, ordinal - 1),
                "reply_source": "capture", "text": body,
            })
        record.update(phase="reply_pending", reply_storage=2,
                      reply_item_count=per_request, reply_sent_count=0,
                      reply_total_bytes=body_bytes, reply_pending_bytes=body_bytes,
                      reply_next_ordinal=per_request + 1)
        _write(path, record)


@pytest.mark.parametrize("size", [32, 128])
def test_reply_index_rebuild_visits_linear_number_of_counter_keys(
    rig: Rig, size: int, monkeypatch: pytest.MonkeyPatch,
) -> None:
    _pending_requests(rig, size)
    visits = 0

    class CountedCounter(Counter[str]):
        def __getitem__(self, key: str) -> int:
            nonlocal visits
            visits += 1
            return super().__getitem__(key)

    original_iadd = Counter[str].__iadd__

    def iadd(counter: Counter[str], other: Counter[str]) -> Counter[str]:
        nonlocal visits
        # Counter's in-place addition sweeps every retained key to purge
        # nonpositive counts, even when adding an empty counter.
        visits += len(counter)
        return original_iadd(counter, other)

    monkeypatch.setattr(CountedCounter, "__iadd__", iadd)
    rig.runtime.pending_texts = CountedCounter()
    rig.runtime._reload_records()
    assert len(rig.runtime.pending_texts) == size * 2
    for key in rig.runtime.record_order[::2]:
        rig.runtime._refresh_reply_index(key, [{"text": "shared"}])
    assert rig.runtime.pending_texts["[fixture-agent] shared"] == size // 2
    for key in rig.runtime.record_order[::2]:
        rig.runtime._refresh_reply_index(key, [])
    assert "[fixture-agent] shared" not in rig.runtime.pending_texts
    assert len(rig.runtime.pending_texts) == size
    assert visits <= size * 12


@pytest.mark.parametrize("limit", [
    "_MAX_PENDING_REPLY_ITEMS", "_MAX_PENDING_REPLY_BYTES",
    "_MAX_REQUEST_REPLY_ITEMS", "_MAX_REQUEST_REPLY_BYTES",
    "_MAX_STATE_REPLY_ITEMS", "_MAX_STATE_REPLY_BYTES",
])
@pytest.mark.parametrize("entry", ["migration", "runtime"])
def test_current_reply_caps_refuse_before_pending_body_materialization(
    rig: Rig, monkeypatch: pytest.MonkeyPatch, limit: str, entry: str,
) -> None:
    _pending_requests(rig, 2)
    monkeypatch.setattr(chat_module, limit, 1)
    reads: list[Path] = []
    original = chat_module._read

    def read(path: Path) -> dict[str, object]:
        if "items" in path.parts:
            reads.append(path)
            raise AssertionError("over-limit recovery materialized a pending body")
        return original(path)

    monkeypatch.setattr(chat_module, "_read", read)
    with pytest.raises(ValueError, match="storage limits"):
        if entry == "migration":
            rig.bridge.migrate_reply_outboxes()
        else:
            rig.runtime._reload_records()
    assert reads == []


def test_deferred_release_preflights_one_batch_and_delivers_without_recovery(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    rig.bridge._ingest_result({"messages": [_message(f"old-{i}") for i in range(64)]})
    rig.runtime._reload_records()
    for index in range(48):
        message = _message(f"deferred-{index}", text="possible echo")
        rig.runtime.pending_texts["possible echo"] = 1
        rig.runtime._accept_message(message)
    rig.runtime.pending_texts.clear()
    rig.runtime.deferred_dirty = True
    calls: list[tuple[int, int]] = []
    original = rig.bridge._ingest_result

    def ingest(result: dict[str, object], checkpoint: dict[str, object] | None = None,
               records: Sequence[dict[str, object]] | None = None,
               own_replies: set[object] | None = None) -> None:
        messages = result["messages"]
        assert isinstance(messages, list) and records is not None
        calls.append((len(records), len(messages)))
        original(result, checkpoint, records, own_replies)

    monkeypatch.setattr(rig.bridge, "_ingest_result", ingest)
    rig.runtime._stage()
    assert calls == [(64, 48)]
    assert len(rig.runtime.records) == 112
    assert rig.runtime.deferred_messages == {}
    assert not list((rig.state / "deferred").glob("*.json"))
    rig.until(lambda: len(rig.harness.prompts) == 112 and not rig.runtime.jobs)


def test_deferred_batch_cap_refusal_preserves_all_sources_and_request_population(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    for index in range(2):
        rig.runtime.pending_texts["possible echo"] = 1
        rig.runtime._accept_message(_message(f"deferred-{index}", text="possible echo"))
    before = {path: path.read_bytes() for path in rig.runtime.deferred_messages}
    rig.runtime.pending_texts.clear()
    rig.runtime.deferred_dirty = True
    monkeypatch.setattr(chat_module, "_MAX_REQUEST_RECORDS", 1)
    with pytest.raises(ValueError, match="request record limit"):
        rig.runtime._stage()
    assert {path: path.read_bytes() for path in rig.runtime.deferred_messages} == before
    assert not list((rig.state / "requests").glob("*.json"))
    assert rig.runtime.deferred_dirty


@pytest.mark.parametrize("limit", ["records", "source-bytes", "encoded-bytes"])
def test_request_snapshot_stops_before_retaining_first_over_limit_record(
    rig: Rig, monkeypatch: pytest.MonkeyPatch, limit: str,
) -> None:
    messages = [_message(f"bounded-{index}") for index in range(8)]
    rig.bridge._ingest_result({"messages": messages})
    if limit == "records":
        monkeypatch.setattr(chat_module, "_MAX_REQUEST_RECORDS", 2)
    elif limit == "source-bytes":
        monkeypatch.setattr(chat_module, "_MAX_REQUEST_SOURCE_BYTES",
                            rig.bridge._message_source_bytes(messages[0]) * 2)
    else:
        first_two = sorted((rig.state / "requests").glob("*.json"))[:2]
        monkeypatch.setattr(
            chat_module, "_MAX_REQUEST_ENCODED_BYTES",
            sum(path.stat().st_size for path in first_two))
    retained: list[dict[str, object]] = []
    read_count = 0
    original = rig.bridge._read_bounded_record

    def read(path: Path) -> tuple[dict[str, object], int]:
        nonlocal read_count
        read_count += 1
        return original(path)

    monkeypatch.setattr(rig.bridge, "_read_bounded_record", read)
    usage = rig.bridge._request_population(retained=retained)
    assert len(retained) == 2 and read_count == 3 and usage["records"] == 3
    with pytest.raises(ValueError, match="request.*limit"):
        rig.bridge._load_request_records()
    assert read_count == 6


def test_request_encoded_population_accepts_cap_and_refuses_cap_plus_one(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    rig.bridge._ingest_result({"messages": [_message("encoded-boundary")]})
    path = next((rig.state / "requests").glob("*.json"))
    record = _read(path)
    record["capture_error"] = "x" * 4096
    _write(path, record)
    path.write_bytes(b" " * 1024 + path.read_bytes())
    path.chmod(0o600)
    encoded_size = path.stat().st_size

    monkeypatch.setattr(chat_module, "_MAX_REQUEST_ENCODED_BYTES", encoded_size)
    loaded = rig.bridge._load_request_records()
    retained: list[dict[str, object]] = []
    usage = rig.bridge._request_population(loaded, retained=retained)
    assert usage["encoded_bytes"] == encoded_size and retained == [record]

    monkeypatch.setattr(chat_module, "_MAX_REQUEST_ENCODED_BYTES", encoded_size - 1)
    retained.clear()
    usage = rig.bridge._request_population(retained=retained)
    assert usage["encoded_bytes"] == encoded_size and retained == []
    with pytest.raises(ValueError, match=f"encoded request byte limit {encoded_size - 1}"):
        rig.bridge._load_request_records()
    refusal = _read(rig.state / "request-limit.json")
    assert refusal["encoded_bytes"] == encoded_size
    assert as_mapping(refusal["limits"], "request limits")["encoded_bytes"] == encoded_size - 1


def test_request_schema_refuses_unknown_padding_and_incomplete_current_summary(
    rig: Rig,
) -> None:
    rig.bridge._ingest_result({"messages": [_message("request-schema")]})
    path = next((rig.state / "requests").glob("*.json"))
    record = _read(path)
    record["unaccounted_padding"] = "x" * 100_000
    _write(path, record)
    with pytest.raises(ValueError, match="contains 1 unsupported fields") as caught:
        rig.bridge._load_request_records()
    assert "unaccounted_padding" not in str(caught.value)

    record.pop("unaccounted_padding")
    record.pop("reply_total_bytes")
    _write(path, record)
    with pytest.raises(ValueError, match="current request record is missing reply summary fields"):
        rig.bridge._load_request_records()


def test_disabled_busy_delivery_waits_for_status_not_periodic_drain(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    rig = Rig(tmp_path / "disabled", outbound_mode="disabled")
    try:
        rig.harness.state = "working"
        rig.accept()
        rig.until(lambda: not rig.runtime.jobs)
        queued = next((rig.state / "queue" / "inbox").glob("*.json"))
        before = queued.read_bytes()
        deadline = rig.runtime.next_drain
        assert rig.runtime.output.desired is not None
        assert rig.runtime.output.desired.watching and not rig.runtime.output.desired.markers
        counts = {"drain": 0, "queue_scan": 0, "write": 0}
        original_drain, original_glob, original_write = agent_module.drain, Path.glob, agent_module._atomic_json

        def drain(client: HerdrClient, target: Target, root: str, *,
                  ready_timeout: float, max_artifact_bytes: int | None,
                  atomic_policy: AtomicWritePolicy | None = None) -> QueueResult:
            counts["drain"] += 1
            return original_drain(client, target, root, ready_timeout=ready_timeout,
                                  max_artifact_bytes=max_artifact_bytes, atomic_policy=atomic_policy)

        def glob(path: Path, pattern: str) -> Iterator[Path]:
            if "queue" in path.parts:
                counts["queue_scan"] += 1
            return original_glob(path, pattern)

        def write(path: str, document: dict[str, object], *, max_artifact_bytes: int | None = None) -> None:
            if "queue" in Path(path).parts:
                counts["write"] += 1
            original_write(path, document, max_artifact_bytes=max_artifact_bytes)

        monkeypatch.setattr(runtime_module, "drain", drain)
        monkeypatch.setattr(Path, "glob", glob)
        monkeypatch.setattr(agent_module, "_atomic_json", write)
        # A virtual 30-second tick must not trigger the old busy retry.
        monkeypatch.setattr(time, "monotonic", lambda: deadline - 200)
        rig.runtime._stage()
        assert counts == {"drain": 0, "queue_scan": 0, "write": 0}
        assert queued.read_bytes() == before
        rig.harness.state = "idle"
        pane = rig.bridge.config.target.pane_id
        assert pane is not None
        rig.runtime._handle(_Notice("output", PaneAgentStatus(pane, "idle")))
        rig.runtime._stage()
        rig.until(lambda: rig.phase() == "awaiting_reply" and not rig.runtime.jobs)
        assert counts["drain"] == 1 and len(rig.harness.prompts) == 1
        assert not rig.transport.calls("send") and not rig.transport.calls("react")
    finally:
        rig.finish()


def test_repeated_busy_drain_does_not_rewrite_unchanged_error_artifact(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, harness, chat = setup(tmp_path)
    harness.state = "working"
    chat.message()
    bridge.tick()
    path = next((tmp_path / "queue" / "inbox").glob("*.json"))
    before = path.read_bytes()
    writes: list[str] = []
    original = agent_module._atomic_json

    def write(path: str, document: dict[str, object], *, max_artifact_bytes: int | None = None) -> None:
        writes.append(path)
        original(path, document, max_artifact_bytes=max_artifact_bytes)

    monkeypatch.setattr(agent_module, "_atomic_json", write)
    for _ in range(3):
        result = agent_module.drain(harness, bridge.config.target, str(tmp_path / "queue"),
                                   ready_timeout=0, max_artifact_bytes=512 << 10)
        assert result.blocked is not None and result.pending
    assert writes == [] and path.read_bytes() == before
    assert not harness.prompts


def test_polling_rearm_loads_one_snapshot_and_opens_without_request_io(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, harness, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _read(next((tmp_path / "requests").glob("*.json")))
    original_load = bridge._load_request_records
    original_glob = Path.glob
    original_read = chat_module._read_bounded_json
    loads: list[tuple[int, int]] = []
    counters = {"scans": 0, "reads": 0}
    opening = False
    constructor_calls = 0

    def glob(path: Path, pattern: str) -> Iterator[Path]:
        if path == tmp_path / "requests":
            assert not opening, "rearming scanned requests"
            counters["scans"] += 1
        return original_glob(path, pattern)

    def read(
        path: Path, max_artifact_bytes: int, label: str,
    ) -> tuple[dict[str, object], int]:
        if path.parent == tmp_path / "requests":
            assert not opening, "rearming reread requests"
            counters["reads"] += 1
        return original_read(path, max_artifact_bytes, label)

    def load() -> list[dict[str, object]]:
        before = dict(counters)
        result = original_load()
        loads.append((counters["scans"] - before["scans"],
                      counters["reads"] - before["reads"]))
        return result

    original_open = bridge._open_cached_output

    def open_cached(subscription: _OutputSubscription) -> PaneOutputStream:
        nonlocal opening
        opening = True
        try:
            return original_open(subscription)
        finally:
            opening = False

    def constructor(socket_path: str, pane_id: str, pattern: Sequence[str], *,
                    watch_settled: bool = False) -> PaneOutputStream:
        nonlocal constructor_calls
        constructor_calls += 1
        expected = str(record["reply_nonce"]) + f"_{constructor_calls}"
        assert tuple(pattern) == chat_module._closing_patterns((expected,))
        if constructor_calls == 2:
            raise _StopLoop()
        return _FakeStream(lambda timeout: (_snapshot(_block(record)),))

    monkeypatch.setattr(bridge, "tick", lambda: {})
    monkeypatch.setattr(bridge, "_load_request_records", load)
    monkeypatch.setattr(bridge, "_open_cached_output", open_cached)
    monkeypatch.setattr(harness, "event_socket", lambda: "test-events")
    monkeypatch.setattr(chat_module, "PaneOutputStream", constructor)
    monkeypatch.setattr(Path, "glob", glob)
    monkeypatch.setattr(chat_module, "_read_bounded_json", read)
    with pytest.raises(_StopLoop):
        chat_module._run_polling(bridge, 3600, "test-chat")
    # Each bounded loader validates and retains R in one pass. There are exactly two
    # snapshots: initial and post-event, never extra loads for either marker
    # set, capture itself, or either stream open.
    assert loads == [(1, 1), (1, 1)]
    assert constructor_calls == 2 and len(chat.sent) == 1


def test_polling_inbound_only_delivers_on_status_before_provider_poll(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, harness, chat = setup(tmp_path)
    bridge.config = replace(bridge.config, outbound_mode="disabled")
    harness.state = "working"
    chat.message()
    clock = _Clock()
    opens: list[tuple[str, ...]] = []
    waits = 0

    def wait(timeout: float) -> tuple[PaneOutputSnapshot | PaneAgentStatus, ...]:
        nonlocal waits
        waits += 1
        assert not harness.prompts
        if waits == 1:
            clock.now = 30
            return ()
        assert waits == 2
        assert clock.now == 30
        harness.state = "idle"
        return (PaneAgentStatus("w1:p1", "idle"),)

    def constructor(socket_path: str, pane_id: str, pattern: Sequence[str], *,
                    watch_settled: bool = False) -> PaneOutputStream:
        assert watch_settled and pane_id == "w1:p1"
        opens.append(tuple(pattern))
        return _FakeStream(wait)

    monkeypatch.setattr(chat_module, "time", clock)
    monkeypatch.setattr(chat_module, "PaneOutputStream", constructor)
    monkeypatch.setattr(harness, "event_socket", lambda: "test-events")
    with pytest.raises(_StopLoop):
        chat_module._run_polling(bridge, 3600, "test-chat")
    assert opens == [()] and len(harness.prompts) == 1
    assert clock.now == 30
    assert [call["action"] for call in chat.calls] == ["poll"]
