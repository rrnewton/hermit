"""Push intake stays responsive while independent transport and harness work waits."""

from __future__ import annotations

import fcntl
import hashlib
import os
import queue
import socket
import threading
import time
import weakref
from collections.abc import Callable, Iterator, Sequence
from concurrent.futures import Future
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest

import agentctl.chat as chat_module
import agentctl.chat_runtime as runtime_module
from agentctl.agent import AtomicWritePolicy, Target, enqueue
from agentctl.chat import Bridge, Config, _read, _write, submit_reply
from agentctl.chat_input import InputStreamError
from agentctl.chat_output import PaneAgentStatus, PaneOutputSnapshot, PaneOutputStream
from agentctl.chat_runtime import _Completion, _Notice, _Runtime
from agentctl.chat_storage import ArtifactClass, read_json
from agentctl.client import AgentPaneInfo, GuardedInput
from agentctl.errors import AgentDeliveryError
from agentctl.jsonx import as_mapping, as_sequence, get_str
from tests.test_herdr_chat import Harness


_SPACE = "spaces/test"
_THREAD = _SPACE + "/threads/conversation"


def _message(name: str = "one", **fields: object) -> dict[str, object]:
    message: dict[str, object] = {
        "id": _SPACE + "/messages/" + name, "sender": "users/owner", "thread": _THREAD,
        "text": name, "created_at": "2026-01-02T00:00:00Z", "thread_reply": True,
    }
    message.update(fields)
    return message


def _key(name: str = "one") -> str:
    return hashlib.sha256((_SPACE + "/messages/" + name).encode()).hexdigest()


class Gate:
    def __init__(self) -> None:
        self.entered = threading.Event()
        self.release = threading.Event()
        self.expired = threading.Event()

    def wait(self) -> None:
        self.entered.set()
        if not self.release.wait(10):
            self.expired.set()
            raise AssertionError("fixture gate was not released")


class GatedHarness(Harness):
    def __init__(self) -> None:
        super().__init__()
        self.prompted = threading.Event()
        self.lookup: Gate | None = None

    def pane_info(self, pane_id: str) -> AgentPaneInfo:
        if self.lookup is not None:
            self.lookup.wait()
        return super().pane_info(pane_id)

    def prompt_agent(
        self, pane_id: str, text: str, *, terminal: GuardedInput | None = None,
    ) -> None:
        super().prompt_agent(pane_id, text)
        self.prompted.set()


class Transport:
    def __init__(self) -> None:
        self.gates: dict[str, Gate] = {}
        self.lock = threading.Lock()
        self.requests: list[dict[str, object]] = []
        self.sent: dict[str, str] = {}
        self.lose_send_once = False
        self.poll_messages: list[dict[str, object]] = []

    def gate(self, action: str) -> Gate:
        gate = Gate()
        self.gates[action] = gate
        return gate

    def calls(self, action: str) -> list[dict[str, object]]:
        with self.lock:
            return [dict(row) for row in self.requests if row["action"] == action]

    def __call__(self, request: dict[str, object]) -> dict[str, object]:
        action = str(request["action"])
        with self.lock:
            self.requests.append(dict(request))
            lose = action == "send" and self.lose_send_once
            if action == "send":
                self.lose_send_once = False
                request_id = str(request["request_id"])
                reply_id = self.sent.setdefault(request_id, _SPACE + "/messages/reply-" + request_id)
        gate = self.gates.get(action)
        if gate is not None:
            gate.wait()
        if action == "poll":
            return {"messages": list(self.poll_messages), "cursor": None}
        if action == "react":
            return {"id": str(request["message"]) + "/reactions/robot"}
        if action == "send":
            if lose:
                raise OSError("reply accepted but acknowledgement lost")
            return {"id": reply_id}
        raise AssertionError("unexpected transport action")


class Rig:
    def __init__(self, state: Path, *, outbound_mode: str = "enabled") -> None:
        self.state = state
        self.harness = GatedHarness()
        self.transport = Transport()
        pane_id = "fixture:" + hashlib.sha256(str(state).encode()).hexdigest()[:16]
        target = Target(pane_id=pane_id, expected_agent="codex",
                        expected_cwd="/work/project", expected_workspace="project")
        Bridge.initialize(state, Config(_SPACE, ("users/owner",), target, "fixture-agent",
                                       event_command=("fixture-events",), outbound_mode=outbound_mode),
                          after="2026-01-01T00:00:00Z")
        self.bridge = Bridge(state, self.harness, self.transport)
        self.instances: list[_Runtime] = []
        self.runtime = self.restart()

    def restart(self) -> _Runtime:
        self.bridge = Bridge(self.state, self.harness, self.transport)
        instance = _Runtime(self.bridge, 300, "test-chat", threading.Event())
        instance.reconcile_requested = False
        instance.next_poll = float("inf")
        self.instances.append(instance)
        self.runtime = instance
        return instance

    def record(self, name: str = "one") -> dict[str, object]:
        return _read(self.state / "requests" / (_key(name) + ".json"))

    def phase(self, name: str = "one") -> object:
        return self.record(name)["phase"]

    def ack(self, name: str = "one") -> object:
        return as_mapping(self.record(name)["ack"], "ack")["state"]

    def accept(self, name: str = "one", cursor: str = "cursor-one", **fields: object) -> None:
        self.runtime._input_event({"type": "message", "message": _message(name, **fields), "cursor": cursor})

    def until(self, predicate: Callable[[], bool]) -> None:
        while not predicate():
            try:
                notice = self.runtime.events.get(timeout=5)
            except queue.Empty:
                pytest.fail(f"runtime produced no completion; remaining jobs: {self.runtime.jobs}")
            self.runtime._handle(notice)
            self.runtime._stage()

    def finish(self) -> None:
        for gate in self.transport.gates.values():
            gate.release.set()
        if self.harness.lookup is not None:
            self.harness.lookup.release.set()
        for instance in self.instances:
            instance.stop.set()
            if instance.input.thread.ident is not None:
                instance.input.close()
            if instance.output.thread.ident is not None:
                instance.output.close()
            instance.local.close()
            for pool in instance.pools.values():
                pool.shutdown(wait=True, cancel_futures=True)
        gates = list(self.transport.gates.values())
        if self.harness.lookup is not None:
            gates.append(self.harness.lookup)
        assert not any(gate.expired.is_set() for gate in gates), "a blocked lane only progressed after its test gate expired"


@pytest.fixture
def rig(tmp_path: Path) -> Iterator[Rig]:
    result = Rig(tmp_path / "bridge")
    try:
        yield result
    finally:
        result.finish()


def test_hung_ack_does_not_delay_prompt_or_overwrite_later_reply_phase(rig: Rig) -> None:
    ack = rig.transport.gate("react")
    rig.accept()
    rig.runtime._stage()
    assert ack.entered.wait(5)
    assert rig.harness.prompted.wait(5)
    assert not ack.release.is_set()
    rig.until(lambda: rig.phase() == "awaiting_reply")
    assert rig.ack() == "pending"
    submit_reply(rig.state, _key(), "Finished while the reaction is pending")
    rig.runtime._stage()
    rig.until(lambda: rig.phase() == "replied")
    replied = rig.record()
    assert not ack.release.is_set()
    ack.release.set()
    rig.until(lambda: rig.ack() == "acked")
    current = rig.record()
    assert current["phase"] == "replied"
    assert current["reply_id"] == replied["reply_id"]
    assert current["reply_nonce"] == replied["reply_nonce"]
    assert len(rig.harness.prompts) == 1


def test_blocked_cursor_commit_does_not_delay_ack_or_native_delivery(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    rig.runtime._input_event({"type": "checkpoint", "cursor": "previous"})
    checkpoint = Gate()
    ack = rig.transport.gate("react")
    failures: list[BaseException] = []
    finished = threading.Event()

    def blocked_cursor(path: Path, document: dict[str, object]) -> None:
        if path == rig.state / "input.json":
            checkpoint.wait()
        _write(path, document)

    def accept() -> None:
        try:
            rig.accept()
        except BaseException as exc:
            failures.append(exc)
        finally:
            finished.set()

    monkeypatch.setattr(runtime_module, "_write", blocked_cursor)
    # This intake must not run unrelated polling or housekeeping while making
    # the newly durable request eligible for the two independent workers.
    rig.runtime.reconcile_requested = True
    owner = threading.Thread(target=accept)
    owner.start()
    try:
        assert checkpoint.entered.wait(5)
        assert ack.entered.wait(5)
        assert rig.harness.prompted.wait(5)
        assert not finished.is_set()
        assert not checkpoint.release.is_set()
        assert _read(rig.state / "input.json")["cursor"] == "previous"
        saved = rig.record()
        attempted = as_mapping(saved["ack"], "ack")
        assert saved["phase"] == "received"
        assert attempted["attempts"] == 1
        assert attempted["last_attempt_at"] is not None
        assert attempted["next_retry_at"] is not None
        assert rig.transport.calls("react")[0]["request_id"] == attempted["request_id"]
        assert len(rig.harness.prompts) == 1
        assert rig.transport.calls("poll") == []
        assert not (rig.state / "output.json").exists()
        submit_reply(rig.state, _key(), "Finished before the cursor commit")
        assert rig.record() == saved, "file replies must not advance the owner's request phase"
        assert _read(rig.state / "submissions" / f"{_key()}.json")["text"] == "Finished before the cursor commit"
    finally:
        checkpoint.release.set()
        owner.join(timeout=5)
        ack.release.set()
    assert not owner.is_alive()
    assert not checkpoint.expired.is_set()
    assert failures == []
    assert _read(rig.state / "input.json")["cursor"] == "cursor-one"
    rig.runtime.reconcile_requested = False
    rig.until(lambda: rig.phase() == "replied" and rig.ack() == "acked")
    assert [call["text"] for call in rig.transport.calls("send")] == [
        "[fixture-agent] Finished before the cursor commit",
    ]


def test_received_but_undispatched_request_cannot_accept_a_file_reply(rig: Rig) -> None:
    rig.bridge._ingest_result({"messages": [_message()]})
    assert rig.phase() == "received"
    with pytest.raises(ValueError, match="not awaiting a reply"):
        submit_reply(rig.state, _key(), "Too early")
    assert list((rig.state / "replies").glob("*.json")) == []
    assert list((rig.state / "replies" / "items").rglob("*.json")) == []
    assert list((rig.state / "submissions").glob("*.json")) == []
    assert rig.phase() == "received"


def test_streaming_output_observer_coalesces_flaps_to_latest_state(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    writes: list[dict[str, object]] = []
    original = chat_module._write

    def write(path: Path, document: dict[str, object]) -> None:
        if path == rig.state / "output.json":
            writes.append(dict(document))
        original(path, document)

    monkeypatch.setattr(chat_module, "_write", write)
    rig.runtime._handle(_Notice("output_connected"))
    initial_mtime = (rig.state / "output.json").stat().st_mtime_ns
    rig.runtime._handle(_Notice("output_connected"))
    assert (rig.state / "output.json").stat().st_mtime_ns == initial_mtime
    rig.runtime._handle(_Notice("output_error", "socket unavailable"))
    rig.runtime._handle(_Notice("output_error", "socket unavailable"))
    rig.runtime._handle(_Notice("output_error", "permission denied"))
    assert [(row["state"], row["error"]) for row in writes] == [("connected", None)]
    rig.runtime.output_observer._next_write = 0
    assert rig.runtime.output_observer.flush()
    assert (writes[-1]["state"], writes[-1]["error"]) == ("retrying", "permission denied")
    rig.runtime._handle(_Notice("output_connected"))
    assert len(writes) == 2
    rig.runtime.output_observer._next_write = 0
    assert rig.runtime.output_observer.flush()
    assert (writes[-1]["state"], writes[-1]["error"]) == ("connected", None)


def test_input_heartbeats_do_not_write_and_cursor_advances_remain_immediate(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    writes: list[dict[str, object]] = []
    original = _write

    def write(path: Path, document: dict[str, object]) -> None:
        if path == rig.state / "input.json":
            writes.append(dict(document))
        original(path, document)

    monkeypatch.setattr(runtime_module, "_write", write)
    for _ in range(100):
        assert rig.runtime._input_event({"type": "heartbeat"}) == ("heartbeat", None)
    assert writes == [] and not (rig.state / "input.json").exists()

    rig.runtime._input_event({"type": "checkpoint", "cursor": "durable-one"})
    assert len(writes) == 1 and writes[0]["cursor"] == "durable-one"
    for _ in range(100):
        rig.runtime._input_event({"type": "heartbeat"})
    assert len(writes) == 1

    rig.runtime._handle(_Notice("input_error", "socket unavailable"))
    rig.runtime._handle(_Notice("input_connected"))
    rig.runtime._handle(_Notice("input_error", "permission denied"))
    assert len(writes) == 1
    rig.runtime.input_observer._next_write = 0
    assert rig.runtime.input_observer.flush()
    assert len(writes) == 2 and writes[-1]["error"] == "permission denied"


def test_quiet_reconciliation_has_monotonic_hourly_not_poll_frequency_writes(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    writes: list[dict[str, object]] = []
    original = _write

    def write(path: Path, document: dict[str, object]) -> None:
        if path == rig.state / "input.json":
            writes.append(dict(document))
        original(path, document)

    monkeypatch.setattr(runtime_module, "_write", write)
    monotonic = [100.0]
    monkeypatch.setattr(time, "monotonic", lambda: monotonic[0])
    rig.runtime._handle(_Notice("input_connected"))
    assert len(writes) == 1
    first = "2026-01-02T00:05:00Z"
    rig.runtime.input_observer._next_write = 0
    rig.runtime._complete(_Completion(
        "poll", "page", {"messages": [], "cursor": None}, None, first))
    assert len(writes) == 2
    assert _read(rig.state / "input.json")["reconciled_at"] == first

    latest = first
    # Event timestamps deliberately cross the wall-clock boundary backward and
    # forward. They must not control the monotonic disk-write cadence.
    for offset, latest in enumerate((
        "2026-01-02T01:05:00Z",
        "2026-01-02T00:10:00Z",
        "2026-01-02T01:10:00Z",
        "2026-01-02T00:15:00Z",
    ), start=1):
        monotonic[0] = 100.0 + offset * 300.0
        # Make every completion eligible under the ordinary observer interval.
        # The separate monotonic health deadline must still suppress it.
        rig.runtime.input_observer._next_write = 0
        rig.runtime._complete(_Completion(
            "poll", "page", {"messages": [], "cursor": None}, None, latest))
    assert len(writes) == 2
    assert rig.runtime.input_state["reconciled_at"] == latest
    assert _read(rig.state / "input.json")["reconciled_at"] == first

    monotonic[0] = 3699.999
    rig.runtime._complete(_Completion(
        "poll", "page", {"messages": [], "cursor": None}, None,
        "2026-01-01T23:59:00Z"))
    assert len(writes) == 2

    next_hour = "2026-01-02T00:20:00Z"
    monotonic[0] = 3700.0
    rig.runtime._complete(_Completion(
        "poll", "page", {"messages": [], "cursor": None}, None, next_hour))
    assert len(writes) == 3
    assert _read(rig.state / "input.json")["reconciled_at"] == next_hour

    folded = "2026-01-02T01:05:00Z"
    rig.runtime._complete(_Completion(
        "poll", "page", {"messages": [], "cursor": None}, None, folded))
    rig.runtime.input_observer._next_write = 0
    rig.runtime._handle(_Notice("input_error", "socket unavailable"))
    assert len(writes) == 4
    state_saved = _read(rig.state / "input.json")
    assert state_saved["error"] == "socket unavailable"
    assert state_saved["reconciled_at"] == folded

    rig.runtime._input_event({"type": "checkpoint", "cursor": "durable-one"})
    assert len(writes) == 5
    saved = _read(rig.state / "input.json")
    assert saved["cursor"] == "durable-one"
    assert saved["reconciled_at"] == folded


def test_missing_input_state_is_repaired_by_unchanged_status(rig: Rig) -> None:
    rig.runtime._handle(_Notice("input_connected"))
    completed = "2026-01-02T00:05:00Z"
    rig.runtime._complete(_Completion(
        "poll", "page", {"messages": [], "cursor": None}, None, completed))
    rig.runtime._input_event({"type": "checkpoint", "cursor": "durable-one"})
    rig.runtime.input_observer._next_write = 0
    rig.runtime._handle(_Notice("input_error", "socket unavailable"))
    (rig.state / "input.json").unlink()
    assert rig.runtime.input_observer.status(state="retrying", error="socket unavailable")
    repaired = _read(rig.state / "input.json")
    assert repaired["state"] == "retrying"
    assert repaired["error"] == "socket unavailable"
    assert repaired["cursor"] == "durable-one"
    assert repaired["reconciled_at"] == completed


@pytest.mark.parametrize(("updated_at", "delay"), [
    ("2026-01-02T00:30:00Z", 1800.0),
    ("2026-01-02T02:00:00Z", 3600.0),
    ("2026-01-01T23:00:00Z", 0.0),
])
def test_reconciliation_health_restart_anchor_is_bounded_and_clock_safe(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, updated_at: str, delay: float,
) -> None:
    path = tmp_path / "input.json"
    document: dict[str, object] = {
        "state": "connected",
        "error": None,
        "reconcile_error": None,
        "reconciled_at": "2026-01-02T00:25:00Z",
        "updated_at": updated_at,
    }
    _write(path, document)
    monotonic = [100.0]
    monkeypatch.setattr(time, "monotonic", lambda: monotonic[0])
    monkeypatch.setattr(runtime_module, "_utc", lambda: "2026-01-02T01:00:00Z")
    writes: list[dict[str, object]] = []
    original = _write

    def write(target: Path, value: dict[str, object]) -> None:
        writes.append(dict(value))
        original(target, value)

    monkeypatch.setattr(runtime_module, "_write", write)
    observer = runtime_module._InputObserver(path, dict(document), 60.0)
    assert observer._next_health_write == 100.0 + delay
    if delay:
        monotonic[0] = 100.0 + delay - 0.001
        assert not observer.status(reconciled_at="2026-01-02T01:05:00Z")
        assert writes == []
    monotonic[0] = 100.0 + delay
    assert observer.status(reconciled_at="2026-01-02T01:10:00Z")
    assert len(writes) == 1


def test_reconciliation_health_and_net_error_state_are_durable(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    writes: list[dict[str, object]] = []
    original = _write

    def write(path: Path, document: dict[str, object]) -> None:
        if path == rig.state / "input.json":
            writes.append(dict(document))
        original(path, document)

    monkeypatch.setattr(runtime_module, "_write", write)
    rig.runtime._handle(_Notice("input_connected"))
    assert len(writes) == 1

    latest = "2026-01-02T00:10:00Z"
    rig.runtime.input_observer._next_write = 0
    rig.runtime._complete(_Completion(
        "poll", "page", {"messages": [], "cursor": None}, None, latest))
    assert len(writes) == 2, "the first successful reconciliation is durable health"
    assert rig.runtime.input_state["reconciled_at"] == latest
    assert _read(rig.state / "input.json")["reconciled_at"] == latest

    rig.runtime.input_observer._next_write = 0
    rig.runtime._complete(_Completion(
        "poll", "page", None, OSError("provider unavailable"),
        "2026-01-02T00:11:00Z"))
    assert len(writes) == 3
    failed = _read(rig.state / "input.json")
    assert failed["reconcile_error"] == "provider unavailable"
    assert failed["reconciled_at"] == latest

    recovered = "2026-01-02T00:12:00Z"
    rig.runtime.input_observer._next_write = 0
    rig.runtime._complete(_Completion(
        "poll", "page", {"messages": [], "cursor": None}, None, recovered))
    assert len(writes) == 4
    saved = _read(rig.state / "input.json")
    assert saved["reconcile_error"] is None
    assert saved["reconciled_at"] == recovered

    restarted = rig.restart()
    assert restarted.input_state["reconciled_at"] == recovered
    restarted._handle(_Notice("input_connected"))
    for _ in range(100):
        assert restarted._input_event({"type": "heartbeat"}) == ("heartbeat", None)
    assert len(writes) == 4, "unchanged status and heartbeats must not write"


def test_reconcile_error_then_success_clears_error_and_persists_completion(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    writes: list[dict[str, object]] = []
    original = _write

    def write(path: Path, document: dict[str, object]) -> None:
        if path == rig.state / "input.json":
            writes.append(dict(document))
        original(path, document)

    monkeypatch.setattr(runtime_module, "_write", write)
    rig.runtime._complete(_Completion(
        "poll", "page", None, OSError("provider unavailable"), "2026-01-02T00:04:00Z"))
    assert _read(rig.state / "input.json")["reconcile_error"] == "provider unavailable"

    completed = "2026-01-02T00:05:00Z"
    rig.runtime._complete(_Completion(
        "poll", "page", {"messages": [], "cursor": None}, None, completed))
    assert len(writes) == 2, "the first successful reconciliation is immediately durable"
    assert rig.runtime.input_state["reconcile_error"] is None
    assert rig.runtime.input_state["reconciled_at"] == completed
    saved = _read(rig.state / "input.json")
    assert saved["reconcile_error"] is None
    assert saved["reconciled_at"] == completed
    assert len(writes) == 2, "error and first successful completion each write once"


def test_nonterminal_reconcile_page_is_not_reported_complete(rig: Rig) -> None:
    first = "2026-01-02T00:05:00Z"
    needed, selected = rig.runtime._handle(_Notice("complete", _Completion(
        "poll", "page", {"messages": [_message()], "cursor": "page-two"}, None, first)))
    assert needed and selected == {_key()}
    assert rig.runtime.input_state.get("reconciled_at") is None
    assert rig.runtime.input_state.get("reconcile_error") is None
    assert _read(rig.state / "bridge.json")["cursor"] == "page-two"
    assert _key() in rig.runtime.records

    completed = "2026-01-02T00:06:00Z"
    needed, selected = rig.runtime._handle(_Notice("complete", _Completion(
        "poll", "page", {"messages": [], "cursor": None}, None, completed)))
    assert needed and selected == set()
    assert rig.runtime.input_state["reconciled_at"] == completed
    assert rig.runtime.input_state["reconcile_error"] is None


def test_repeated_poll_cursor_refuses_page_before_intake_and_backs_off(rig: Rig) -> None:
    rig.runtime._handle(_Notice("complete", _Completion(
        "poll", "page", {"messages": [_message("one")], "cursor": "loop"}, None,
        "2026-01-02T00:05:00Z")))
    checkpoint = (rig.state / "bridge.json").read_bytes()
    needed, selected = rig.runtime._handle(_Notice("complete", _Completion(
        "poll", "page", {"messages": [_message("two")], "cursor": "loop"}, None,
        "2026-01-02T00:05:01Z")))
    assert needed and selected == set()
    assert (rig.state / "bridge.json").read_bytes() == checkpoint
    assert not (rig.state / "requests" / f"{_key('two')}.json").exists()
    assert rig.runtime.reconcile_window is None and rig.runtime.reconcile_requested
    retry_at, delay = rig.runtime.retry[("poll", "page")]
    assert retry_at > time.monotonic() and delay == 1
    assert "repeated a reconciliation cursor" in str(
        rig.runtime.input_state["reconcile_error"])
    assert rig.runtime.input_state.get("reconciled_at") is None


@pytest.mark.parametrize("budget", ["pages", "time"])
def test_nonterminal_reconcile_page_stops_at_page_or_time_budget(
    rig: Rig, monkeypatch: pytest.MonkeyPatch, budget: str,
) -> None:
    if budget == "pages":
        monkeypatch.setattr(runtime_module, "_MAX_RECONCILE_PAGES", 1)
    else:
        rig.runtime.reconcile_window = runtime_module._ReconcileWindow(
            started_at=time.monotonic() - runtime_module._MAX_RECONCILE_SECONDS - 1,
            pages=0, cursors=set())
    needed, selected = rig.runtime._handle(_Notice("complete", _Completion(
        "poll", "page", {"messages": [_message()], "cursor": "more"}, None,
        "2026-01-02T00:05:00Z")))
    assert needed and selected == {_key()}
    assert _read(rig.state / "bridge.json")["cursor"] == "more"
    assert _key() in rig.runtime.records
    assert rig.runtime.reconcile_window is None and rig.runtime.reconcile_requested
    assert ("poll", "page") in rig.runtime.retry
    assert f"reconciliation {budget[:-1] if budget == 'pages' else budget} budget" in str(
        rig.runtime.input_state["reconcile_error"])
    assert rig.runtime.input_state.get("reconciled_at") is None


def test_poll_pages_admit_only_new_records_without_full_recovery_io(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    baseline = 64
    rig.bridge._ingest_result({
        "messages": [_message(f"old-{index}") for index in range(baseline)]})
    rig.runtime._reload_records()
    reads: list[Path] = []
    original_read = rig.bridge._read_bounded_record

    def read(path: Path) -> tuple[dict[str, object], int]:
        reads.append(path)
        return original_read(path)

    monkeypatch.setattr(rig.bridge, "_read_bounded_record", read)
    monkeypatch.setattr(rig.runtime, "_reload_records", lambda: (_ for _ in ()).throw(
        AssertionError("a poll page performed full request recovery")))
    for index in range(5):
        cursor = None if index == 4 else f"page-{index + 1}"
        needed, selected = rig.runtime._handle(_Notice("complete", _Completion(
            "poll", "page", {
                "messages": [_message(f"new-{index}")], "cursor": cursor,
            }, None, f"2026-01-02T00:05:0{index}Z")))
        assert needed and selected == {_key(f"new-{index}")}
    assert len(rig.runtime.records) == baseline + 5
    assert reads == [
        rig.state / "requests" / f"{_key(f'new-{index}')}.json"
        for index in range(5)
    ]
    staged: list[str] = []

    def stage_one(
        path: Path, record: dict[str, object], now: float,
    ) -> tuple[bool, tuple[str, str] | None]:
        staged.append(get_str(record, "key", "request"))
        return False, None

    monkeypatch.setattr(rig.runtime, "_stage_request", stage_one)
    rig.runtime._stage(selected)
    assert staged == [_key("new-4")]


def test_incrementally_admitted_poll_request_still_delivers_and_acks(rig: Rig) -> None:
    _, selected = rig.runtime._handle(_Notice("complete", _Completion(
        "poll", "page", {"messages": [_message()], "cursor": None}, None,
        "2026-01-02T00:05:00Z")))
    assert selected == {_key()}
    rig.runtime._stage(selected)
    rig.until(lambda: rig.phase() == "awaiting_reply"
              and rig.ack() == "acked" and not rig.runtime.jobs)
    assert len(rig.harness.prompts) == 1


@pytest.mark.parametrize("ambiguous_write", ["request", "checkpoint"])
def test_poll_write_then_error_immediately_recovers_durable_intake(
    rig: Rig, monkeypatch: pytest.MonkeyPatch, ambiguous_write: str,
) -> None:
    original_write = chat_module._write
    injected = False

    def write(path: Path, document: dict[str, object]) -> None:
        nonlocal injected
        original_write(path, document)
        target = (path.parent == rig.state / "requests" if ambiguous_write == "request"
                  else path == rig.state / "bridge.json")
        if target and not injected:
            injected = True
            raise OSError(f"ambiguous {ambiguous_write} fsync")

    monkeypatch.setattr(chat_module, "_write", write)
    needed, selected = rig.runtime._handle(_Notice("complete", _Completion(
        "poll", "page", {"messages": [_message()], "cursor": "next"}, None,
        "2026-01-02T00:05:00Z")))
    assert injected and needed and selected is None
    assert _key() in rig.runtime.records
    assert (rig.state / "requests" / f"{_key()}.json").exists()
    assert _read(rig.state / "bridge.json")["cursor"] == (
        "next" if ambiguous_write == "checkpoint" else None)

    _, delay = rig.runtime.retry[("poll", "page")]
    rig.runtime.retry[("poll", "page")] = (float("inf"), delay)
    rig.runtime._stage(selected)
    rig.until(lambda: rig.phase() == "awaiting_reply"
              and rig.ack() == "acked" and not rig.runtime.jobs)
    assert len(rig.harness.prompts) == 1


def test_later_incremental_admission_error_recovers_and_stages_entire_page(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    original_read = rig.bridge._read_bounded_record
    reads = 0

    def read(path: Path) -> tuple[dict[str, object], int]:
        nonlocal reads
        reads += 1
        if reads == 2:
            raise OSError("second incremental admission failed")
        return original_read(path)

    monkeypatch.setattr(rig.bridge, "_read_bounded_record", read)
    needed, selected = rig.runtime._handle(_Notice("complete", _Completion(
        "poll", "page", {
            "messages": [_message("one"), _message("two")], "cursor": None,
        }, None, "2026-01-02T00:05:00Z")))
    assert needed and selected is None and reads >= 4
    assert set(rig.runtime.records) == {_key("one"), _key("two")}
    _, delay = rig.runtime.retry[("poll", "page")]
    rig.runtime.retry[("poll", "page")] = (float("inf"), delay)
    rig.runtime._stage(selected)
    rig.until(lambda: all(rig.phase(name) == "awaiting_reply" for name in ("one", "two"))
              and all(rig.ack(name) == "acked" for name in ("one", "two"))
              and not rig.runtime.jobs)
    assert len(rig.harness.prompts) == 2


def test_failed_authoritative_poll_recovery_propagates_before_cursor_can_continue(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    original_write = chat_module._write
    checkpoint_failed = False

    def write(path: Path, document: dict[str, object]) -> None:
        nonlocal checkpoint_failed
        original_write(path, document)
        if path == rig.state / "bridge.json" and not checkpoint_failed:
            checkpoint_failed = True
            raise OSError("ambiguous checkpoint fsync")

    original_read = rig.bridge._read_bounded_record
    recovery_failed = False

    def read(path: Path) -> tuple[dict[str, object], int]:
        nonlocal recovery_failed
        if not recovery_failed:
            recovery_failed = True
            raise OSError("authoritative recovery failed")
        return original_read(path)

    monkeypatch.setattr(chat_module, "_write", write)
    monkeypatch.setattr(rig.bridge, "_read_bounded_record", read)
    with pytest.raises(OSError, match="authoritative recovery failed"):
        rig.runtime._handle(_Notice("complete", _Completion(
            "poll", "page", {"messages": [_message()], "cursor": "next"}, None,
            "2026-01-02T00:05:00Z")))
    assert checkpoint_failed and recovery_failed
    assert _read(rig.state / "bridge.json")["cursor"] == "next"
    assert (rig.state / "requests" / f"{_key()}.json").exists()
    assert _key() not in rig.runtime.records
    assert rig.runtime.input_state.get("reconciled_at") is None

    # A restarted owner performs the same startup authority scan before it can
    # consume another cursor or publish healthy reconciliation state.
    rig.runtime._reload_records()
    assert _key() in rig.runtime.records


def test_input_pump_filters_unchanged_heartbeats_before_owner_mailbox(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    _write(rig.state / "input.json", {"cursor": "durable-one"})
    frames: list[dict[str, object]] = [
        {"type": "heartbeat"},
        {"type": "heartbeat", "cursor": "durable-one"},
        {"type": "checkpoint", "cursor": "durable-one"},
        {"type": "heartbeat", "cursor": "durable-two"},
        {"type": "heartbeat", "cursor": "durable-two"},
        {"type": "gap", "cursor": "durable-two"},
    ]
    subscriptions: list[dict[str, object]] = []
    stop = threading.Event()

    class Stream:
        def __init__(self, command: Sequence[str], request: dict[str, object]) -> None:
            subscriptions.append(request)

        def wait(self, timeout: float) -> list[dict[str, object]]:
            if frames:
                return [frames.pop(0)]
            stop.set()
            return []

        def close(self) -> None:
            pass

    monkeypatch.setattr(runtime_module, "EventCommandStream", Stream)
    events: queue.Queue[_Notice] = queue.Queue(maxsize=64)
    pump = runtime_module._InputPump(rig.bridge, events, stop)
    pump._run()

    notices: list[_Notice] = []
    while not events.empty():
        notices.append(events.get_nowait())
    assert subscriptions == [{
        "action": "subscribe", "space": rig.bridge.config.space, "cursor": "durable-one",
    }]
    assert [notice.kind for notice in notices] == ["input_connected", "input", "input", "input"]
    assert [notice.value for notice in notices[1:]] == [
        {"type": "checkpoint", "cursor": "durable-one"},
        {"type": "heartbeat", "cursor": "durable-two"},
        {"type": "gap", "cursor": "durable-two"},
    ]


@pytest.mark.parametrize(
    "event",
    [
        {"type": "message", "message": _message()},
        {"type": "message", "message": _message(), "cursor": "next"},
        {"type": "heartbeat"},
        {"type": "heartbeat", "cursor": "next"},
        {"type": "checkpoint", "cursor": "next"},
        {"type": "gap"},
        {"type": "gap", "reason": "resume unavailable", "cursor": "next"},
    ],
)
def test_stream_event_exact_shapes_accept_only_documented_fields(
    event: dict[str, object],
) -> None:
    kind, cursor = runtime_module._validate_input_event(event)
    assert kind == event["type"] and cursor == event.get("cursor")


@pytest.mark.parametrize(
    "event",
    [
        {"type": "message"},
        {"type": "message", "message": _message(), "reason": "wrong type"},
        {"type": "heartbeat", "message": _message()},
        {"type": "heartbeat", "reason": "wrong type"},
        {"type": "checkpoint"},
        {"type": "checkpoint", "cursor": None},
        {"type": "checkpoint", "cursor": "next", "message": _message()},
        {"type": "gap", "message": _message()},
        {"type": "gap", "reason": 1},
        {"type": "gap", "reason": None},
        {"type": "gap", "reason": ""},
        {"type": "gap", "reason": "x" * 2001},
        {"type": "heartbeat", "cursor": ""},
        {"type": "heartbeat", "cursor": None},
        {"type": "heartbeat", "cursor": 1},
        {"type": "heartbeat", "cursor": "x" * 8193},
        {"type": "unknown"},
        {"type": "gap", "unexpected": True},
    ],
)
def test_stream_event_cross_type_or_malformed_fields_fail_before_mutation(
    rig: Rig, event: dict[str, object],
) -> None:
    requests_before = list((rig.state / "requests").glob("*.json"))
    input_before = ((rig.state / "input.json").read_bytes()
                    if (rig.state / "input.json").exists() else None)
    with pytest.raises((TypeError, ValueError)):
        rig.runtime._input_event(event)
    assert list((rig.state / "requests").glob("*.json")) == requests_before
    assert ((rig.state / "input.json").read_bytes()
            if (rig.state / "input.json").exists() else None) == input_before


def test_input_pump_rejects_unchanged_heartbeat_with_hidden_message(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    stop = threading.Event()

    class Stream:
        def __init__(self, command: Sequence[str], request: dict[str, object]) -> None:
            pass

        def wait(self, timeout: float) -> list[dict[str, object]]:
            return [{"type": "heartbeat", "message": _message()}]

        def close(self) -> None:
            stop.set()

    monkeypatch.setattr(runtime_module, "EventCommandStream", Stream)
    events: queue.Queue[_Notice] = queue.Queue(maxsize=64)
    runtime_module._InputPump(rig.bridge, events, stop)._run()
    notices: list[_Notice] = []
    while not events.empty():
        notices.append(events.get_nowait())
    assert [notice.kind for notice in notices] == ["input_connected", "input_error"]
    assert "unsupported fields: message" in str(notices[-1].value)
    assert not (rig.state / "input.json").exists()
    assert list((rig.state / "requests").glob("*.json")) == []


def test_request_population_status_starts_empty_with_explicit_limits(rig: Rig) -> None:
    storage = as_mapping(rig.bridge.status()["request_storage"], "request storage")
    assert storage["usage"] == {"records": 0, "source_bytes": 0, "encoded_bytes": 0}
    assert as_mapping(storage["limits"], "request limits") == {
        "records": 2048, "source_bytes": 64 << 20,
        "encoded_bytes": 128 << 20,
        "message_bytes": 64 << 10, "encoded_record_bytes": 512 << 10,
    }


def test_stream_request_count_cap_preserves_cursor_and_accepted_work(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(chat_module, "_MAX_REQUEST_RECORDS", 1)
    rig.accept("one", "cursor-one")
    accepted = rig.record("one")
    with pytest.raises(ValueError, match="request record limit 1"):
        rig.runtime._input_event({
            "type": "message", "message": _message("two"), "cursor": "cursor-two"})
    assert _read(rig.state / "input.json")["cursor"] == "cursor-one"
    assert rig.record("one") == accepted
    assert not (rig.state / "requests" / f"{_key('two')}.json").exists()
    refusal = _read(rig.state / "request-limit.json")
    assert refusal["attempted_message_sha256"] and refusal["records"] == 1


def test_poll_batch_at_request_cap_keeps_checkpoint_for_replay(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(chat_module, "_MAX_REQUEST_RECORDS", 1)
    checkpoint = _read(rig.state / "bridge.json")
    before = (rig.state / "bridge.json").read_bytes()
    result: dict[str, object] = {"messages": [_message("one"), _message("two")], "cursor": "page-two"}
    with pytest.raises(ValueError, match="request record limit 1"):
        rig.bridge._ingest_result(result, checkpoint)
    assert (rig.state / "bridge.json").read_bytes() == before
    assert not (rig.state / "requests" / f"{_key('one')}.json").exists()
    assert not (rig.state / "requests" / f"{_key('two')}.json").exists()
    with pytest.raises(ValueError, match="request record limit 1"):
        rig.bridge._ingest_result(result, _read(rig.state / "bridge.json"))
    assert list((rig.state / "requests").glob("*.json")) == []


def test_tick_rejects_oversized_poll_batch_atomically_then_delivers_replay(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(chat_module, "_MAX_REQUEST_RECORDS", 1)
    before = (rig.state / "bridge.json").read_bytes()
    rig.transport.poll_messages = [_message("one"), _message("two")]
    with pytest.raises(ValueError, match="request record limit 1"):
        rig.bridge.tick()
    assert (rig.state / "bridge.json").read_bytes() == before
    assert list((rig.state / "requests").glob("*.json")) == []
    assert rig.harness.prompts == []

    # A provider replay which fits the bound is accepted and drained through
    # the real one-shot path; no partially accepted request depends on a
    # permanently failing oversized batch for recovery.
    rig.transport.poll_messages = [_message("one")]
    status = rig.bridge.tick()
    assert len(as_sequence(status["requests"], "requests")) == 1
    assert rig.record("one")["phase"] == "awaiting_reply"
    assert len(rig.harness.prompts) == 1


def test_runtime_queue_reservation_refuses_before_intake_without_directory_scan(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    rig.accept("one", "cursor-one")
    reservations = rig.bridge._queue_reservations
    assert reservations is not None and len(reservations) == 1
    before = dict(reservations)
    monkeypatch.setattr(chat_module, "_MAX_QUEUE_BYTES",
                        rig.bridge._queue_fixed_reservation + sum(before.values()))
    monkeypatch.setattr(Path, "glob", lambda *args, **kwargs: (_ for _ in ()).throw(
        AssertionError("ordinary admission scanned a durable directory")))
    with pytest.raises(ValueError, match="queue artifact byte limit"):
        rig.runtime._input_event({
            "type": "message", "message": _message("two"), "cursor": "cursor-two"})
    assert rig.bridge._queue_reservations == before
    assert _read(rig.state / "input.json")["cursor"] == "cursor-one"
    assert not (rig.state / "requests" / f"{_key('two')}.json").exists()


def test_poll_queue_reservation_refuses_entire_batch_before_checkpoint_or_artifacts(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    rig.bridge._ingest_result({"messages": [_message("one")]})
    reservations = rig.bridge._queue_reservations
    assert reservations is not None
    before = dict(reservations)
    monkeypatch.setattr(chat_module, "_MAX_QUEUE_BYTES",
                        rig.bridge._queue_fixed_reservation + 2 * sum(before.values()) + 100)
    checkpoint = _read(rig.state / "bridge.json")
    checkpoint_bytes = (rig.state / "bridge.json").read_bytes()
    with pytest.raises(ValueError, match="queue artifact byte limit"):
        rig.bridge._ingest_result(
            {"messages": [_message("two"), _message("three")], "cursor": "advance"},
            checkpoint)
    assert rig.bridge._queue_reservations == before
    assert (rig.state / "bridge.json").read_bytes() == checkpoint_bytes
    assert not (rig.state / "requests" / f"{_key('two')}.json").exists()
    assert not (rig.state / "requests" / f"{_key('three')}.json").exists()


def test_queue_reservation_survives_failed_enqueue_and_restart_accounts_sidecars(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    rig.bridge._ingest_result({"messages": [_message("one")]})
    record = rig.record("one")
    identifier = str(record["queue_id"])
    prompt = rig.bridge._prompt(record)
    assert rig.bridge._queue_reservations is not None
    before = dict(rig.bridge._queue_reservations)
    rig.bridge._reserve_queue_prompts([(identifier, prompt)])
    rig.bridge._reserve_queue_prompts([(identifier, prompt)])
    assert rig.bridge._queue_reservations == before
    def failed_enqueue(*args: object, **kwargs: object) -> str:
        raise OSError("injected enqueue failure")

    with monkeypatch.context() as failed:
        failed.setattr(chat_module, "enqueue", failed_enqueue)
        for _ in range(2):
            with pytest.raises(OSError, match="injected enqueue failure"):
                rig.bridge._deliver()
            assert rig.bridge._queue_reservations == before
    # A legacy error sidecar is counted alongside its queue artifact, even
    # though the generic queue does not enumerate it as a pending prompt.
    enqueue(str(rig.state / "queue"), prompt, message_id=identifier)
    sidecar = rig.state / "queue" / "inbox" / f"{identifier}.json.error"
    _write(sidecar, {"artifact": identifier + ".json", "error": "x" * 40_000})
    usage = rig.bridge.validate_aux_population()
    artifact = sidecar.with_name(identifier + ".json")
    assert usage["queue_bytes"] == artifact.stat().st_size + sidecar.stat().st_size
    assert usage["queue_reserved_bytes"] > sum(before.values())
    monkeypatch.setattr(chat_module, "_MAX_QUEUE_BYTES", usage["queue_reserved_bytes"] - 1)
    prior = sidecar.read_bytes()
    with pytest.raises(ValueError, match="queue artifact byte limit"):
        rig.runtime._reload_records()
    assert sidecar.read_bytes() == prior


def test_chat_delivery_failure_bounds_queue_artifact_and_error_sidecar(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    from agentctl.agent import QUEUE_ERROR_MAX_BYTES

    rig.bridge._ingest_result({"messages": [_message("one")]})
    assert rig.bridge._queue_reservations is not None
    reserved = sum(rig.bridge._queue_reservations.values())

    def failed_prompt(pane_id: str, text: str) -> None:
        raise AgentDeliveryError("\x00" * 100_000)

    monkeypatch.setattr(rig.harness, "prompt_agent", failed_prompt)
    rig.bridge._deliver()
    identifier = str(rig.record("one")["queue_id"])
    artifact = rig.state / "queue" / "failed" / f"{identifier}.json"
    sidecar = artifact.with_name(artifact.name + ".error")
    for path, field in ((artifact, "delivery_error"), (sidecar, "error")):
        document = _read(path)
        error = document[field]
        assert isinstance(error, str) and len(error.encode("utf-8")) <= QUEUE_ERROR_MAX_BYTES
        assert path.stat().st_size <= chat_module._MAX_QUEUE_ARTIFACT_BYTES
    assert artifact.stat().st_size + sidecar.stat().st_size <= reserved
    assert rig.record("one")["phase"] == "delivery_uncertain"


def test_escaped_queue_prompt_cap_precedes_intake_and_legacy_cache_reload(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    # The source is exactly 32,000 UTF-8 bytes, but ensure_ascii JSON serializes
    # these characters to 96,000 bytes before metadata and prompt instructions.
    message = _message("escaped", text="\u0080" * 16_000)
    assert len(str(message["text"]).encode("utf-8")) == 32_000
    assert rig.bridge._message_source_bytes(message) < 64 << 10
    with monkeypatch.context() as limited:
        limited.setattr(chat_module, "_MAX_QUEUE_ARTIFACT_BYTES", 64 << 10)
        with pytest.raises(ValueError, match="queue artifact exceeds"):
            rig.runtime._input_event({"type": "message", "message": message, "cursor": "escaped"})
        assert not (rig.state / "input.json").exists()
        assert not (rig.state / "requests" / f"{_key('escaped')}.json").exists()
    rig.bridge._ingest_result({"messages": [message]})
    monkeypatch.setattr(chat_module, "_MAX_QUEUE_ARTIFACT_BYTES", 64 << 10)
    monkeypatch.setattr(rig.bridge, "_prime_prompt_cache", lambda: (_ for _ in ()).throw(
        AssertionError("oversized queued prompt reached retained cache construction")))
    with pytest.raises(ValueError, match="queue artifact exceeds"):
        rig.runtime._reload_records()


@pytest.mark.parametrize("phase", ["", "inbox", "failed"])
def test_queue_recovery_counts_crash_left_temporary_bytes_without_removing_them(
    rig: Rig, monkeypatch: pytest.MonkeyPatch, phase: str,
) -> None:
    rig.bridge._ingest_result({"messages": [_message("one")]})
    usage = rig.bridge.validate_aux_population()
    directory = rig.state / "queue" / phase
    directory.mkdir(mode=0o700, exist_ok=True)
    orphan = directory / ".message.crashed-before-rename"
    _write(orphan, {"retained": "x" * 4096})
    before = orphan.read_bytes()
    monkeypatch.setattr(chat_module, "_MAX_QUEUE_BYTES",
                        usage["queue_reserved_bytes"] + len(before) - 1)
    with pytest.raises(ValueError, match="queue artifact byte limit"):
        rig.runtime._reload_records()
    assert orphan.read_bytes() == before


def test_queue_recovery_caps_temporary_population_even_when_files_are_small(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(chat_module, "_MAX_REQUEST_RECORDS", 0)
    monkeypatch.setattr(chat_module, "_MAX_FEEDBACK_RECORDS", 0)
    paths = [rig.state / "queue" / f".message.orphan-{index}" for index in range(17)]
    for path in paths:
        _write(path, {})
    with pytest.raises(ValueError, match="temporary artifact population limit"):
        rig.runtime._reload_records()
    assert all(path.exists() for path in paths)


def test_full_normalized_source_bytes_bound_and_restart_preflight(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    first = _message("one", text="x" * 1024)
    first_bytes = rig.bridge._message_source_bytes(first)
    monkeypatch.setattr(chat_module, "_MAX_REQUEST_SOURCE_BYTES", first_bytes)
    rig.runtime._input_event({"type": "message", "message": first, "cursor": "one"})
    rig.runtime._reload_records()
    request_path = next((rig.state / "requests").glob("*.json"))
    assert rig.bridge._request_usage == {
        "records": 1, "source_bytes": first_bytes,
        "encoded_bytes": request_path.stat().st_size,
    }
    with pytest.raises(ValueError, match="request source byte limit"):
        rig.runtime._input_event({
            "type": "message", "message": _message("two"), "cursor": "two"})
    assert _read(rig.state / "input.json")["cursor"] == "one"


def test_single_message_cap_refuses_unbounded_resource_fields(rig: Rig) -> None:
    message = _message(
        "large-resource",
        id=_SPACE + "/messages/" + "m" * 40_000,
        thread=_SPACE + "/threads/" + "t" * 40_000,
        text="small body",
    )
    with pytest.raises(ValueError, match="outside the configured space"):
        rig.runtime._input_event({"type": "message", "message": message, "cursor": "large"})
    assert not (rig.state / "input.json").exists()
    assert list((rig.state / "requests").glob("*.json")) == []


def test_existing_over_cap_inbound_only_population_refuses_before_reload(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    disabled = Rig(tmp_path / "disabled-over-cap", outbound_mode="disabled")
    try:
        disabled.bridge._ingest_result({"messages": [_message("one"), _message("two")]})
        durable = sorted(path.read_bytes() for path in (disabled.state / "requests").glob("*.json"))
        monkeypatch.setattr(chat_module, "_MAX_REQUEST_RECORDS", 1)
        monkeypatch.setattr(
            disabled.bridge, "_prime_prompt_cache",
            lambda: (_ for _ in ()).throw(AssertionError("preflight constructed later indexes")))
        with pytest.raises(ValueError, match="request record limit 1 exceeded"):
            disabled.runtime._reload_records()
        assert sorted(path.read_bytes() for path in (disabled.state / "requests").glob("*.json")) == durable
        assert _read(disabled.state / "request-limit.json")["records"] == 2
    finally:
        disabled.finish()


@pytest.mark.parametrize(
    ("limit_name", "expected"),
    [
        ("_MAX_DEFERRED_RECORDS", "deferred message limit 0"),
        ("_MAX_DEFERRED_BYTES", "deferred message byte limit 0"),
    ],
)
def test_deferred_population_caps_refuse_echo_before_cursor_advance(
    rig: Rig, monkeypatch: pytest.MonkeyPatch, limit_name: str, expected: str,
) -> None:
    rig.runtime.pending_texts["matching pending text"] = 1
    monkeypatch.setattr(chat_module, limit_name, 0)
    echo = _message("echo", text="matching pending text")
    with pytest.raises(ValueError, match=expected):
        rig.runtime._input_event({"type": "message", "message": echo, "cursor": "echo"})
    assert not (rig.state / "input.json").exists()
    assert list((rig.state / "deferred").glob("*.json")) == []
    error = _read(rig.state / "population-limit.json")["error"]
    assert isinstance(error, str) and error.startswith(expected)


@pytest.mark.parametrize("cap", ["records", "bytes"])
def test_deferred_post_rename_failure_repairs_existing_replay_before_admission(
    rig: Rig, monkeypatch: pytest.MonkeyPatch, cap: str,
) -> None:
    text = "matching pending text"
    rig.runtime.pending_texts[text] = 1
    echo = _message("echo", text=text)
    path = rig.state / "deferred" / f"{_key('echo')}.json"
    original_write = _write

    def fail_after_rename(destination: Path, document: dict[str, object]) -> None:
        original_write(destination, document)
        if destination == path:
            raise OSError("injected deferred directory fsync failure")

    with monkeypatch.context() as failing:
        failing.setattr(runtime_module, "_write", fail_after_rename)
        with pytest.raises(OSError, match="deferred directory fsync failure"):
            rig.runtime._input_event({"type": "message", "message": echo, "cursor": "echo"})
    assert _read(path) == echo
    invalidated = rig.bridge._aux_usage is None
    assert invalidated
    assert not (rig.state / "input.json").exists()

    source_bytes = rig.bridge._message_source_bytes(echo)
    if cap == "records":
        monkeypatch.setattr(chat_module, "_MAX_DEFERRED_RECORDS", 1)
        expected = "deferred message limit 1"
    else:
        monkeypatch.setattr(chat_module, "_MAX_DEFERRED_BYTES", source_bytes)
        expected = "deferred message byte limit"
    rig.runtime._input_event({"type": "message", "message": echo, "cursor": "echo"})
    assert rig.bridge._aux_usage is not None
    cached = dict(rig.bridge._aux_usage)
    rebuilt = Bridge(rig.state, rig.harness, rig.transport).validate_aux_population()
    assert cached["deferred_records"] == rebuilt["deferred_records"] == 1
    assert cached["deferred_bytes"] == rebuilt["deferred_bytes"] == source_bytes
    assert _read(rig.state / "input.json")["cursor"] == "echo"

    with pytest.raises(ValueError, match=expected):
        rig.runtime._input_event({
            "type": "message", "message": _message("next", text=text), "cursor": "next"})
    assert _read(rig.state / "input.json")["cursor"] == "echo"
    assert list((rig.state / "deferred").glob("*.json")) == [path]


def test_empty_deadline_stage_does_not_read_or_scan_durable_state(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    rig.runtime.reconcile_requested = False
    rig.runtime.next_poll = float("inf")
    rig.runtime.feedback_dirty = False
    rig.runtime.deferred_dirty = False

    def forbidden_read(path: Path) -> dict[str, object]:
        raise AssertionError(f"steady-state stage read {path}")

    monkeypatch.setattr(runtime_module, "_read", forbidden_read)
    monkeypatch.setattr(rig.bridge, "_feedback_delivery",
                        lambda: (_ for _ in ()).throw(AssertionError("feedback scan")))
    for _ in range(100):
        rig.runtime._stage(set())


@pytest.mark.parametrize("entry", ["snapshot", "runtime"])
def test_aux_snapshot_decodes_each_queue_feedback_and_deferred_artifact_once(
    rig: Rig, monkeypatch: pytest.MonkeyPatch, entry: str,
) -> None:
    rig.accept()
    rig.runtime._stage()
    rig.until(lambda: rig.phase() == "awaiting_reply" and not rig.runtime.jobs)
    record = rig.record()
    feedback_key = "f" * 64
    feedback_id = f"feedback-{feedback_key}"
    feedback_path = rig.state / "feedback" / f"{feedback_key}.json"
    feedback_text = "bounded feedback"
    _write(feedback_path, {
        "queue_id": feedback_id, "text": feedback_text,
        "created_at": "2026-01-02T00:00:00Z",
    })
    enqueue(str(rig.state / "queue"), feedback_text, message_id=feedback_id,
            max_artifact_bytes=chat_module._MAX_QUEUE_ARTIFACT_BYTES)
    deferred = _message("deferred", text="possible echo")
    deferred_path = rig.state / "deferred" / f"{_key('deferred')}.json"
    _write(deferred_path, deferred)

    bridge = Bridge(rig.state, rig.harness, rig.transport)
    original_read = chat_module._read_bounded_json
    reads: dict[Path, int] = {}

    def count_read(
        path: Path, limit: int, label: str,
    ) -> tuple[dict[str, object], int]:
        if (path.parent == rig.state / "feedback"
                or path.parent == rig.state / "deferred"
                or path.parent == rig.state / "requests"
                or path.parent.parent == rig.state / "queue"):
            reads[path] = reads.get(path, 0) + 1
        return original_read(path, limit, label)

    monkeypatch.setattr(chat_module, "_read_bounded_json", count_read)
    prompt_index: dict[str, tuple[str, str]] | None
    if entry == "snapshot":
        snapshot = bridge.validate_aux_snapshot([record])
        prompt_index = snapshot.prompt_index
        feedback_pending = snapshot.feedback_pending
        deferred_messages = snapshot.deferred_messages
        expected_request_paths: list[Path] = []
        assert bridge._prompt_text_cache == prompt_index
    else:
        rig.runtime._reload_records()
        prompt_index = rig.bridge._prompt_text_cache
        feedback_pending = rig.runtime.feedback_pending
        deferred_messages = rig.runtime.deferred_messages
        expected_request_paths = [rig.state / "requests" / f"{record['key']}.json"]
        assert rig.runtime.feedback_dirty and rig.runtime.deferred_dirty

    queue_paths = list((rig.state / "queue").glob("*/*.json"))
    assert queue_paths
    assert reads == {path: 1 for path in [*queue_paths, feedback_path, deferred_path,
                                        *expected_request_paths]}
    assert prompt_index is not None
    assert set(prompt_index) == {
        str(record["queue_id"]), feedback_id,
    }
    assert feedback_pending == {feedback_id: (feedback_text, True)}
    assert deferred_messages == {deferred_path: deferred}


@pytest.mark.parametrize("entry", ["snapshot", "runtime"])
def test_recovery_refuses_request_feedback_queue_identity_collision(
    rig: Rig, monkeypatch: pytest.MonkeyPatch, entry: str,
) -> None:
    rig.bridge._ingest_result({"messages": [_message()]})
    request_path = rig.state / "requests" / f"{_key()}.json"
    record = _read(request_path)
    feedback_key = "e" * 64
    queue_id = f"feedback-{feedback_key}"
    record["queue_id"] = queue_id
    _write(request_path, record)
    feedback_path = rig.state / "feedback" / f"{feedback_key}.json"
    _write(feedback_path, {
        "queue_id": queue_id, "text": "feedback must not alias a request",
        "created_at": "2026-01-02T00:00:00Z",
    })
    enqueue(str(rig.state / "queue"), rig.bridge._prompt(record), message_id=queue_id,
            max_artifact_bytes=chat_module._MAX_QUEUE_ARTIFACT_BYTES)
    queued = rig.state / "queue" / "inbox" / f"{queue_id}.json"
    processed = rig.state / "queue" / "processed" / queued.name
    queued.rename(processed)
    before = {path: path.read_bytes() for path in (request_path, feedback_path, processed)}
    original = chat_module._read_bounded_json

    def read(
        path: Path, limit: int, label: str,
    ) -> tuple[dict[str, object], int]:
        assert path != processed, "collision was not refused before queue materialization"
        return original(path, limit, label)

    monkeypatch.setattr(chat_module, "_read_bounded_json", read)
    with pytest.raises(ValueError, match="request and feedback records share a queue identity"):
        if entry == "snapshot":
            rig.bridge.validate_aux_snapshot()
        else:
            rig.runtime._reload_records()
    assert {path: path.read_bytes() for path in before} == before
    assert not rig.harness.prompts and not rig.transport.requests


def test_bounded_json_refuses_unlinked_open_inode_and_oversized_replacement(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    path = tmp_path / "artifact.json"
    replacement = tmp_path / "replacement.json"
    original: dict[str, object] = {"value": "original"}
    _write(path, original)
    original_size = path.stat().st_size
    limit = original_size + 32
    _write(replacement, {"value": "x" * limit})
    assert replacement.stat().st_size > limit

    original_fstat = os.fstat
    replaced = False

    def replace_after_fstat(descriptor: int) -> os.stat_result:
        nonlocal replaced
        metadata = original_fstat(descriptor)
        if not replaced:
            replaced = True
            os.replace(replacement, path)
        return metadata

    monkeypatch.setattr(os, "fstat", replace_after_fstat)
    with pytest.raises(ValueError, match="changed while it was read"):
        chat_module._read_bounded_json(path, limit, "test artifact")
    assert path.stat().st_size > limit

    with pytest.raises(ValueError, match=f"exceeds its {limit}-byte encoded file limit"):
        chat_module._read_bounded_json(path, limit, "test artifact")


def test_bounded_json_refuses_open_file_growth_during_read(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    path = tmp_path / "growing.json"
    _write(path, {"value": "small"})
    limit = path.stat().st_size + 64
    original_read = os.read
    grew = False

    def grow_after_first_read(descriptor: int, count: int) -> bytes:
        nonlocal grew
        block = original_read(descriptor, count)
        if not grew:
            grew = True
            append_descriptor = os.open(path, os.O_WRONLY | os.O_APPEND)
            try:
                os.write(append_descriptor, b"x" * (limit + 1))
            finally:
                os.close(append_descriptor)
        return block

    monkeypatch.setattr(os, "read", grow_after_first_read)
    with pytest.raises(ValueError, match=f"exceeds its {limit}-byte encoded file limit"):
        chat_module._read_bounded_json(path, limit, "growing artifact")
    assert grew and path.stat().st_size > limit


def test_bounded_json_reports_exact_opened_bytes_and_refuses_symlink(tmp_path: Path) -> None:
    path = tmp_path / "spaced.json"
    encoded = b'{\n  "value": "exact bytes"\n}\n'
    path.write_bytes(encoded)
    path.chmod(0o600)
    document, encoded_size = chat_module._read_bounded_json(
        path, len(encoded), "spaced artifact")
    assert document == {"value": "exact bytes"}
    assert encoded_size == len(encoded) == path.stat().st_size

    alias = tmp_path / "alias.json"
    alias.symlink_to(path)
    with pytest.raises(AgentDeliveryError, match="cannot open symlinked artifact"):
        chat_module._read_bounded_json(alias, len(encoded), "symlinked artifact")


def test_compatibility_aux_validation_streams_bodies_without_snapshot_caches(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    for index in range(24):
        feedback_key = f"{index:064x}"
        _write(rig.state / "feedback" / f"{feedback_key}.json", {
            "queue_id": f"feedback-{feedback_key}", "text": "feedback " + "x" * 1000,
            "created_at": "2026-01-02T00:00:00Z",
        })
        name = f"deferred-{index}"
        _write(rig.state / "deferred" / f"{_key(name)}.json",
               _message(name, text="deferred " + "y" * 1000))
    enqueue(str(rig.state / "queue"), "feedback " + "x" * 1000,
            message_id=f"feedback-{0:064x}",
            max_artifact_bytes=chat_module._MAX_QUEUE_ARTIFACT_BYTES)
    original_read = chat_module._read_bounded_json
    original_snapshot = chat_module._AuxPopulationSnapshot
    bodies: list[weakref.ReferenceType[object]] = []
    peak_live = 0
    constructed = 0

    class BodyText(str):
        pass

    class BodyDocument(dict[str, object]):
        pass

    def read(
        path: Path, limit: int, label: str,
    ) -> tuple[dict[str, object], int]:
        nonlocal peak_live
        document, encoded_size = original_read(path, limit, label)
        if path.parent == rig.state / "feedback":
            text = BodyText(str(document["text"]))
            document["text"] = text
            bodies.append(weakref.ref(text))
        elif path.parent == rig.state / "deferred":
            document = BodyDocument(document)
            bodies.append(weakref.ref(document))
        peak_live = max(peak_live, sum(reference() is not None for reference in bodies))
        return document, encoded_size

    def snapshot(*, usage: dict[str, int], prompt_index: dict[str, tuple[str, str]],
                 feedback_pending: dict[str, tuple[str, bool]],
                 deferred_messages: dict[Path, dict[str, object]],
                 queue_reservations: dict[str, int],
                 queue_fixed_reservation: int) -> chat_module._AuxPopulationSnapshot:
        nonlocal constructed
        constructed += 1
        assert not prompt_index and not feedback_pending and not deferred_messages
        return original_snapshot(usage, prompt_index, feedback_pending, deferred_messages,
                                 queue_reservations, queue_fixed_reservation)

    def forbidden_prompt_index(prompt: str) -> tuple[str, str]:
        raise AssertionError("streaming validation constructed a prompt index")

    with monkeypatch.context() as streaming:
        streaming.setattr(chat_module, "_read_bounded_json", read)
        streaming.setattr(chat_module, "_AuxPopulationSnapshot", snapshot)
        streaming.setattr(rig.bridge, "_prompt_cache_entry", forbidden_prompt_index)
        usage = rig.bridge.validate_aux_population()
    assert constructed == 1 and len(bodies) == 48
    assert peak_live <= 3, "streaming validation retained earlier feedback/deferred bodies"
    assert all(reference() is None for reference in bodies)
    snapshot_result = rig.bridge.validate_aux_snapshot()
    assert usage == snapshot_result.usage
    assert len(snapshot_result.prompt_index) == 1
    assert len(snapshot_result.feedback_pending) == len(snapshot_result.deferred_messages) == 24


@pytest.mark.parametrize("limit", [
    "_MAX_FEEDBACK_RECORDS", "_MAX_FEEDBACK_BYTES",
    "_MAX_DEFERRED_RECORDS", "_MAX_DEFERRED_BYTES",
])
def test_aux_streaming_and_cached_validation_have_identical_refusal(
    rig: Rig, monkeypatch: pytest.MonkeyPatch, limit: str,
) -> None:
    feedback_key = "d" * 64
    _write(rig.state / "feedback" / f"{feedback_key}.json", {
        "queue_id": f"feedback-{feedback_key}", "text": "feedback",
        "created_at": "2026-01-02T00:00:00Z",
    })
    _write(rig.state / "deferred" / f"{_key('deferred')}.json", _message("deferred"))
    monkeypatch.setattr(chat_module, limit, 0)
    with pytest.raises(ValueError) as streaming:
        rig.bridge.validate_aux_population()
    diagnostic = _read(rig.state / "population-limit.json")
    with pytest.raises(ValueError) as cached:
        rig.bridge.validate_aux_snapshot()
    assert str(streaming.value) == str(cached.value)
    assert _read(rig.state / "population-limit.json") == diagnostic
    assert rig.bridge._aux_usage is None and rig.bridge._queue_reservations is None
    assert rig.bridge._prompt_text_cache is None


@pytest.mark.parametrize("entry", ["snapshot", "runtime", "compatibility"])
def test_failed_aux_scan_never_publishes_prefix_as_admission_authority(
    rig: Rig, monkeypatch: pytest.MonkeyPatch, entry: str,
) -> None:
    for index in range(2):
        feedback_key = f"{index:064x}"
        _write(rig.state / "feedback" / f"{feedback_key}.json", {
            "queue_id": f"feedback-{feedback_key}", "text": f"feedback-{index}",
            "created_at": "2026-01-02T00:00:00Z",
        })
    monkeypatch.setattr(chat_module, "_MAX_FEEDBACK_RECORDS", 2)
    original_read = chat_module._read_bounded_json
    read_count = 0
    before = {path: path.read_bytes() for path in (rig.state / "feedback").glob("*.json")}
    original_usage = rig.bridge.validate_aux_population()
    assert original_usage["feedback_records"] == 2

    def fail_second_feedback(
        path: Path, limit: int, label: str,
    ) -> tuple[dict[str, object], int]:
        nonlocal read_count
        if path.parent == rig.state / "feedback":
            read_count += 1
            # No prefix may replace even the previous complete population.
            assert rig.bridge._aux_usage == original_usage
            if read_count == 2:
                raise OSError("injected late feedback read fault")
        return original_read(path, limit, label)

    with monkeypatch.context() as failing:
        failing.setattr(chat_module, "_read_bounded_json", fail_second_feedback)
        with pytest.raises(OSError, match="late feedback read fault"):
            if entry == "runtime":
                rig.runtime._reload_records()
            elif entry == "snapshot":
                rig.bridge.validate_aux_snapshot()
            else:
                rig.bridge.validate_aux_population()
    assert read_count == 2
    invalidated = rig.bridge._aux_usage is None and rig.bridge._queue_reservations is None
    assert invalidated
    assert rig.bridge._prompt_text_cache is None
    # The routing event must rescan both durable files and refuse a third,
    # rather than accepting it using a plausible one-file prefix count.
    with pytest.raises(ValueError, match="feedback record limit 2 exceeded"):
        rig.bridge._unknown_reply_feedback("", {}, [], ["unknown-id"])
    assert {path: path.read_bytes() for path in (rig.state / "feedback").glob("*.json")} == before
    assert rig.bridge._aux_usage == original_usage
    assert rig.bridge.validate_aux_population() == original_usage


@pytest.mark.parametrize("entry", ["snapshot", "runtime"])
def test_aux_snapshot_waits_for_delivery_lock_across_phase_rename(
    rig: Rig, monkeypatch: pytest.MonkeyPatch, entry: str,
) -> None:
    rig.accept()
    rig.runtime._stage()
    rig.until(lambda: rig.phase() == "awaiting_reply" and not rig.runtime.jobs)
    queue_id = str(rig.record()["queue_id"])
    source = rig.state / "queue" / "processed" / f"{queue_id}.json"
    destination = rig.state / "queue" / "failed" / source.name
    assert source.exists() and not destination.exists()

    lock_path = rig.state / "queue" / ".delivery.lock"
    descriptor = os.open(lock_path, os.O_RDWR | os.O_CREAT | os.O_CLOEXEC, 0o600)
    lock_metadata = os.fstat(descriptor)
    lock_identity = (lock_metadata.st_dev, lock_metadata.st_ino)
    original_flock = fcntl.flock
    original_scan = Bridge._aux_population_locked
    scan_attempted = threading.Event()
    scan_entered = threading.Event()
    outcome: list[object] = []

    def observed_flock(fd: int, operation: int) -> None:
        metadata = os.fstat(fd)
        if (threading.current_thread().name == "aux-scan"
                and (metadata.st_dev, metadata.st_ino) == lock_identity):
            scan_attempted.set()
        original_flock(fd, operation)

    def observed_scan(
        self: Bridge, records: Sequence[dict[str, object]] | None = None,
        *, collect_caches: bool = False,
    ) -> object:
        scan_entered.set()
        return original_scan(self, records, collect_caches=collect_caches)

    monkeypatch.setattr(fcntl, "flock", observed_flock)
    monkeypatch.setattr(Bridge, "_aux_population_locked", observed_scan)
    original_flock(descriptor, fcntl.LOCK_EX)
    bridge = (Bridge(rig.state, rig.harness, rig.transport)
              if entry == "snapshot" else rig.bridge)

    def scan() -> None:
        try:
            if entry == "snapshot":
                outcome.append(bridge.validate_aux_snapshot())
            else:
                rig.runtime._reload_records()
                outcome.append(None)
        except BaseException as exc:
            outcome.append(exc)

    worker = threading.Thread(target=scan, name="aux-scan")
    try:
        worker.start()
        assert scan_attempted.wait(5)
        assert not scan_entered.is_set(), "auxiliary scan crossed the held delivery lock"
        os.replace(source, destination)
        os.close(descriptor)
        descriptor = -1
        assert scan_entered.wait(5)
        worker.join(timeout=5)
    finally:
        if descriptor >= 0:
            os.close(descriptor)
        if worker.is_alive():
            worker.join(timeout=5)
    assert not worker.is_alive()
    assert len(outcome) == 1 and not isinstance(outcome[0], BaseException)
    if entry == "snapshot":
        snapshot = outcome[0]
        assert isinstance(snapshot, chat_module._AuxPopulationSnapshot)
        assert queue_id in snapshot.prompt_index
    else:
        assert outcome == [None]
        assert bridge._prompt_text_cache is not None
        assert queue_id in bridge._prompt_text_cache
    assert bridge._aux_usage is not None
    assert bridge._aux_usage["queue_records"] == 1
    controls = [rig.state / "queue" / name for name in (
        "target.json", ".delivery.lock", ".binding.lock")]
    assert bridge._aux_usage["queue_bytes"] == (
        destination.stat().st_size + sum(path.stat().st_size for path in controls if path.exists()))
    assert destination.exists() and not source.exists()


def test_recovery_preserves_prompt_registration_before_worker_enqueue(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    blocked = Gate()

    def delayed_enqueue(root: str, text: str, *, message_id: str,
                        max_artifact_bytes: int | None = None,
                        atomic_policy: AtomicWritePolicy | None = None) -> str:
        blocked.wait()
        return enqueue(root, text, message_id=message_id,
                       max_artifact_bytes=max_artifact_bytes, atomic_policy=atomic_policy)

    monkeypatch.setattr(runtime_module, "enqueue", delayed_enqueue)
    try:
        rig.accept()
        assert blocked.entered.wait(5)
        registered = dict(rig.runtime.delivery_inflight)
        assert len(registered) == 1
        identifier, prompt = next(iter(registered.items()))
        assert not (rig.state / "queue" / "inbox" / f"{identifier}.json").exists()
        rig.runtime._reload_records()
        assert rig.bridge._prompt_text_cache is not None
        assert rig.bridge._prompt_text_cache[identifier] == rig.bridge._prompt_cache_entry(prompt)
        # A failed tentative enqueue is still removable after recovery; the
        # registration must not turn an absent artifact into durable authority.
        rig.bridge._forget_tentative_prompt(identifier)
        assert identifier not in rig.bridge._prompt_text_cache
        rig.bridge._remember_prompt(identifier, prompt)
        blocked.release.set()
        rig.until(lambda: rig.phase() == "awaiting_reply" and not rig.runtime.jobs)
        assert rig.bridge._prompt_echo_views(prompt) == ("", "")
        assert not blocked.expired.is_set()
    finally:
        blocked.release.set()


def test_real_output_event_uses_prompt_index_without_queue_directory_scan(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    rig.accept()
    rig.runtime._stage()
    rig.until(lambda: rig.phase() == "awaiting_reply")
    record = rig.record()
    nonce = str(record["reply_nonce"])
    pane_id = rig.bridge.config.target.pane_id
    assert pane_id is not None
    snapshot = PaneOutputSnapshot(
        pane_id,
        rig.harness.prompts[0]
        + f"\n<CHAT_REPLY_{nonce}_1>\nCached answer\n</CHAT_REPLY_{nonce}_1>",
        False,
        0,
    )
    rig.runtime.output_inflight["snapshot"] = snapshot
    original_read = chat_module._read
    original_glob = Path.glob
    queue_reads: list[Path] = []

    def observe_queue_read(path: Path) -> dict[str, object]:
        if path.parent.name in ("processed", "inflight", "inbox", "failed"):
            queue_reads.append(path)
        return original_read(path)

    def forbid_queue_glob(path: Path, pattern: str) -> Iterator[Path]:
        if path.name in ("queue", "feedback", "deferred") or path.parent.name == "queue":
            raise AssertionError(f"real output event scanned hot-path directory {path}")
        return original_glob(path, pattern)

    monkeypatch.setattr(chat_module, "_read", observe_queue_read)
    monkeypatch.setattr(Path, "glob", forbid_queue_glob)
    selected = rig.runtime._complete(_Completion(
        "output", "snapshot", snapshot, None, "2026-01-02T00:01:00Z"))
    assert selected == {_key()}
    assert queue_reads == [
        rig.state / "queue" / phase / f"{record['queue_id']}.json"
        for phase in ("inbox", "inflight", "processed")
    ]
    assert [item["text"] for item in rig.runtime.reply_items_by_key[_key()]] == [
        "Cached answer"]


def test_a_delivery_batch_reaches_the_agent_in_message_creation_order(rig: Rig) -> None:
    # `drain` types prompts in the order they were queued. While the first delivery
    # waits on the pane lookup, "later" and then "earlier" arrive and wait together for
    # the next one. "earlier"'s message was created first, so it must be queued, and
    # typed, first.
    lookup = rig.harness.lookup = Gate()
    rig.accept()
    assert lookup.entered.wait(5)
    rig.accept("later", "cursor-later", created_at="2026-01-02T00:00:01Z")
    rig.accept("earlier", "cursor-earlier")
    assert list(rig.runtime.delivery_prompts) == [
        get_str(rig.record(name), "queue_id", "request") for name in ("later", "earlier")]
    lookup.release.set()
    rig.until(lambda: all(rig.phase(name) == "awaiting_reply" for name in ("one", "later", "earlier")))
    assert len(rig.harness.prompts) == 3
    assert "messages/one" in rig.harness.prompts[0]
    assert "messages/earlier" in rig.harness.prompts[1]
    assert "messages/later" in rig.harness.prompts[2]


def test_prompt_created_during_drain_cooldown_is_retained_until_worker_accepts_it(
    rig: Rig,
) -> None:
    rig.accept()
    rig.runtime._stage()
    rig.until(lambda: rig.phase() == "awaiting_reply")
    identifier = "feedback-cooldown"
    prompt = "Durable feedback created while the delivery lane cools down"
    rig.runtime.feedback_pending[identifier] = (prompt, False)
    rig.runtime.feedback_dirty = True
    rig.runtime.next_drain = time.monotonic() + 30

    rig.runtime._stage(set())
    assert rig.runtime.delivery_prompts == {identifier: prompt}
    assert ("drain", "queue") not in rig.runtime.jobs
    rig.runtime.next_drain = 0
    rig.runtime._stage(set())
    rig.until(lambda: prompt in rig.harness.prompts and ("drain", "queue") not in rig.runtime.jobs)
    assert rig.harness.prompts.count(prompt) == 1
    assert identifier not in rig.runtime.delivery_prompts


def test_full_worker_lanes_do_not_create_zero_timeout_retry_spin(rig: Rig) -> None:
    rig.accept()
    rig.runtime.reconcile_requested = False
    rig.runtime.next_poll = float("inf")
    rig.runtime.next_recovery = time.monotonic() + 300
    rig.runtime.jobs.update({("ack", f"ack-{index}") for index in range(4)})
    rig.runtime.jobs.update({("send", f"send-{index}") for index in range(2)})
    rig.runtime.retry[("send", _key())] = (0, 1)

    now = time.monotonic()
    assert rig.runtime._due_keys(now) == set()
    assert rig.runtime._next_deadline() > now + 250


def test_reply_captured_before_prompt_delivery_does_not_create_zero_timeout_spin(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    rig.bridge._ingest_result({"messages": [_message()]})
    path = rig.state / "requests" / f"{_key()}.json"
    record = _read(path)
    record["phase"] = "queued"
    ack = as_mapping(record["ack"], "ack")
    ack.update(state="acked", reaction_id=_SPACE + "/messages/one/reactions/robot",
               acked_at="2026-01-02T00:00:01Z", error=None, next_retry_at=None)
    record["ack"] = ack
    text = "Captured while the coordinator pane is still busy"
    reply = {
        "reply_key": "0",
        "request_id": rig.bridge._reply_identity(record, 0),
        "reply_source": "capture", "reply_ordinal": 1, "text": text,
    }
    durable, created = rig.bridge._create_reply_item(record, 0, reply)
    assert created and durable == reply
    size = len(text.encode())
    record.update(reply_item_count=1, reply_sent_count=0,
                  reply_total_bytes=size, reply_pending_bytes=size,
                  reply_next_ordinal=2)
    _write(path, record)
    rig.runtime._reload_records()
    rig.runtime.reconcile_requested = False
    rig.runtime.next_poll = float("inf")
    rig.runtime.next_recovery = time.monotonic() + 300
    rig.runtime.retry[("send", _key())] = (0, 1)
    started: list[str] = []
    monkeypatch.setattr(
        rig.runtime, "_send_start",
        lambda _path, saved, _reply: started.append(str(saved["key"])),
    )

    now = time.monotonic()
    assert rig.runtime._due_keys(now) == set()
    rig.runtime._stage({_key()})
    assert started == []
    assert rig.runtime._next_deadline() > now + 250

    rig.runtime.records[_key()][1]["phase"] = "awaiting_reply"
    ready = time.monotonic()
    assert rig.runtime._due_keys(ready) == {_key()}
    assert rig.runtime._next_deadline() - ready < 0.1
    rig.runtime._stage({_key()})
    assert started == [_key()]


def test_send_deadline_uses_earliest_eligible_retry_not_record_order(rig: Rig) -> None:
    for name in ("one", "two"):
        rig.bridge._ingest_result({"messages": [_message(name)]})
        path = rig.state / "requests" / f"{_key(name)}.json"
        record = _read(path)
        record["phase"] = "awaiting_reply"
        ack = as_mapping(record["ack"], "ack")
        ack.update(state="acked", reaction_id=None, acked_at="2026-01-02T00:00:01Z",
                   error=None, next_retry_at=None)
        record["ack"] = ack
        text = f"pending {name}"
        reply = {
            "reply_key": "0",
            "request_id": rig.bridge._reply_identity(record, 0),
            "reply_source": "capture", "reply_ordinal": 1, "text": text,
        }
        durable, created = rig.bridge._create_reply_item(record, 0, reply)
        assert created and durable == reply
        size = len(text.encode())
        record.update(reply_item_count=1, reply_sent_count=0,
                      reply_total_bytes=size, reply_pending_bytes=size,
                      reply_next_ordinal=2)
        _write(path, record)
    rig.runtime._reload_records()
    rig.runtime.reconcile_requested = False
    rig.runtime.next_poll = float("inf")
    rig.runtime.next_recovery = time.monotonic() + 300

    now = time.monotonic()
    later = now + 60
    earlier = now + 1
    assert set(rig.runtime.record_order) == {_key("one"), _key("two")}
    late_key, early_key = rig.runtime.record_order
    rig.runtime.retry[("send", late_key)] = (later, 60)
    rig.runtime.retry[("send", early_key)] = (earlier, 1)
    assert rig.runtime._next_deadline() == earlier


@pytest.mark.parametrize("completed_index", [0, 1], ids=("first-slot", "second-slot"))
def test_send_fairness_starts_third_request_before_second_item_on_either_freed_slot(
    rig: Rig, monkeypatch: pytest.MonkeyPatch, completed_index: int,
) -> None:
    keys: list[str] = []
    for index, name in enumerate(("one", "two", "three"), start=1):
        rig.bridge._ingest_result({"messages": [_message(name)]})
        key = _key(name)
        keys.append(key)
        path = rig.state / "requests" / f"{key}.json"
        record = _read(path)
        record.update(phase="awaiting_reply", received_at=f"2026-01-02T00:00:0{index}Z")
        ack = as_mapping(record["ack"], "ack")
        ack.update(state="disabled", emoji=None)
        record["ack"] = ack
        count = 2 if name == "one" else 1
        total_bytes = 0
        for item_index in range(count):
            text = f"{name}-{item_index}"
            total_bytes += len(text.encode())
            item: dict[str, object] = {
                "reply_key": str(item_index),
                "request_id": rig.bridge._reply_identity(record, item_index),
                "reply_source": "capture", "reply_ordinal": item_index + 1,
                "text": text,
            }
            durable, created = rig.bridge._create_reply_item(record, item_index, item)
            assert created and durable == item
        record.update(reply_item_count=count, reply_sent_count=0,
                      reply_total_bytes=total_bytes, reply_pending_bytes=total_bytes,
                      reply_next_ordinal=count + 1)
        _write(path, record)
    rig.runtime._reload_records()
    rig.runtime.reconcile_requested = False
    rig.runtime.next_poll = float("inf")
    rig.runtime.feedback_dirty = False
    rig.runtime.deferred_dirty = False
    started: list[tuple[str, str]] = []

    def start(path: Path, record: dict[str, object], reply: dict[str, object]) -> None:
        key = str(record["key"])
        started.append((key, str(reply["reply_key"])))
        rig.runtime.send_cursor = key
        rig.runtime.jobs.add(("send", key))

    monkeypatch.setattr(rig.runtime, "_send_start", start)
    rig.runtime._stage()
    assert started == [(keys[0], "0"), (keys[1], "0")]

    def complete_first(index: int) -> None:
        key = keys[index]
        path = rig.state / "requests" / f"{key}.json"
        record = _read(path)
        items = rig.runtime.reply_items_by_key[key]
        rig.bridge._reply_complete(
            path, record, items[0], _SPACE + f"/messages/{index}-0",
            "2026-01-02T00:01:00Z", None)
        items.pop(0)
        rig.runtime._cache_record(path, record)
        rig.runtime.jobs.discard(("send", key))
        rig.runtime._refresh_reply_index(key, items)

    # Whichever initial slot finishes, the never-started third request wins it;
    # request one may still have a second durable item waiting behind that turn.
    complete_first(completed_index)
    due = rig.runtime._due_keys(time.monotonic())
    assert keys[2] in due
    rig.runtime._stage({keys[completed_index], *due})
    assert started[-1] == (keys[2], "0"), "fresh third request must take the first free slot"

    # The second item on request one is still indexed and starts at the next free slot.
    complete_first(1 - completed_index)
    due = rig.runtime._due_keys(time.monotonic())
    rig.runtime._stage({keys[1 - completed_index], *due})
    assert started[-1] == (keys[0], "1")


@pytest.mark.parametrize("lane", ["ack", "send"])
def test_disabled_runtime_transport_boundary_never_reaches_adapter(
    tmp_path: Path, lane: str,
) -> None:
    disabled = Rig(tmp_path / lane, outbound_mode="disabled")
    try:
        disabled.runtime._transport(lane, "guard", {"action": "react" if lane == "ack" else "send"})
        notice = disabled.runtime.events.get(timeout=5)
        assert notice.kind == "complete"
        completion = notice.value
        assert isinstance(completion, runtime_module._Completion)
        assert isinstance(completion.error, ValueError)
        assert "outbound Chat is disabled" in str(completion.error)
        assert disabled.transport.requests == []
        disabled.runtime.jobs.discard((lane, "guard"))
    finally:
        disabled.finish()


def test_disabled_streaming_stages_inbound_prompt_but_no_ack_send_or_output_watch(
    tmp_path: Path,
) -> None:
    disabled = Rig(tmp_path / "disabled", outbound_mode="disabled")
    try:
        disabled.accept()
        disabled.runtime._stage()
        disabled.until(lambda: disabled.phase() == "awaiting_reply" and not disabled.runtime.jobs)
        record = disabled.record()
        key = str(record["key"])
        ack = as_mapping(record["ack"], "ack")
        ack.update(state="pending", emoji="🤖", error="retry", next_retry_at=None)
        record.update(phase="reply_pending", ack=ack, reply_nonce="A" * 22, reply_protocol=2)
        _write(disabled.state / "requests" / f"{key}.json", record)
        _write(disabled.state / "replies" / f"{key}.json", {
            "text": "preexisting", "items": [{
                "reply_key": "0", "request_id": record["request_id"], "text": "preexisting",
            }],
        })
        disabled.runtime._reload_records()
        disabled.runtime._stage()

        assert disabled.transport.calls("react") == []
        assert disabled.transport.calls("send") == []
        assert as_mapping(disabled.record()["ack"], "ack")["state"] == "disabled"
        desired = disabled.runtime.output.desired
        assert desired is not None and desired.markers == () and not desired.watching
        assert len(disabled.harness.prompts) == 1
        assert "no Chat reply will be published" in disabled.harness.prompts[0]
    finally:
        disabled.finish()


def test_reply_wake_is_nonblocking_when_unread_datagram_queue_is_full(rig: Rig) -> None:
    rig.accept()
    rig.runtime._stage()
    rig.until(lambda: rig.phase() == "awaiting_reply")
    rig.runtime.local.close()
    path = rig.state / ".wake.sock"
    with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as listener:
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 1024)
        listener.bind(str(path))
        for _ in range(128):
            submit_reply(rig.state, _key(), "Completed")
    assert _read(rig.state / "submissions" / f"{_key()}.json")["text"] == "Completed"


def test_local_reply_wake_stages_send_without_waiting_for_recovery(rig: Rig) -> None:
    rig.accept()
    rig.runtime._stage()
    rig.until(lambda: rig.phase() == "awaiting_reply" and rig.ack() == "acked")
    rig.runtime.local.thread.start()
    rig.runtime.next_recovery = time.monotonic() + 300
    submit_reply(rig.state, _key(), "Immediate")
    notice = rig.runtime.events.get(timeout=5)
    assert notice == _Notice("local_reply", _key())
    needed, selected = rig.runtime._handle(notice)
    assert needed and selected == {_key()}
    rig.runtime._stage(selected)
    rig.until(lambda: rig.phase() == "replied")
    assert len(rig.transport.calls("send")) == 1
    assert rig.runtime.next_recovery > time.monotonic() + 250


def test_missed_local_reply_wake_is_adopted_by_fixed_recovery(rig: Rig) -> None:
    rig.accept()
    rig.runtime._stage()
    rig.until(lambda: rig.phase() == "awaiting_reply" and rig.ack() == "acked")
    assert rig.runtime.recovery_interval == 300
    submit_reply(rig.state, _key(), "Recovered")
    assert rig.runtime.reply_items_by_key.get(_key(), []) == []
    # Model the unconditional 300-second branch in run(): it reloads durable
    # summaries/submissions even when the best-effort datagram was missed.
    rig.runtime._reload_records()
    rig.runtime._stage({_key()})
    rig.until(lambda: rig.phase() == "replied")
    assert len(rig.transport.calls("send")) == 1


def test_fixed_recovery_reads_pending_and_submission_without_sent_history(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    rig.accept()
    rig.runtime._stage()
    rig.until(lambda: rig.phase() == "awaiting_reply" and rig.ack() == "acked")
    _queue_snapshot(rig, _reply_snapshot(rig, ["sent history"]))
    rig.until(lambda: rig.phase() == "replied")
    first = rig.record()
    second_snapshot = _reply_snapshot(rig, ["sent history", "still pending"])
    assert rig.bridge.capture_output(second_snapshot, deliver=False)["errors"] == []

    rig.accept("two", "cursor-two")
    rig.runtime._stage()
    rig.until(lambda: rig.phase("two") == "awaiting_reply")
    submit_reply(rig.state, _key("two"), "local submission")

    original_read = chat_module._read

    def forbid_history(path: Path) -> dict[str, object]:
        if "history" in path.parts:
            raise AssertionError(f"fixed recovery read immutable history {path}")
        return original_read(path)

    monkeypatch.setattr(chat_module, "_read", forbid_history)
    monkeypatch.setattr(runtime_module, "_read", forbid_history)
    rig.runtime._reload_records()
    assert [item["text"] for item in rig.runtime.reply_items_by_key[str(first["key"])]] == [
        "still pending"]
    assert [item["text"] for item in rig.runtime.reply_items_by_key[_key("two")]] == [
        "local submission"]


@pytest.mark.parametrize("phase", ["inbox", "inflight", "processed", "failed"])
def test_received_request_accepts_a_file_reply_from_its_matching_queue_artifact(
    rig: Rig, phase: str,
) -> None:
    rig.bridge._ingest_result({"messages": [_message()]})
    record = rig.record()
    queue_id = str(record["queue_id"])
    enqueue(str(rig.state / "queue"), rig.bridge._prompt(record), message_id=queue_id)
    if phase != "inbox":
        (rig.state / "queue" / "inbox" / f"{queue_id}.json").rename(
            rig.state / "queue" / phase / f"{queue_id}.json")
    submit_reply(rig.state, _key(), "Completed")
    submit_reply(rig.state, _key(), "Completed")
    assert rig.record() == record
    assert _read(rig.state / "submissions" / f"{_key()}.json")["text"] == "Completed"


def test_stopped_runner_file_submission_backlog_is_bounded(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    for name in ("one", "two"):
        rig.bridge._ingest_result({"messages": [_message(name)]})
        record = rig.record(name)
        enqueue(
            str(rig.state / "queue"), rig.bridge._prompt(record),
            message_id=str(record["queue_id"]))
    monkeypatch.setattr(chat_module, "_MAX_PENDING_REPLY_ITEMS", 1)
    submit_reply(rig.state, _key("one"), "first")
    with pytest.raises(ValueError, match="local reply submission count limit 1"):
        submit_reply(rig.state, _key("two"), "second")
    assert [path.stem for path in (rig.state / "submissions").glob("*.json")] == [
        _key("one")]


@pytest.mark.parametrize("queue_id", ["../escape", "/absolute", "0" * 20 + "-" + "f" * 64])
def test_received_reply_rejects_unsafe_or_unrelated_queue_ids_before_path_use(
    rig: Rig, monkeypatch: pytest.MonkeyPatch, queue_id: str,
) -> None:
    rig.bridge._ingest_result({"messages": [_message()]})
    record = rig.record()
    record["queue_id"] = queue_id
    _write(rig.state / "requests" / f"{_key()}.json", record)

    def forbidden_lookup(root: str) -> None:
        raise AssertionError("invalid queue identity reached filesystem lookup")

    monkeypatch.setattr(chat_module, "_validate_existing_queue", forbidden_lookup)
    with pytest.raises(ValueError, match="queue identity"):
        submit_reply(rig.state, _key(), "Must not be accepted")
    assert list((rig.state / "replies").glob("*.json")) == []
    assert list((rig.state / "replies" / "items").rglob("*.json")) == []
    assert list((rig.state / "submissions").glob("*.json")) == []


@pytest.mark.parametrize("tamper", ["wrong-id", "symlink", "directory-symlink"])
def test_received_reply_requires_an_owned_matching_queue_artifact(rig: Rig, tamper: str) -> None:
    rig.bridge._ingest_result({"messages": [_message()]})
    record = rig.record()
    queue_id = str(record["queue_id"])
    enqueue(str(rig.state / "queue"), rig.bridge._prompt(record), message_id=queue_id)
    inbox = rig.state / "queue" / "inbox"
    artifact = inbox / f"{queue_id}.json"
    if tamper == "wrong-id":
        document = _read(artifact)
        document["id"] = "different-request"
        _write(artifact, document)
    elif tamper == "symlink":
        moved = artifact.with_suffix(".saved")
        artifact.rename(moved)
        artifact.symlink_to(moved)
    else:
        moved = inbox.with_name("saved-inbox")
        inbox.rename(moved)
        inbox.symlink_to(moved, target_is_directory=True)
    with pytest.raises((ValueError, AgentDeliveryError)):
        submit_reply(rig.state, _key(), "Must not be accepted")
    assert list((rig.state / "replies").glob("*.json")) == []
    assert list((rig.state / "replies" / "items").rglob("*.json")) == []
    assert list((rig.state / "submissions").glob("*.json")) == []


def test_received_reply_follows_an_artifact_that_moves_during_lookup(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    rig.bridge._ingest_result({"messages": [_message()]})
    record = rig.record()
    queue_id = str(record["queue_id"])
    enqueue(str(rig.state / "queue"), rig.bridge._prompt(record), message_id=queue_id)
    inbox = rig.state / "queue" / "inbox" / f"{queue_id}.json"
    processed = rig.state / "queue" / "processed" / f"{queue_id}.json"

    def moving_read(path: Path) -> dict[str, object]:
        if path == inbox:
            inbox.rename(processed)
        return _read(path)

    monkeypatch.setattr(chat_module, "_read", moving_read)
    submit_reply(rig.state, _key(), "Completed")
    assert rig.record() == record
    assert processed.exists()
    assert _read(rig.state / "submissions" / f"{_key()}.json")["text"] == "Completed"


def test_received_reply_retries_an_atomic_change_then_follows_a_phase_move(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    rig.bridge._ingest_result({"messages": [_message()]})
    record = rig.record()
    queue_id = str(record["queue_id"])
    enqueue(str(rig.state / "queue"), rig.bridge._prompt(record), message_id=queue_id)
    artifact = rig.state / "queue" / "inbox" / f"{queue_id}.json"
    processed = rig.state / "queue" / "processed" / f"{queue_id}.json"
    original = read_json
    races = 0

    def read(
        path: Path, selected: ArtifactClass | None = None,
    ) -> tuple[dict[str, object], int]:
        nonlocal races
        if path == artifact and races < 2:
            races += 1
            if races == 2:
                artifact.rename(processed)
            raise AgentDeliveryError(f"saved queue artifact changed while it was read: {path}")
        return original(path, selected)

    monkeypatch.setattr(chat_module, "read_json", read)
    submit_reply(rig.state, _key(), "Completed")
    assert races == 2 and processed.exists()
    assert rig.record() == record
    assert _read(rig.state / "submissions" / f"{_key()}.json")["text"] == "Completed"


@pytest.mark.parametrize("failed_commit", ["request", "ack-attempt"])
def test_failed_request_or_attempt_commit_cannot_start_external_effects(
    rig: Rig, monkeypatch: pytest.MonkeyPatch, failed_commit: str,
) -> None:
    def failed_write(path: Path, document: dict[str, object]) -> None:
        if path.parent == rig.state / "requests":
            raise OSError("fixture persistence failure")
        _write(path, document)

    module = chat_module if failed_commit == "request" else runtime_module
    monkeypatch.setattr(module, "_write", failed_write)
    with pytest.raises(OSError, match="persistence failure"):
        rig.accept()
    assert rig.transport.requests == []
    assert rig.harness.prompts == []
    assert not (rig.state / "input.json").exists()
    for phase in ("inbox", "inflight", "processed", "failed"):
        assert list((rig.state / "queue" / phase).glob("*.json")) == []
    if failed_commit == "request":
        assert list((rig.state / "requests").glob("*.json")) == []
    else:
        assert as_mapping(rig.record()["ack"], "ack")["attempts"] == 0


def test_fast_completion_with_full_mailbox_cannot_block_owner_start(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    events = rig.runtime.events
    for _ in range(events.maxsize):
        events.put_nowait(_Notice("fixture"))
    worker_ready = threading.Event()
    owner_returned = threading.Event()
    completion_posted = threading.Event()
    original_post = runtime_module._post
    original_submit = rig.runtime.pools["ack"].submit

    def post(mailbox: queue.Queue[_Notice], notice: _Notice, stop: threading.Event) -> None:
        worker_ready.set()
        original_post(mailbox, notice, stop)
        completion_posted.set()

    def submit(operation: Callable[[], object]) -> Future[object]:
        future = original_submit(operation)
        future.add_done_callback(lambda completed: worker_ready.set())
        # Return only once the fast operation has completed or its worker has
        # reached the full mailbox. This forces the completion-registration race.
        assert worker_ready.wait(5)
        return future

    monkeypatch.setattr(runtime_module, "_post", post)
    monkeypatch.setattr(rig.runtime.pools["ack"], "submit", submit)

    def start() -> None:
        rig.runtime._start("ack", "fixture", lambda: {"id": "fixture"})
        owner_returned.set()

    owner = threading.Thread(target=start)
    owner.start()
    try:
        assert owner_returned.wait(5), "the event-loop owner waited for space in its own mailbox"
        assert events.full()
        assert not completion_posted.is_set()
    finally:
        events.get_nowait()
        owner.join(timeout=5)
    assert not owner.is_alive()
    assert completion_posted.wait(5)
    notices = [events.get_nowait() for _ in range(events.maxsize)]
    assert sum(notice.kind == "complete" for notice in notices) == 1


@pytest.mark.parametrize("state", ["working", "blocked"])
def test_busy_or_blocked_harness_does_not_delay_ack(rig: Rig, state: str) -> None:
    rig.harness.state = state
    rig.accept()
    rig.runtime._stage()
    rig.until(lambda: rig.ack() == "acked" and rig.phase() == "queued")
    assert rig.ack() == "acked"
    assert rig.phase() == "queued"
    assert rig.harness.prompts == []
    rig.harness.state = "idle"
    rig.runtime.next_drain = 0
    rig.runtime._stage()
    rig.until(lambda: rig.phase() == "awaiting_reply")
    assert len(rig.harness.prompts) == 1


def test_slow_harness_lookup_cannot_hold_intake_or_ack_workers(rig: Rig) -> None:
    lookup = rig.harness.lookup = Gate()
    rig.accept()
    rig.runtime._stage()
    assert lookup.entered.wait(5)
    rig.accept("two", "cursor-two")
    rig.runtime._stage()
    rig.until(lambda: rig.ack("one") == "acked" and rig.ack("two") == "acked")
    assert not lookup.release.is_set()
    assert not lookup.expired.is_set()
    assert _read(rig.state / "input.json")["cursor"] == "cursor-two"
    assert rig.harness.prompts == []
    lookup.release.set()
    rig.until(lambda: rig.phase("one") == "awaiting_reply" and rig.phase("two") == "awaiting_reply")


def test_delayed_poll_and_send_do_not_stall_new_push_acceptance(rig: Rig) -> None:
    poll = rig.transport.gate("poll")
    rig.runtime.reconcile_requested = True
    rig.runtime._stage()
    assert poll.entered.wait(5)
    rig.accept()
    rig.runtime._stage()
    rig.until(lambda: rig.phase() == "awaiting_reply" and rig.ack() == "acked")
    send = rig.transport.gate("send")
    submit_reply(rig.state, _key(), "First answer")
    rig.runtime._stage()
    assert send.entered.wait(5)
    rig.accept("two", "cursor-two")
    rig.runtime._stage()
    rig.until(lambda: rig.phase("two") == "awaiting_reply" and rig.ack("two") == "acked")
    assert not poll.release.is_set() and not send.release.is_set()
    assert _read(rig.state / "input.json")["cursor"] == "cursor-two"
    assert len(rig.harness.prompts) == 2
    assert len(rig.transport.calls("poll")) == 1
    assert len(rig.transport.calls("send")) == 1


def test_cursor_commit_follows_durable_intake_and_restart_replays_only_once(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    original = _write
    observed: list[object] = []
    ack = rig.transport.gate("react")

    def crash_cursor(path: Path, document: dict[str, object]) -> None:
        if path == rig.state / "input.json":
            saved = rig.record()
            assert saved["phase"] == "received"
            assert as_mapping(saved["message"], "source")["id"] == _SPACE + "/messages/one"
            assert as_mapping(saved["ack"], "ack")["attempts"] == 1
            assert ack.entered.wait(5)
            assert rig.harness.prompted.wait(5)
            observed.append(saved["reply_nonce"])
            raise OSError("simulated crash before cursor commit")
        original(path, document)

    monkeypatch.setattr(runtime_module, "_write", crash_cursor)
    with pytest.raises(OSError, match="before cursor"):
        rig.accept()
    assert len(observed) == 1
    assert not (rig.state / "input.json").exists()
    # Lose the old owner's completions after the effects reached their workers.
    # The restarted owner must use the durable queue and ACK retry identities.
    rig.runtime.stop.set()
    ack.release.set()
    for pool in rig.runtime.pools.values():
        pool.shutdown(wait=True, cancel_futures=True)
    retry_at = as_mapping(rig.record()["ack"], "ack")["next_retry_at"]
    assert isinstance(retry_at, str)
    retry_due = datetime.fromisoformat(retry_at.replace("Z", "+00:00")) + timedelta(seconds=1)

    class RetryClock:
        @staticmethod
        def now(zone: timezone) -> datetime:
            assert zone is timezone.utc
            return retry_due

    monkeypatch.setattr(runtime_module, "datetime", RetryClock)
    monkeypatch.setattr(runtime_module, "_write", original)
    rig.restart()
    rig.accept()
    rig.accept()
    assert rig.record()["reply_nonce"] == observed[0]
    assert _read(rig.state / "input.json")["cursor"] == "cursor-one"
    rig.runtime._stage()
    rig.until(lambda: rig.phase() == "awaiting_reply" and rig.ack() == "acked")
    assert len(list((rig.state / "requests").glob("*.json"))) == 1
    assert len(list((rig.state / "queue" / "processed").glob("*.json"))) == 1
    assert len(rig.harness.prompts) == 1
    reactions = rig.transport.calls("react")
    assert len(reactions) == 2
    assert reactions[0]["request_id"] == reactions[1]["request_id"]
    assert [reaction["attempt"] for reaction in reactions] == [1, 2]


def test_echo_before_lost_send_confirmation_is_durable_then_deduplicated(rig: Rig) -> None:
    rig.accept()
    rig.runtime._stage()
    rig.until(lambda: rig.phase() == "awaiting_reply" and rig.ack() == "acked")
    send = rig.transport.gate("send")
    rig.transport.lose_send_once = True
    submit_reply(rig.state, _key(), "Completed")
    rig.runtime._stage()
    assert send.entered.wait(5)
    request_id = str(rig.record()["request_id"])
    with rig.transport.lock:
        reply_id = rig.transport.sent[request_id]
    echo = _message("echo", id=reply_id, text="[fixture-agent] Completed")
    rig.runtime._input_event({"type": "message", "message": echo, "cursor": "cursor-echo"})
    assert len(list(rig.runtime.deferred.glob("*.json"))) == 1
    assert _read(rig.state / "input.json")["cursor"] == "cursor-echo"
    assert len(list((rig.state / "requests").glob("*.json"))) == 1
    send.release.set()
    rig.until(lambda: "reply_error" in rig.record())
    assert rig.phase() == "reply_pending"
    assert len(list(rig.runtime.deferred.glob("*.json"))) == 1
    rig.runtime.retry[("send", _key())] = (0, 1)
    rig.runtime._stage()
    rig.until(lambda: rig.phase() == "replied")
    rig.runtime._stage()
    assert list(rig.runtime.deferred.glob("*.json")) == []
    assert len(list((rig.state / "requests").glob("*.json"))) == 1
    assert len(rig.harness.prompts) == 1
    sends = rig.transport.calls("send")
    assert len(sends) == 2
    assert sends[0]["request_id"] == sends[1]["request_id"] == request_id


@pytest.mark.parametrize("change", ["space", "thread", "cursor", "type"])
def test_invalid_stream_authority_never_advances_cursor_or_queues_input(rig: Rig, change: str) -> None:
    event: dict[str, object] = {"type": "message", "message": _message(), "cursor": "bad-cursor"}
    if change == "space":
        event["message"] = _message(id="spaces/other/messages/one")
    elif change == "thread":
        event["message"] = _message(thread="spaces/other/threads/one")
    elif change == "cursor":
        event["cursor"] = 123
    else:
        event["type"] = "unexpected"
    with pytest.raises((ValueError, TypeError)):
        rig.runtime._input_event(event)
    assert not (rig.state / "input.json").exists()
    assert list((rig.state / "requests").glob("*.json")) == []
    assert rig.transport.requests == []
    assert rig.harness.prompts == []


def test_excluded_sender_advances_safe_checkpoint_without_task_authority(rig: Rig) -> None:
    rig.accept(sender="users/unapproved")
    assert _read(rig.state / "input.json")["cursor"] == "cursor-one"
    assert list((rig.state / "requests").glob("*.json")) == []
    assert rig.transport.requests == []
    assert rig.harness.prompts == []


def test_source_supplied_nonce_and_unknown_output_cannot_authorize_a_reply(rig: Rig) -> None:
    supplied = "X" * 22
    with pytest.raises(ValueError, match="unsupported fields: reply_nonce"):
        rig.accept(reply_nonce=supplied)
    assert list((rig.state / "requests").glob("*.json")) == []
    assert rig.transport.calls("send") == []

    rig.accept()
    rig.runtime._stage()
    rig.until(lambda: rig.phase() == "awaiting_reply" and rig.ack() == "acked")
    assert rig.record()["reply_nonce"] != supplied
    target = rig.bridge.config.target.pane_id
    assert target is not None
    text = f"<GCHAT_REPLY_{supplied}>\nUnrelated output\n</GCHAT_REPLY_{supplied}>"
    rig.runtime._handle(_Notice("output", PaneOutputSnapshot(target, text, False)))
    rig.runtime._stage()
    rig.until(lambda: not rig.runtime.jobs and not rig.runtime.output_pending)
    assert list((rig.state / "replies").glob("*.json")) == []
    assert list((rig.state / "replies" / "items").rglob("*.json")) == []
    assert list((rig.state / "submissions").glob("*.json")) == []
    assert rig.transport.calls("send") == []
    assert rig.phase() == "awaiting_reply"


def _reply_snapshot(rig: Rig, bodies: Sequence[str], *, nonce: str | None = None,
                    legacy: bool = False) -> PaneOutputSnapshot:
    selected = nonce if nonce is not None else str(rig.record()["reply_nonce"])
    marker = "GCHAT_REPLY" if legacy else "CHAT_REPLY"
    target = rig.bridge.config.target.pane_id
    assert target is not None
    text = "\n".join(
        f"<{marker}_{selected}_{ordinal}>\n{body}\n</{marker}_{selected}_{ordinal}>"
        for ordinal, body in enumerate(bodies, start=1)
    )
    return PaneOutputSnapshot(target, text, False)


def _queue_snapshot(rig: Rig, snapshot: PaneOutputSnapshot) -> None:
    rig.runtime._handle(_Notice("output", snapshot))
    rig.runtime._stage()


@pytest.mark.parametrize("legacy", [False, True])
def test_multiple_replies_keep_order_while_more_output_arrives_and_after_restart(
    rig: Rig, legacy: bool,
) -> None:
    rig.accept()
    rig.runtime._stage()
    rig.until(lambda: rig.phase() == "awaiting_reply" and rig.ack() == "acked")
    send = rig.transport.gate("send")
    _queue_snapshot(rig, _reply_snapshot(rig, ["First milestone", "Second milestone"], legacy=legacy))
    rig.until(lambda: ("send", _key()) in rig.runtime.jobs)
    assert send.entered.wait(5)
    assert [call["text"] for call in rig.transport.calls("send")] == ["[fixture-agent] First milestone"]
    full = _reply_snapshot(rig, ["First milestone", "Second milestone", "Completed"], legacy=legacy)
    _queue_snapshot(rig, full)
    rig.until(lambda: not rig.runtime.output_pending and not rig.runtime.output_inflight)
    assert len(rig.transport.calls("send")) == 1
    send.release.set()
    rig.until(lambda: rig.phase() == "replied" and len(rig.transport.calls("send")) == 3)
    sent = rig.transport.calls("send")
    assert [call["text"] for call in sent] == [
        "[fixture-agent] First milestone", "[fixture-agent] Second milestone", "[fixture-agent] Completed",
    ]
    assert len({str(call["request_id"]) for call in sent}) == 3
    assert all(call["thread"] == _THREAD for call in sent)
    assert sent[0]["request_id"] == rig.record()["request_id"]
    rig.restart()
    _queue_snapshot(rig, full)
    rig.until(lambda: not rig.runtime.jobs and not rig.runtime.output_pending)
    assert len(rig.transport.calls("send")) == 3
    assert len(rig.harness.prompts) == 1


def test_later_reply_retry_preserves_identity_and_suppresses_all_reply_echoes(rig: Rig) -> None:
    rig.accept()
    rig.runtime._stage()
    rig.until(lambda: rig.phase() == "awaiting_reply" and rig.ack() == "acked")
    _queue_snapshot(rig, _reply_snapshot(rig, ["First milestone"]))
    rig.until(lambda: rig.phase() == "replied")
    first_id = str(rig.transport.calls("send")[0]["request_id"])
    send = rig.transport.gate("send")
    rig.transport.lose_send_once = True
    _queue_snapshot(rig, _reply_snapshot(rig, ["First milestone", "Second milestone", "Completed"]))
    rig.until(lambda: ("send", _key()) in rig.runtime.jobs)
    assert send.entered.wait(5)
    second_id = str(rig.transport.calls("send")[1]["request_id"])
    assert second_id != first_id
    with rig.transport.lock:
        echo_id = rig.transport.sent[second_id]
    echo = _message("echo", id=echo_id, text="[fixture-agent] Second milestone")
    rig.runtime._input_event({"type": "message", "message": echo, "cursor": "second-reply-echo"})
    assert len(list(rig.runtime.deferred.glob("*.json"))) == 1
    send.release.set()
    rig.until(lambda: "reply_error" in rig.record())
    assert len(rig.transport.calls("send")) == 2
    rig.restart()
    rig.runtime._stage()
    rig.until(lambda: rig.phase() == "replied" and len(rig.transport.calls("send")) == 4)
    sent = rig.transport.calls("send")
    assert [call["text"] for call in sent] == [
        "[fixture-agent] First milestone", "[fixture-agent] Second milestone",
        "[fixture-agent] Second milestone", "[fixture-agent] Completed",
    ]
    assert sent[1]["request_id"] == sent[2]["request_id"] == second_id
    assert len(rig.transport.sent) == 3
    assert list(rig.runtime.deferred.glob("*.json")) == []
    for request_id, body in ((first_id, "First milestone"), (second_id, "Second milestone")):
        with rig.transport.lock:
            identifier = rig.transport.sent[request_id]
        rig.runtime._input_event({"type": "message", "message": _message(
            "echo", id=identifier, text="[fixture-agent] " + body), "cursor": "known-reply-echo"})
    rig.runtime._stage()
    assert len(list((rig.state / "requests").glob("*.json"))) == 1
    assert len(rig.harness.prompts) == 1


def test_restart_recovers_outbox_confirmation_before_request_metadata_commit(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    rig.accept()
    rig.runtime._stage()
    rig.until(lambda: rig.phase() == "awaiting_reply" and rig.ack() == "acked")
    submit_reply(rig.state, _key(), "Completed")

    def crash_aggregate(path: Path, document: dict[str, object]) -> None:
        if path.parent == rig.state / "requests" and document.get("phase") == "replied":
            pending = _read(chat_module._reply_item_path(
                rig.state, _key(), 0, sent=False))
            history = _read(chat_module._reply_item_path(
                rig.state, _key(), 0, sent=True))
            assert pending["reply_id"] == history["reply_id"]
            raise OSError("crash before aggregate reply confirmation")
        _write(path, document)

    monkeypatch.setattr(chat_module, "_write", crash_aggregate)
    rig.runtime._stage()
    with pytest.raises(OSError, match="before aggregate reply confirmation"):
        rig.until(lambda: rig.phase() == "replied")
    assert rig.phase() == "reply_pending"
    assert len(rig.transport.calls("send")) == 1
    monkeypatch.setattr(chat_module, "_write", _write)
    rig.restart()
    rig.runtime._stage()
    assert rig.phase() == "replied"
    assert rig.record()["reply_id"] == next(iter(rig.transport.sent.values()))
    assert rig.record()["replied_at"]
    assert len(rig.transport.calls("send")) == 1
    assert len(rig.harness.prompts) == 1


def test_unknown_reply_feedback_is_durable_and_uses_the_harness_lane(rig: Rig) -> None:
    rig.accept()
    rig.runtime._stage()
    rig.until(lambda: rig.phase() == "awaiting_reply" and rig.ack() == "acked")
    unknown = "X" * 22
    rig.harness.state = "working"
    bad = _reply_snapshot(rig, ["Wrong destination"], nonce=unknown)
    _queue_snapshot(rig, bad)
    rig.until(lambda: not rig.runtime.jobs and not rig.runtime.output_pending)
    assert len(rig.harness.prompts) == 1
    assert rig.transport.calls("send") == []
    rig.restart()
    rig.harness.state = "idle"
    rig.runtime.next_drain = 0
    rig.runtime._stage()
    rig.until(lambda: len(rig.harness.prompts) == 2 and not rig.runtime.jobs)
    feedback = rig.harness.prompts[-1]
    assert unknown in feedback
    assert f'{rig.record()["reply_nonce"]}_1' in feedback
    assert "unavailable" in feedback.lower() or "not available" in feedback.lower()
    assert "CHAT_REPLY" in feedback
    _queue_snapshot(rig, bad)
    rig.until(lambda: not rig.runtime.jobs and not rig.runtime.output_pending)
    assert len(rig.harness.prompts) == 2
    assert rig.transport.calls("send") == []
    assert len(list((rig.state / "requests").glob("*.json"))) == 1


def test_explicit_identical_blocks_are_separate_messages_but_snapshot_replay_is_not(rig: Rig) -> None:
    rig.accept()
    rig.runtime._stage()
    rig.until(lambda: rig.phase() == "awaiting_reply" and rig.ack() == "acked")
    repeated = _reply_snapshot(rig, ["Still working", "Still working"])
    _queue_snapshot(rig, repeated)
    rig.until(lambda: rig.phase() == "replied" and len(rig.transport.calls("send")) == 2)
    sent = rig.transport.calls("send")
    assert sent[0]["text"] == sent[1]["text"] == "[fixture-agent] Still working"
    assert sent[0]["request_id"] != sent[1]["request_id"]
    _queue_snapshot(rig, repeated)
    rig.until(lambda: not rig.runtime.jobs and not rig.runtime.output_pending)
    assert len(rig.transport.calls("send")) == 2


def test_unknown_reply_without_any_request_still_delivers_protocol_feedback(rig: Rig) -> None:
    unknown = "Y" * 22
    _queue_snapshot(rig, _reply_snapshot(rig, ["No destination"], nonce=unknown))
    rig.until(lambda: len(rig.harness.prompts) == 1 and not rig.runtime.jobs)
    assert unknown in rig.harness.prompts[0]
    assert "none" in rig.harness.prompts[0].lower()
    assert rig.transport.calls("send") == []
    assert list((rig.state / "requests").glob("*.json")) == []


def test_output_pump_keeps_one_subscription_for_multiple_events(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    subscriptions: list[tuple[str, ...]] = []
    target = rig.bridge.config.target.pane_id
    assert target is not None
    pane_id = target
    wake = threading.Event()

    class Stream(PaneOutputStream):
        def __init__(self, nonces: Sequence[str]) -> None:
            self.number = len(subscriptions)
            self.reads = 0
            subscriptions.append(tuple(nonces))

        def wait(self, timeout: float) -> tuple[PaneOutputSnapshot | PaneAgentStatus, ...]:
            self.reads += 1
            if self.reads == 1:
                return (PaneAgentStatus(pane_id, "idle"),)
            if self.reads in (2, 3):
                return (PaneOutputSnapshot(
                    pane_id, f"retained-output-{self.reads - 2}", False),)
            wake.wait(timeout)
            return ()

        def wake(self) -> None:
            wake.set()

        def close(self) -> None:
            pass

    def open_output(nonces: Sequence[str]) -> PaneOutputStream:
        return Stream(nonces)

    monkeypatch.setattr(rig.bridge, "_open_cached_output",
                        lambda subscription: open_output(subscription.markers))
    rig.runtime._stage()
    rig.runtime.output.thread.start()
    captured: list[str] = []
    while len(captured) < 2:
        notice = rig.runtime.events.get(timeout=5)
        if isinstance(notice.value, PaneOutputSnapshot):
            captured.append(notice.value.text)
    assert subscriptions == [()]
    assert captured == ["retained-output-0", "retained-output-1"]


def test_output_identity_lookup_cannot_delay_next_message_intake_or_ack(rig: Rig) -> None:
    rig.accept()
    rig.runtime._stage()
    rig.until(lambda: rig.phase() == "awaiting_reply" and rig.ack() == "acked")
    nonce = rig.record()["reply_nonce"]
    target = rig.bridge.config.target.pane_id
    assert target is not None
    snapshot = PaneOutputSnapshot(
        target, f"<GCHAT_REPLY_{nonce}_1>\nFirst answer\n</GCHAT_REPLY_{nonce}_1>", False)
    lookup = rig.harness.lookup = Gate()
    rig.runtime._handle(_Notice("output", snapshot))
    rig.runtime._stage()
    assert lookup.entered.wait(5)
    rig.accept("two", "cursor-two")
    rig.runtime._stage()
    rig.until(lambda: rig.ack("two") == "acked")
    assert not lookup.release.is_set()
    assert not lookup.expired.is_set()
    assert _read(rig.state / "input.json")["cursor"] == "cursor-two"
    assert rig.phase() == "awaiting_reply"
    lookup.release.set()
    rig.until(lambda: rig.phase() == "replied" and rig.phase("two") == "awaiting_reply")
    assert len(rig.harness.prompts) == 2


def test_input_source_reconnects_with_last_durable_cursor(
    rig: Rig, monkeypatch: pytest.MonkeyPatch,
) -> None:
    subscriptions: list[dict[str, object]] = []
    fail_source = threading.Event()
    second_connected = threading.Event()
    sleeping = threading.Event()

    class Stream:
        def __init__(self, command: Sequence[str], request: dict[str, object]) -> None:
            assert tuple(command) == ("fixture-events",)
            self.number = len(subscriptions)
            self.delivered = False
            subscriptions.append(dict(request))
            if self.number == 1:
                second_connected.set()

        def wait(self, timeout: float) -> list[dict[str, object]]:
            if self.number == 0:
                if not self.delivered:
                    self.delivered = True
                    return [{"type": "message", "message": _message(), "cursor": "durable-cursor"}]
                assert fail_source.wait(5)
                raise InputStreamError("stream_eof", "fixture source disconnected")
            sleeping.wait(5)
            return []

        def wake(self) -> None:
            sleeping.set()
            fail_source.set()

        def close(self) -> None:
            pass

    monkeypatch.setattr(runtime_module, "EventCommandStream", Stream)
    monkeypatch.setattr(rig.runtime.stop, "wait", lambda timeout=None: rig.runtime.stop.is_set())
    rig.runtime.input.thread.start()
    connected = rig.runtime.events.get(timeout=5)
    assert connected.kind == "input_connected"
    rig.runtime._handle(connected)
    incoming = rig.runtime.events.get(timeout=5)
    assert incoming.kind == "input"
    rig.runtime._handle(incoming)
    assert _read(rig.state / "input.json")["cursor"] == "durable-cursor"
    fail_source.set()
    error = rig.runtime.events.get(timeout=5)
    while error.kind == "complete":
        rig.runtime._handle(error)
        error = rig.runtime.events.get(timeout=5)
    assert error.kind == "input_error"
    rig.runtime._handle(error)
    assert _read(rig.state / "input.json")["cursor"] == "durable-cursor"
    assert second_connected.wait(5)
    assert subscriptions == [
        {"action": "subscribe", "space": _SPACE, "cursor": None},
        {"action": "subscribe", "space": _SPACE, "cursor": "durable-cursor"},
    ]
    assert len(list((rig.state / "requests").glob("*.json"))) == 1
