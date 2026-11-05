"""Tagged replies through durable Chat intake, output capture, and daemon scheduling."""

from __future__ import annotations

import copy
import json
import re
import uuid
from collections.abc import Callable, Sequence
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest

import agentctl.chat as chat_module
import agentctl.chat_replies as chat_replies_module
from agentctl.agent import _atomic_json, _fsync_dir
from agentctl.chat import Bridge, _OutputObserver, _read, _write, read_reply_history
from agentctl.chat_output import OutputStreamError, PaneAgentStatus, PaneOutputSnapshot, PaneOutputStream
from agentctl.errors import AgentDeliveryError, HerdrUnavailable
from agentctl.jsonx import as_mapping, as_sequence
from tests.test_herdr_chat import Harness, setup


def _records(state: Path) -> dict[str, dict[str, object]]:
    records = [_read(path) for path in (state / "requests").glob("*.json")]
    return {str(as_mapping(record["message"], "message")["id"]).rsplit("/", 1)[-1]: record
            for record in records}


def _block(record: dict[str, object], text: str = "Finished", *, ordinal: int | None = None) -> str:
    nonce = record["reply_nonce"]
    selected = record.get("reply_next_ordinal", 1) if ordinal is None else ordinal
    return f"<CHAT_REPLY_{nonce}_{selected}>\n{text}\n</CHAT_REPLY_{nonce}_{selected}>"


def _snapshot(text: str, *, pane: str = "w1:p1", truncated: bool = False) -> PaneOutputSnapshot:
    return PaneOutputSnapshot(pane, text, truncated, 0)


def test_default_nonce_prompt_is_short_durable_and_not_itself_a_reply(tmp_path: Path) -> None:
    bridge, harness, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _records(tmp_path)["one"]
    nonce = str(record["reply_nonce"])
    assert re.fullmatch(r"[A-Za-z0-9_-]{22}", nonce)
    prompt = harness.prompts[0]
    assert f"`{nonce}_1`" in prompt
    assert f"<CHAT_REPLY_{nonce}_1>" not in prompt and f"</CHAT_REPLY_{nonce}_1>" not in prompt
    assert "one or multiple replies" in prompt and "separate chat message" in prompt
    assert "Google Chat" not in prompt and "GCHAT_REPLY" not in prompt
    assert "PATH_TO_YOUR_REPLY" not in prompt and " --file " not in prompt
    assert "Source: spaces/test/messages/one" in prompt
    assert len(prompt) < 700
    pattern, = chat_module._closing_patterns((f"{nonce}_1",))
    assert not any(re.fullmatch(pattern, line) for line in prompt.splitlines())
    assert bridge.capture_output(_snapshot(prompt)) == {"captured": [], "errors": []}
    assert not chat.sent
    restarted = Bridge(tmp_path, harness, chat)
    restarted.tick()
    assert _records(tmp_path)["one"]["reply_nonce"] == nonce
    assert _records(tmp_path)["one"]["reply_protocol"] == 3
    assert len(harness.prompts) == 1


def test_output_capture_reads_each_queue_artifact_once(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, harness, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _records(tmp_path)["one"]
    queue_reads: list[Path] = []
    original = chat_module._read

    def read(path: Path) -> dict[str, object]:
        if path.parent.name in ("processed", "inflight", "inbox", "failed"):
            queue_reads.append(path)
        return original(path)

    monkeypatch.setattr(chat_module, "_read", read)
    snapshot = _snapshot(harness.prompts[0] + "\n\n" + _block(record, "answer"))
    assert bridge.capture_output(snapshot, deliver=False) == {
        "captured": [record["key"]], "errors": [],
    }
    assert queue_reads == [tmp_path / "queue" / "processed" / f"{record['queue_id']}.json"]


def test_capture_is_durable_threaded_and_deduplicated_after_restart(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, harness, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _records(tmp_path)["one"]
    snapshot = _snapshot(harness.prompts[0] + "\n\n" + _block(record, "The answer 🤖"))
    outcome = bridge.capture_output(snapshot)
    key = str(record["key"])
    assert outcome == {"captured": [key], "errors": []}
    assert _read(chat_module._reply_item_path(tmp_path, key, 0, sent=True))["text"] == "The answer 🤖"
    saved = _records(tmp_path)["one"]
    assert saved["phase"] == "replied"
    assert as_mapping(saved["reply_capture"], "capture")["source"] == "herdr_output"
    sent = next(iter(chat.sent.values()))
    assert sent["thread"] == "spaces/test/threads/one"
    assert sent["text"] == "[test-agent] The answer 🤖"
    restarted = Bridge(tmp_path, harness, chat)
    request_writes: list[Path] = []
    original_write = chat_module._write

    def write(path: Path, document: dict[str, object]) -> None:
        if path.parent == tmp_path / "requests":
            request_writes.append(path)
        original_write(path, document)

    monkeypatch.setattr(chat_module, "_write", write)
    assert restarted.capture_output(snapshot) == {"captured": [], "errors": []}
    assert request_writes == [], "an unchanged retained snapshot must not rewrite request metadata"
    restarted.tick()
    assert len(harness.prompts) == 1
    assert len([call for call in chat.calls if call["action"] == "send"]) == 1
    assert restarted.output_requests() == {str(record["reply_nonce"]): str(record["key"])}


def test_two_identical_blocks_send_two_replies_and_repeated_snapshot_sends_none(tmp_path: Path) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _records(tmp_path)["one"]
    first = _block(record, ordinal=1)
    second = _block(record, ordinal=2)
    snapshot = _snapshot(first + "\n" + second)
    assert not bridge.capture_output(snapshot)["errors"]
    assert len(chat.sent) == 2
    assert bridge.capture_output(snapshot) == {"captured": [], "errors": []}
    assert len(chat.sent) == 2
    assert len([call for call in chat.calls if call["action"] == "send"]) == 2
    saved = _records(tmp_path)["one"]
    assert saved["reply_next_ordinal"] == 3
    assert bridge.output_markers() == (f"{saved['reply_nonce']}_3",)


@pytest.mark.parametrize("ordinals", [(1, 1), (2,)])
def test_v3_capture_refuses_duplicate_or_skipped_ordinals(
    tmp_path: Path, ordinals: tuple[int, ...],
) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _records(tmp_path)["one"]
    text = "\n".join(_block(record, f"answer-{index}", ordinal=ordinal)
                     for index, ordinal in enumerate(ordinals))

    outcome = bridge.capture_output(_snapshot(text))

    error, = as_sequence(outcome["errors"], "capture errors")
    assert "ordinal" in str(as_mapping(error, "capture error")["error"])
    assert not chat.sent
    assert not (tmp_path / "replies" / f"{record['key']}.json").exists()
    assert _records(tmp_path)["one"]["reply_next_ordinal"] == 1


def test_v3_capture_refuses_reused_ordinal_with_changed_text(tmp_path: Path) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _records(tmp_path)["one"]
    assert not bridge.capture_output(_snapshot(_block(record, "first", ordinal=1)))["errors"]

    replay = (_block(record, "changed", ordinal=1) + "\n"
              + _block(record, "second", ordinal=2))
    outcome = bridge.capture_output(_snapshot(replay))

    error, = as_sequence(outcome["errors"], "capture errors")
    assert "reused with different text" in str(as_mapping(error, "capture error")["error"])
    assert len(chat.sent) == 1
    assert _records(tmp_path)["one"]["reply_next_ordinal"] == 2


def test_exact_next_patterns_are_independent_per_request(tmp_path: Path) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message("one")
    chat.message("two")
    bridge.tick()
    markers = bridge.output_markers()
    patterns = chat_module._closing_patterns(markers)

    assert len(markers) == len(patterns) == 2
    for marker in markers:
        closing = f"</CHAT_REPLY_{marker}>"
        assert sum(re.fullmatch(pattern, closing) is not None for pattern in patterns) == 1
        assert not any(re.fullmatch(pattern, f"<CHAT_REPLY_{marker}>") for pattern in patterns)

    records = _records(tmp_path)
    assert not bridge.capture_output(_snapshot(_block(records["one"])), deliver=False)["errors"]
    assert set(bridge.output_markers()) == {
        f'{records["one"]["reply_nonce"]}_2',
        f'{records["two"]["reply_nonce"]}_1',
    }


def test_capture_builds_marker_index_once_for_many_requests(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, _, chat = setup(tmp_path)
    for name in ("one", "two", "three"):
        chat.message(name)
    bridge.tick()
    record = _records(tmp_path)["two"]
    calls = {"sets": 0, "extract": 0}
    original_sets = chat_replies_module.reply_marker_sets
    original_extract = chat_replies_module.extract_sequenced_replies

    def marker_sets(text: str) -> tuple[list[str], list[str]]:
        calls["sets"] += 1
        return original_sets(text)

    def extract(text: str, nonces: Sequence[str], *,
                marker_ids: Sequence[str] | None = None) -> dict[str, list[tuple[int, str]]]:
        calls["extract"] += 1
        return original_extract(text, nonces, marker_ids=marker_ids)

    monkeypatch.setattr(chat_module, "reply_marker_sets", marker_sets)
    monkeypatch.setattr(chat_module, "extract_sequenced_replies", extract)
    monkeypatch.setattr(chat_module, "reply_marker_ids",
                        lambda text: (_ for _ in ()).throw(AssertionError("second marker scan")))

    assert bridge.capture_output(_snapshot(_block(record)), deliver=False)["captured"] == [record["key"]]
    assert calls == {"sets": 1, "extract": 1}


def test_snapshot_from_wrong_pane_is_refused_before_transport(tmp_path: Path) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    before = len(chat.calls)
    with pytest.raises(HerdrUnavailable, match="different coordinator pane"):
        bridge.capture_output(_snapshot(_block(_records(tmp_path)["one"]), pane="w9:p1"))
    assert len(chat.calls) == before
    assert not list((tmp_path / "replies").glob("*.json"))
    assert not list((tmp_path / "replies" / "items").rglob("*.json"))
    assert not list((tmp_path / "submissions").glob("*.json"))


def test_unknown_request_marker_does_not_complete_current_request(tmp_path: Path) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    text = "<GCHAT_REPLY_XXXXXXXXXXXXXXXXXXXXXX>\nWrong request\n</GCHAT_REPLY_XXXXXXXXXXXXXXXXXXXXXX>"
    assert bridge.capture_output(_snapshot(text)) == {"captured": [], "errors": []}
    assert not chat.sent
    assert _records(tmp_path)["one"]["phase"] == "awaiting_reply"


@pytest.mark.parametrize(
    ("limit_name", "expected"),
    [
        ("_MAX_FEEDBACK_RECORDS", "feedback record limit 0"),
        ("_MAX_FEEDBACK_BYTES", "feedback byte limit 0"),
    ],
)
def test_feedback_population_caps_refuse_without_partial_artifacts(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, limit_name: str, expected: str,
) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    monkeypatch.setattr(chat_module, limit_name, 0)
    unknown = "X" * 22
    snapshot = _snapshot(
        f"<CHAT_REPLY_{unknown}_1>\nwrong destination\n</CHAT_REPLY_{unknown}_1>")
    with pytest.raises(ValueError, match=expected):
        bridge.capture_output(snapshot, deliver=False)
    assert list((tmp_path / "feedback").glob("*.json")) == []
    error = _read(tmp_path / "population-limit.json")["error"]
    assert isinstance(error, str) and error.startswith(expected)


@pytest.mark.parametrize(
    ("limit", "expected", "document_count", "path_checks"),
    [
        ("records", "feedback record limit 1024", 1024, 1025),
        ("bytes", "feedback byte limit 1", 1, 1),
    ],
)
def test_feedback_planning_stops_at_first_cap_proving_invalid_marker(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, limit: str,
    expected: str, document_count: int, path_checks: int,
) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    records = list(_records(tmp_path).values())
    available = bridge._output_requests(records, retry_failed=True)
    invalid = [f"invalid-{index}" for index in range(2048)]
    document_calls = 0
    feedback_path_checks = 0
    original_document_bytes = Bridge._document_bytes
    original_exists = Path.exists

    def count_document(document: dict[str, object], label: str) -> int:
        nonlocal document_calls
        document_calls += 1
        return original_document_bytes(document, label)

    def count_exists(path: Path) -> bool:
        nonlocal feedback_path_checks
        if path.parent == tmp_path / "feedback":
            feedback_path_checks += 1
        return original_exists(path)

    monkeypatch.setattr(Bridge, "_document_bytes", staticmethod(count_document))
    monkeypatch.setattr(Path, "exists", count_exists)
    if limit == "bytes":
        monkeypatch.setattr(chat_module, "_MAX_FEEDBACK_BYTES", 1)

    with pytest.raises(ValueError, match=expected):
        bridge._unknown_reply_feedback(
            "", available, records, invalid, inspect_legacy_history=False)

    assert document_calls == document_count
    assert feedback_path_checks == path_checks
    assert list((tmp_path / "feedback").glob("*.json")) == []


@pytest.mark.parametrize("after_rename", [False, True])
@pytest.mark.parametrize("cap", ["records", "bytes"])
def test_feedback_partial_write_repairs_population_before_retry_and_new_admission(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, after_rename: bool, cap: str,
) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    snapshot = _snapshot("\n".join(
        f"<CHAT_REPLY_{letter * 22}_1>\nwrong destination\n</CHAT_REPLY_{letter * 22}_1>"
        for letter in ("X", "Y")))
    original_write = chat_module._write
    writes = 0

    def fail_second_write(path: Path, document: dict[str, object]) -> None:
        nonlocal writes
        if path.parent == tmp_path / "feedback":
            writes += 1
            if writes == 2:
                if after_rename:
                    original_write(path, document)
                raise OSError("injected feedback persistence failure")
        original_write(path, document)

    with monkeypatch.context() as failing:
        failing.setattr(chat_module, "_write", fail_second_write)
        with pytest.raises(OSError, match="feedback persistence failure"):
            bridge.capture_output(snapshot, deliver=False)
    retained = {path: path.read_bytes() for path in (tmp_path / "feedback").glob("*.json")}
    assert len(retained) == (2 if after_rename else 1)
    invalidated = bridge._aux_usage is None
    assert invalidated

    bridge.capture_output(snapshot, deliver=False)
    assert all(path.read_bytes() == content for path, content in retained.items())
    assert bridge._aux_usage is not None
    cached = dict(bridge._aux_usage)
    rebuilt = Bridge(tmp_path, bridge.client, bridge.transport).validate_aux_population()
    assert cached["feedback_records"] == rebuilt["feedback_records"] == 2
    assert cached["feedback_bytes"] == rebuilt["feedback_bytes"]
    assert cached["feedback_bytes"] == sum(
        bridge._document_bytes(_read(path), "reply feedback")
        for path in (tmp_path / "feedback").glob("*.json"))
    if cap == "records":
        monkeypatch.setattr(chat_module, "_MAX_FEEDBACK_RECORDS", 2)
        expected = "feedback record limit 2"
    else:
        monkeypatch.setattr(chat_module, "_MAX_FEEDBACK_BYTES", cached["feedback_bytes"])
        expected = "feedback byte limit"
    final = {path: path.read_bytes() for path in (tmp_path / "feedback").glob("*.json")}
    next_snapshot = _snapshot(
        f"<CHAT_REPLY_{'Z' * 22}_1>\nwrong destination\n</CHAT_REPLY_{'Z' * 22}_1>")
    with pytest.raises(ValueError, match=expected):
        bridge.capture_output(next_snapshot, deliver=False)
    assert {path: path.read_bytes() for path in (tmp_path / "feedback").glob("*.json")} == final


def test_existing_feedback_and_queue_over_cap_states_refuse_without_deletion(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    feedback_id = "f" * 64
    feedback = tmp_path / "feedback" / f"{feedback_id}.json"
    _write(feedback, {
        "queue_id": f"feedback-{feedback_id}", "text": "retained",
        "created_at": "2026-01-02T00:00:00Z"})
    retained_feedback = feedback.read_bytes()
    monkeypatch.setattr(chat_module, "_MAX_FEEDBACK_RECORDS", 0)
    with pytest.raises(ValueError, match="feedback record limit 0"):
        bridge.validate_aux_population()
    assert feedback.read_bytes() == retained_feedback

    monkeypatch.setattr(chat_module, "_MAX_FEEDBACK_RECORDS", 1024)
    monkeypatch.setattr(chat_module, "_MAX_QUEUE_BYTES", 0)
    queue_artifacts = list((tmp_path / "queue").glob("*/*.json"))
    assert queue_artifacts
    retained_queue = {path: path.read_bytes() for path in queue_artifacts}
    with pytest.raises(ValueError, match="queue artifact byte limit 0"):
        bridge.validate_aux_population()
    assert {path: path.read_bytes() for path in queue_artifacts} == retained_queue


def test_lost_transport_ack_retries_artifact_without_reprompt_or_recapture(tmp_path: Path) -> None:
    bridge, harness, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _records(tmp_path)["one"]
    chat.lose_ack = True
    with pytest.raises(OSError, match="acknowledgement lost"):
        bridge.capture_output(_snapshot(_block(record)))
    assert _records(tmp_path)["one"]["phase"] == "reply_pending"
    assert bridge.output_requests() == {str(record["reply_nonce"]): str(record["key"])}
    restarted = Bridge(tmp_path, harness, chat)
    restarted.tick()
    sends = [call for call in chat.calls if call["action"] == "send"]
    assert len(sends) == 2 and sends[0]["request_id"] == sends[1]["request_id"]
    assert len(chat.sent) == 1 and len(harness.prompts) == 1
    assert _records(tmp_path)["one"]["phase"] == "replied"


def test_crash_after_reply_artifact_before_capture_metadata_recovers(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, harness, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _records(tmp_path)["one"]
    original = chat_module._write

    def interrupted_write(path: Path, document: dict[str, object]) -> None:
        if "reply_capture" in document:
            raise OSError("simulated crash after reply fsync")
        original(path, document)

    monkeypatch.setattr(chat_module, "_write", interrupted_write)
    with pytest.raises(OSError, match="simulated crash"):
        bridge.capture_output(_snapshot(_block(record)))
    monkeypatch.setattr(chat_module, "_write", original)
    assert not chat.sent
    assert _read(chat_module._reply_item_path(
        tmp_path, str(record["key"]), 0, sent=False))["text"] == "Finished"
    restarted = Bridge(tmp_path, harness, chat)
    assert restarted.output_requests() == {str(record["reply_nonce"]): str(record["key"])}
    restarted.tick()
    assert len(chat.sent) == 1 and len(harness.prompts) == 1
    recovered = _records(tmp_path)["one"]
    assert recovered["reply_next_ordinal"] == 2
    assert restarted.output_markers() == (f"{record['reply_nonce']}_2",)


def test_tagged_capture_confirms_uncertain_prompt_delivery(tmp_path: Path) -> None:
    bridge, harness, chat = setup(tmp_path)
    harness.fail_confirmation = True
    chat.message()
    bridge.tick()
    record = _records(tmp_path)["one"]
    assert record["phase"] == "delivery_uncertain"
    bridge.capture_output(_snapshot(_block(record)))
    saved = _records(tmp_path)["one"]
    assert saved["phase"] == "replied"
    assert saved["delivery_confirmed_by"] == "reply_artifact"
    assert len(harness.prompts) == 1


@pytest.mark.parametrize("kind", ["clipped", "fenced", "empty", "mismatched", "oversize"])
def test_bad_capture_is_visible_and_does_not_starve_another_request(tmp_path: Path, kind: str) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message("broken")
    chat.message("valid")
    bridge.tick()
    records = _records(tmp_path)
    broken, valid = records["broken"], records["valid"]
    nonce = broken["reply_nonce"]
    variants = {
        "clipped": f"Reply beginning disappeared\n</GCHAT_REPLY_{nonce}>",
        "fenced": "```text\n" + _block(broken) + "\n```",
        "empty": _block(broken, "  "),
        "mismatched": f"<GCHAT_REPLY_{nonce}>\nWrong close\n</GCHAT_REPLY_XXXXXXXXXXXXXXXXXXXXXX>\n</GCHAT_REPLY_{nonce}>",
        "oversize": _block(broken, "x" * 30001),
    }
    result = bridge.capture_output(_snapshot(variants[kind] + "\n" + _block(valid, "Valid answer"), truncated=True))
    assert result["captured"] == [valid["key"]]
    assert len(as_sequence(result["errors"], "capture errors")) == 1
    saved = _records(tmp_path)
    assert saved["broken"]["capture_error"]
    assert saved["broken"]["phase"] == "awaiting_reply"
    assert saved["valid"]["phase"] == "replied"
    assert str(broken["reply_nonce"]) not in bridge.output_requests()
    assert str(broken["reply_nonce"]) in bridge.output_requests(retry_failed=True)
    assert next(iter(chat.sent.values()))["thread"] == "spaces/test/threads/valid"
    status = bridge.status()
    assert any(as_mapping(item, "request").get("capture_error")
               for item in as_sequence(status["requests"], "requests"))


def test_truncated_scrollback_with_a_complete_reply_is_safe(tmp_path: Path) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _records(tmp_path)["one"]
    result = bridge.capture_output(_snapshot(_block(record), truncated=True))
    assert result["captured"] == [record["key"]]
    assert as_mapping(_records(tmp_path)["one"]["reply_capture"], "capture")["snapshot_truncated"] is True


class _FakeStream(PaneOutputStream):
    def __init__(self, wait: Callable[[float], tuple[PaneOutputSnapshot | PaneAgentStatus, ...]]) -> None:
        self.wait_impl = wait
        self.waits: list[float] = []
        self.closes = 0

    def wait(self, timeout: float) -> tuple[PaneOutputSnapshot | PaneAgentStatus, ...]:
        self.waits.append(timeout)
        return self.wait_impl(timeout)

    def close(self) -> None:
        self.closes += 1


def test_capture_once_explicitly_retries_failed_request_and_closes_stream(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _records(tmp_path)["one"]
    bridge.capture_output(_snapshot(f"</GCHAT_REPLY_{record['reply_nonce']}>"))
    assert not bridge.output_requests()
    stream = _FakeStream(lambda timeout: (_snapshot(_block(record, "Recovered")),))
    requested: list[tuple[str, ...]] = []

    def open_output(nonces: Sequence[str]) -> PaneOutputStream:
        requested.append(tuple(nonces))
        return stream

    monkeypatch.setattr(bridge, "open_output", open_output)
    result = bridge.capture_once()
    assert result["captured"] == [record["key"]]
    assert requested == [(f"{record['reply_nonce']}_1",)]
    assert stream.waits == [0.25] and stream.closes == 1
    assert "capture_error" not in _records(tmp_path)["one"]


def test_capture_once_closes_stream_when_observer_fails(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()

    def fail(timeout: float) -> tuple[PaneOutputSnapshot, ...]:
        raise OutputStreamError("stream_eof", "gone")

    stream = _FakeStream(fail)
    monkeypatch.setattr(bridge, "open_output", lambda nonces: stream)
    with pytest.raises(OutputStreamError, match="stream_eof"):
        bridge.capture_once()
    assert stream.closes == 1


@pytest.mark.parametrize("protocol", [2, None], ids=("v2", "pre-versioned"))
def test_continuous_run_refuses_legacy_before_external_access_and_cleans_stale_monolith(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, protocol: int | None,
) -> None:
    bridge, harness, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _records(tmp_path)["one"]
    path = tmp_path / "requests" / f"{record['key']}.json"
    if protocol is None:
        record.pop("reply_protocol", None)
    else:
        record["reply_protocol"] = protocol
    record.pop("reply_next_ordinal", None)
    chat_module._write(path, record)
    reply_path = tmp_path / "replies" / f"{record['key']}.json"
    chat_module._write(reply_path, {"text": "durable legacy history"})

    monkeypatch.setattr(harness, "pane_info",
                        lambda pane: (_ for _ in ()).throw(AssertionError("external Herdr access")))
    monkeypatch.setattr(chat_module, "_run_polling",
                        lambda *args, **kwargs: (_ for _ in ()).throw(AssertionError("runner opened")))
    with pytest.raises(ValueError, match="protocol-v2"):
        chat_module._run_bridge(bridge, 60, "test-chat")

    assert chat_module.close_replies(tmp_path, str(record["key"])[:12]) == record["key"]
    assert not reply_path.exists()
    history = read_reply_history(tmp_path, str(record["key"])[:12])
    assert [as_mapping(item, "history item")["text"]
            for item in as_sequence(history["items"], "history")] == []
    closed = _records(tmp_path)["one"]
    assert closed["reply_closed_at"] and closed["reply_close_reason"] == "operator"
    assert bridge.output_markers() == ()
    bridge.validate_continuous_output()
    status_document = bridge.status()
    assert status_document["reply_subscriptions"] == {"active": 0, "limit": 128}
    status_record, = as_sequence(status_document["requests"], "requests")
    assert as_mapping(status_record, "request")["reply_closed_at"] == closed["reply_closed_at"]


def test_reply_storage_migration_is_marker_last_idempotent_and_recovery_skips_history(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, harness, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _records(tmp_path)["one"]
    key = str(record["key"])
    request_path = tmp_path / "requests" / f"{key}.json"
    for field in ("reply_storage", "reply_item_count", "reply_sent_count",
                  "reply_ordinal_offset", "reply_total_bytes", "reply_pending_bytes"):
        record.pop(field, None)
    record.update(phase="reply_pending", reply_next_ordinal=3)
    first_id = str(record["request_id"])
    second_id = str(uuid.uuid5(uuid.UUID(first_id), "reply:1"))
    legacy: dict[str, object] = {
        "text": "first",
        "items": [
            {"reply_key": "0", "request_id": first_id, "reply_ordinal": 1,
             "text": "first", "reply_id": "spaces/test/messages/first",
             "replied_at": "2026-01-02T00:01:00Z"},
            {"reply_key": "1", "request_id": second_id, "reply_ordinal": 2,
             "text": "second"},
        ],
    }
    _write(request_path, record)
    embedded = tmp_path / "replies" / f"{key}.json"
    _write(embedded, legacy)
    legacy_bytes = embedded.read_bytes()

    migrated = Bridge(tmp_path, harness, chat)
    original_write = chat_module._write
    interrupted = False

    def fail_marker(path: Path, document: dict[str, object]) -> None:
        nonlocal interrupted
        if path == request_path and document.get("reply_storage") == 2 and not interrupted:
            interrupted = True
            raise OSError("crash before storage marker")
        original_write(path, document)

    monkeypatch.setattr(chat_module, "_write", fail_marker)
    with pytest.raises(OSError, match="before storage marker"):
        migrated.migrate_reply_outboxes()
    assert "reply_storage" not in _read(request_path)
    assert embedded.read_bytes() == legacy_bytes

    monkeypatch.setattr(chat_module, "_write", original_write)
    migrated.migrate_reply_outboxes()
    saved = _read(request_path)
    assert (saved["reply_storage"], saved["reply_item_count"], saved["reply_sent_count"]) == (2, 2, 1)
    assert (saved["reply_total_bytes"], saved["reply_pending_bytes"],
            saved["reply_next_ordinal"]) == (11, 6, 3)
    assert (saved["reply_last_id"], saved["reply_last_at"]) == (
        "spaces/test/messages/first", "2026-01-02T00:01:00Z")
    history = _read(chat_module._reply_item_path(tmp_path, key, 0, sent=True))
    pending = _read(chat_module._reply_item_path(tmp_path, key, 1, sent=False))
    assert history["request_id"] == first_id and history["reply_source"] == "capture"
    assert pending["request_id"] == second_id and pending["reply_source"] == "capture"
    assert not embedded.exists()

    reads: list[Path] = []
    original_read = chat_module._read

    def no_history_read(path: Path) -> dict[str, object]:
        if "history" in path.parts:
            reads.append(path)
            raise AssertionError("fixed recovery traversed immutable history")
        return original_read(path)

    monkeypatch.setattr(chat_module, "_read", no_history_read)
    recovered = migrated._recover_reply_state(request_path, saved)
    assert [item["text"] for item in recovered] == ["second"] and reads == []


def test_legacy_encoded_population_accepts_cap_and_refuses_cap_plus_one_padding(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    template = _records(tmp_path)["one"]
    keys: list[str] = []
    embedded_paths: list[Path] = []
    for index in range(2):
        record = copy.deepcopy(template)
        key = str(record["key"]) if index == 0 else f"{index:064x}"
        identity = uuid.uuid5(uuid.NAMESPACE_URL, f"encoded-legacy-{index}")
        record.update(
            key=key, request_id=str(identity), reply_nonce=f"{index + 1:022d}",
            queue_id=(template["queue_id"] if index == 0 else f"{index:020d}-{key}"),
        )
        for field in ("reply_storage", "reply_item_count", "reply_sent_count",
                      "reply_ordinal_offset", "reply_total_bytes", "reply_pending_bytes"):
            record.pop(field, None)
        _write(tmp_path / "requests" / f"{key}.json", record)
        embedded = tmp_path / "replies" / f"{key}.json"
        _write(embedded, {"text": "x" * 4096, "items": []})
        keys.append(key)
        embedded_paths.append(embedded)

    encoded_total = sum(path.stat().st_size for path in embedded_paths)
    monkeypatch.setattr(chat_module, "_MAX_LEGACY_REPLY_BYTES", encoded_total - 1)
    with pytest.raises(ValueError, match="state-wide encoded byte limit"):
        Bridge(tmp_path).migrate_reply_outboxes()
    assert all(path.exists() for path in embedded_paths)
    assert all("reply_storage" not in _read(
        tmp_path / "requests" / f"{key}.json") for key in keys)

    monkeypatch.setattr(chat_module, "_MAX_LEGACY_REPLY_BYTES", encoded_total)
    Bridge(tmp_path).migrate_reply_outboxes()
    assert all(not path.exists() for path in embedded_paths)
    assert all(_read(tmp_path / "requests" / f"{key}.json")["reply_storage"] == 2
               for key in keys)


def test_legacy_migration_refuses_unknown_top_level_fields(tmp_path: Path) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _records(tmp_path)["one"]
    key = str(record["key"])
    for field in ("reply_storage", "reply_item_count", "reply_sent_count",
                  "reply_ordinal_offset", "reply_total_bytes", "reply_pending_bytes"):
        record.pop(field, None)
    _write(tmp_path / "requests" / f"{key}.json", record)
    embedded = tmp_path / "replies" / f"{key}.json"
    _write(embedded, {"text": "answer", "items": [], "unaccounted_padding": "x"})

    with pytest.raises(ValueError, match="must contain exactly text and items"):
        Bridge(tmp_path).migrate_reply_outboxes()
    assert embedded.exists()
    assert "reply_storage" not in _read(tmp_path / "requests" / f"{key}.json")


def test_legacy_cleanup_unlink_failure_retries_without_remigration(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _records(tmp_path)["one"]
    key = str(record["key"])
    request_path = tmp_path / "requests" / f"{key}.json"
    for field in ("reply_storage", "reply_item_count", "reply_sent_count",
                  "reply_ordinal_offset", "reply_total_bytes", "reply_pending_bytes"):
        record.pop(field, None)
    _write(request_path, record)
    embedded = tmp_path / "replies" / f"{key}.json"
    _write(embedded, {"text": "answer"})

    original_unlink = Path.unlink
    failed = False

    def fail_once(path: Path, *, missing_ok: bool = False) -> None:
        nonlocal failed
        if path == embedded and not failed:
            failed = True
            raise OSError("cleanup unlink failed")
        original_unlink(path, missing_ok=missing_ok)

    monkeypatch.setattr(Path, "unlink", fail_once)
    with pytest.raises(OSError, match="cleanup unlink failed"):
        Bridge(tmp_path).migrate_reply_outboxes()
    assert failed and embedded.exists()
    assert _read(request_path)["reply_storage"] == 2

    def no_remigration(
        self: Bridge, path: Path, saved: dict[str, object],
    ) -> tuple[chat_module._LegacyReplyPlan, list[dict[str, object]]]:
        del self, path, saved
        raise AssertionError("durable current storage must not be remigrated")

    monkeypatch.setattr(Bridge, "_preflight_legacy_outbox", no_remigration)
    Bridge(tmp_path).migrate_reply_outboxes()
    assert not embedded.exists()


def test_legacy_cleanup_fsync_failure_retries_reappeared_entry_without_remigration(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _records(tmp_path)["one"]
    key = str(record["key"])
    request_path = tmp_path / "requests" / f"{key}.json"
    for field in ("reply_storage", "reply_item_count", "reply_sent_count",
                  "reply_ordinal_offset", "reply_total_bytes", "reply_pending_bytes"):
        record.pop(field, None)
    _write(request_path, record)
    embedded = tmp_path / "replies" / f"{key}.json"
    legacy: dict[str, object] = {"text": "answer"}
    _write(embedded, legacy)

    original_fsync = _fsync_dir
    failed = False

    def fail_cleanup_fsync(directory: str) -> None:
        nonlocal failed
        if directory == str(embedded.parent) and not embedded.exists() and not failed:
            failed = True
            raise OSError("cleanup fsync failed")
        original_fsync(directory)

    monkeypatch.setattr(chat_module, "_fsync_dir", fail_cleanup_fsync)
    with pytest.raises(OSError, match="cleanup fsync failed"):
        Bridge(tmp_path).migrate_reply_outboxes()
    assert failed and not embedded.exists()
    assert _read(request_path)["reply_storage"] == 2

    # Model the only crash state made possible by a failed directory fsync:
    # the unlink was visible before the crash but its directory entry returns.
    monkeypatch.setattr(chat_module, "_fsync_dir", original_fsync)
    _write(embedded, legacy)

    def no_remigration(
        self: Bridge, path: Path, saved: dict[str, object],
    ) -> tuple[chat_module._LegacyReplyPlan, list[dict[str, object]]]:
        del self, path, saved
        raise AssertionError("durable current storage must not be remigrated")

    monkeypatch.setattr(Bridge, "_preflight_legacy_outbox", no_remigration)
    Bridge(tmp_path).migrate_reply_outboxes()
    assert not embedded.exists()


def test_over_cap_legacy_migration_creates_zero_sharded_artifacts(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _records(tmp_path)["one"]
    key = str(record["key"])
    for field in ("reply_storage", "reply_item_count", "reply_sent_count",
                  "reply_ordinal_offset", "reply_total_bytes", "reply_pending_bytes"):
        record.pop(field, None)
    _write(tmp_path / "requests" / f"{key}.json", record)
    base = uuid.UUID(str(record["request_id"]))
    _write(tmp_path / "replies" / f"{key}.json", {"text": "one", "items": [
        {"reply_key": "0", "request_id": str(base), "reply_ordinal": 1, "text": "one"},
        {"reply_key": "1", "request_id": str(uuid.uuid5(base, "reply:1")),
         "reply_ordinal": 2, "text": "two"},
    ]})
    monkeypatch.setattr(chat_module, "_MAX_REQUEST_REPLY_ITEMS", 1)
    with pytest.raises(ValueError, match="per-request item limit"):
        Bridge(tmp_path).migrate_reply_outboxes()
    assert not (tmp_path / "replies" / "items" / key[:2] / key[2:4] / key).exists()
    assert "reply_storage" not in _read(tmp_path / "requests" / f"{key}.json")


def test_combined_legacy_population_is_preflighted_before_any_expansion(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    template = _records(tmp_path)["one"]
    keys: list[str] = []
    for index in range(2):
        record = copy.deepcopy(template)
        key = str(record["key"]) if index == 0 else f"{index:064x}"
        identity = uuid.uuid5(uuid.NAMESPACE_URL, f"global-preflight-{index}")
        record.update(key=key, request_id=str(identity), reply_nonce=f"{index + 1:022d}",
                      queue_id=(template["queue_id"] if index == 0
                                else f"{index:020d}-{key}"))
        for field in ("reply_storage", "reply_item_count", "reply_sent_count",
                      "reply_ordinal_offset", "reply_total_bytes", "reply_pending_bytes"):
            record.pop(field, None)
        _write(tmp_path / "requests" / f"{key}.json", record)
        _write(tmp_path / "replies" / f"{key}.json", {"text": "x", "items": [{
            "reply_key": "0", "request_id": str(identity), "reply_ordinal": 1,
            "text": "x",
        }]})
        keys.append(key)
    monkeypatch.setattr(chat_module, "_MAX_STATE_REPLY_ITEMS", 1)
    with pytest.raises(ValueError, match="state-wide storage limits"):
        Bridge(tmp_path).migrate_reply_outboxes()
    assert not (tmp_path / "replies" / "items").exists()
    assert all("reply_storage" not in _read(
        tmp_path / "requests" / f"{key}.json") for key in keys)


@pytest.mark.parametrize("malformation", ["misplaced-unsequenced", "unknown-large-field"])
def test_malformed_legacy_migration_creates_zero_sharded_artifacts(
    tmp_path: Path, malformation: str,
) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _records(tmp_path)["one"]
    key = str(record["key"])
    for field in ("reply_storage", "reply_item_count", "reply_sent_count",
                  "reply_ordinal_offset", "reply_total_bytes", "reply_pending_bytes"):
        record.pop(field, None)
    _write(tmp_path / "requests" / f"{key}.json", record)
    base = uuid.UUID(str(record["request_id"]))
    first: dict[str, object] = {
        "reply_key": "0", "request_id": str(base), "reply_ordinal": 1, "text": "one"}
    second: dict[str, object] = {
        "reply_key": "1", "request_id": str(uuid.uuid5(base, "reply:1")), "text": "two"}
    if malformation == "unknown-large-field":
        second["reply_ordinal"] = 2
        second["unaccounted_body"] = "x" * (2 * 1024 * 1024)
    # Install historical malformed bytes without the future-write staging cap.
    _atomic_json(str(tmp_path / "replies" / f"{key}.json"), {
        "text": "one", "items": [first, second]})

    expected = ("first and only" if malformation == "misplaced-unsequenced"
                else "unsupported fields")
    with pytest.raises(ValueError, match=expected):
        Bridge(tmp_path).migrate_reply_outboxes()
    assert not (tmp_path / "replies" / "items" / key[:2] / key[2:4] / key).exists()
    assert "reply_storage" not in _read(tmp_path / "requests" / f"{key}.json")


def test_reply_artifact_paths_shard_request_keys_and_sequence_buckets(tmp_path: Path) -> None:
    key = "abcd" + "0" * 60
    first = chat_module._reply_item_path(tmp_path, key, 0, sent=False)
    last_in_bucket = chat_module._reply_item_path(tmp_path, key, 999, sent=False)
    next_bucket = chat_module._reply_item_path(tmp_path, key, 1_000, sent=False)
    assert first.parts[-6:-3] == ("ab", "cd", key)
    assert first.parent == last_in_bucket.parent
    assert first.parent.name == "0000" and next_bucket.parent.name == "0001"


def test_reply_artifact_creation_refuses_symlinked_request_shard(tmp_path: Path) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _records(tmp_path)["one"]
    key = str(record["key"])
    outside = tmp_path / "outside"
    outside.mkdir()
    items = tmp_path / "replies" / "items"
    items.mkdir(mode=0o700)
    (items / key[:2]).symlink_to(outside, target_is_directory=True)
    with pytest.raises(AgentDeliveryError, match="reply artifact directory"):
        bridge.capture_output(_snapshot(_block(record)), deliver=False)
    assert list(outside.iterdir()) == []


def test_migration_releases_each_first_pass_body_population_before_expansion(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    template = _records(tmp_path)["one"]
    paths: list[Path] = []
    for index in range(2):
        record = copy.deepcopy(template)
        key = str(record["key"]) if index == 0 else f"{index:064x}"
        identity = uuid.uuid5(uuid.NAMESPACE_URL, f"memory-shape-{index}")
        record.update(key=key, request_id=str(identity), reply_nonce=f"{index + 1:022d}",
                      queue_id=(template["queue_id"] if index == 0
                                else f"{index:020d}-{key}"))
        for field in ("reply_storage", "reply_item_count", "reply_sent_count",
                      "reply_ordinal_offset", "reply_total_bytes", "reply_pending_bytes"):
            record.pop(field, None)
        path = tmp_path / "requests" / f"{key}.json"
        paths.append(path)
        _write(path, record)
        _write(tmp_path / "replies" / f"{key}.json", {"text": "x" * 1024, "items": [{
            "reply_key": "0", "request_id": str(identity), "reply_ordinal": 1,
            "text": "x" * 1024,
        }]})

    released: list[Path] = []

    class TrackedItems(list[dict[str, object]]):
        def __init__(self, label: Path, values: list[dict[str, object]]) -> None:
            super().__init__(values)
            self.label = label

        def __del__(self) -> None:
            released.append(self.label)

    original_preflight = Bridge._preflight_legacy_outbox

    def track_preflight(
        self: Bridge, path: Path, record: dict[str, object],
    ) -> tuple[chat_module._LegacyReplyPlan, list[dict[str, object]]]:
        plan, items = original_preflight(self, path, record)
        return plan, TrackedItems(path, items)

    original_create = Bridge._create_reply_item
    observed = False

    def assert_first_pass_released(
        self: Bridge, record: dict[str, object], index: int, item: dict[str, object],
        *, sent: bool = False,
    ) -> tuple[dict[str, object], bool]:
        nonlocal observed
        if not observed:
            observed = True
            assert all(path in released for path in paths)
        return original_create(self, record, index, item, sent=sent)

    monkeypatch.setattr(Bridge, "_preflight_legacy_outbox", track_preflight)
    monkeypatch.setattr(Bridge, "_create_reply_item", assert_first_pass_released)
    Bridge(tmp_path).migrate_reply_outboxes()
    assert observed


def test_large_history_summary_capture_and_status_do_not_read_history(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _records(tmp_path)["one"]
    key = str(record["key"])
    path = tmp_path / "requests" / f"{key}.json"
    record.update(reply_item_count=1_000, reply_sent_count=1_000,
                  reply_total_bytes=1_000, reply_pending_bytes=0,
                  reply_next_ordinal=1_001, reply_last_id="spaces/test/messages/old")
    _write(path, record)
    bridge._rebuild_reply_usage([record])
    history_reads: list[Path] = []
    original_read = chat_module._read

    def count_reads(candidate: Path) -> dict[str, object]:
        if "history" in candidate.parts:
            history_reads.append(candidate)
        return original_read(candidate)

    monkeypatch.setattr(chat_module, "_read", count_reads)
    snapshot = _snapshot(_block(record, "new", ordinal=1_001))
    assert bridge.capture_output(snapshot, deliver=False)["errors"] == []
    assert bridge.status()["reply_storage"]
    assert history_reads == []
    saved = _read(path)
    assert saved["reply_item_count"] == 1_001 and saved["reply_pending_bytes"] == 3
    assert _read(chat_module._reply_item_path(tmp_path, key, 1_000, sent=False))["text"] == "new"


def test_explicit_history_reads_bodies_while_default_status_reads_only_summaries(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _records(tmp_path)["one"]
    bridge.capture_output(_snapshot(_block(record, "retained body")))
    history_reads: list[Path] = []
    original_read = chat_module._read

    def observe(path: Path) -> dict[str, object]:
        if "history" in path.parts:
            history_reads.append(path)
        return original_read(path)

    monkeypatch.setattr(chat_module, "_read", observe)
    status = bridge.status()
    assert history_reads == []
    storage = as_mapping(status["reply_storage"], "reply storage")
    assert as_mapping(storage["usage"], "usage")["items"] == 1
    audit = read_reply_history(tmp_path, str(record["key"])[:12])
    assert [as_mapping(item, "history item")["text"]
            for item in as_sequence(audit["items"], "history")] == ["retained body"]
    assert len(history_reads) == 1


def test_dynamic_129th_capture_error_refuses_before_target_access(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, harness, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    template = _records(tmp_path)["one"]
    for index in range(1, 128):
        clone = copy.deepcopy(template)
        key = f"{index:064x}"
        clone.update(key=key, reply_nonce=f"{index:022d}", phase="awaiting_reply",
                     queue_id=f"{index:020d}-{key}")
        chat_module._write(tmp_path / "requests" / f"{key}.json", clone)
    assert len(bridge.output_markers(retry_failed=True)) == 128

    clone = copy.deepcopy(template)
    key = f"{128:064x}"
    clone.update(key=key, reply_nonce=f"{128:022d}", phase="awaiting_reply",
                 queue_id=f"{128:020d}-{key}",
                 capture_error="retained block is malformed")
    chat_module._write(tmp_path / "requests" / f"{key}.json", clone)
    monkeypatch.setattr(
        harness, "pane_info",
        lambda pane: (_ for _ in ()).throw(AssertionError("target accessed before refusal")))
    with pytest.raises(ValueError, match="129 active requests"):
        bridge.open_output(())


def test_storage_limit_closes_capture_but_delivers_existing_pending(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(chat_module, "_MAX_REQUEST_REPLY_ITEMS", 1)
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _records(tmp_path)["one"]
    first = _snapshot(_block(record, "first", ordinal=1))
    assert bridge.capture_output(first, deliver=False)["errors"] == []
    second = _snapshot(first.text + "\n" + _block(record, "second", ordinal=2))
    outcome = bridge.capture_output(second, deliver=False)
    assert as_sequence(outcome["errors"], "errors")
    closed = _records(tmp_path)["one"]
    assert closed["reply_close_reason"] == "storage_limit"
    assert "per-request reply count limit 1" in str(closed["reply_storage_error"])
    assert closed["reply_item_count"] == 1 and closed["reply_sent_count"] == 0
    bridge._deliver()
    assert [call["text"] for call in chat.calls if call["action"] == "send"] == [
        "[test-agent] first"]


@pytest.mark.parametrize(
    ("limit_name", "limit", "first_text", "expected"),
    [
        ("_MAX_STATE_REPLY_ITEMS", 1, "first", "state-wide reply count limit 1"),
        ("_MAX_PENDING_REPLY_ITEMS", 1, "first", "state-wide pending reply count limit 1"),
        ("_MAX_STATE_REPLY_BYTES", 5, "12345", "state-wide reply byte limit 5"),
        ("_MAX_PENDING_REPLY_BYTES", 5, "12345", "state-wide pending reply byte limit 5"),
    ],
)
def test_state_cap_accepts_exact_boundary_and_closes_plus_one_capture(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, limit_name: str,
    limit: int, first_text: str, expected: str,
) -> None:
    monkeypatch.setattr(chat_module, limit_name, limit)
    bridge, _, chat = setup(tmp_path)
    chat.message("one")
    chat.message("two", timestamp="2026-01-02T00:01:00Z")
    bridge.tick()
    records = _records(tmp_path)
    assert bridge.capture_output(
        _snapshot(_block(records["one"], first_text)), deliver=False)["errors"] == []
    outcome = bridge.capture_output(
        _snapshot(_block(records["two"], "x")), deliver=False)
    assert as_sequence(outcome["errors"], "errors")
    closed = _records(tmp_path)["two"]
    assert closed["reply_close_reason"] == "storage_limit"
    assert expected in str(closed["reply_storage_error"])
    assert closed["reply_item_count"] == 0


def test_over_cap_file_submission_is_visibly_rejected_and_unlinked_once(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _records(tmp_path)["one"]
    key = str(record["key"])
    submission = tmp_path / "submissions" / f"{key}.json"
    _write(submission, {"request": key, "text": "cannot fit"})
    monkeypatch.setattr(chat_module, "_MAX_PENDING_REPLY_ITEMS", 0)
    path = tmp_path / "requests" / f"{key}.json"
    pending: list[dict[str, object]] = []
    assert bridge._adopt_submission(path, record, pending) is False
    assert not submission.exists() and pending == []
    rejected = _read(path)
    assert rejected["reply_close_reason"] == "storage_limit"
    assert rejected["reply_submission_bytes"] == len("cannot fit")
    assert "pending reply count limit 0" in str(rejected["reply_submission_error"])

    writes: list[Path] = []
    original_write = chat_module._write

    def observe(candidate: Path, document: dict[str, object]) -> None:
        if candidate == path:
            writes.append(candidate)
        original_write(candidate, document)

    monkeypatch.setattr(chat_module, "_write", observe)
    assert bridge._adopt_submission(path, rejected, pending) is False
    assert writes == []


def test_rejected_submission_crash_after_request_commit_finishes_unlink_on_recovery(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _records(tmp_path)["one"]
    key = str(record["key"])
    request_path = tmp_path / "requests" / f"{key}.json"
    submission = tmp_path / "submissions" / f"{key}.json"
    _write(submission, {"request": key, "text": "cannot fit"})
    monkeypatch.setattr(chat_module, "_MAX_PENDING_REPLY_ITEMS", 0)
    original_unlink = Path.unlink
    interrupted = False

    def crash_unlink(path: Path, missing_ok: bool = False) -> None:
        nonlocal interrupted
        if path == submission and not interrupted:
            interrupted = True
            raise OSError("crash after rejection commit")
        original_unlink(path, missing_ok=missing_ok)

    monkeypatch.setattr(Path, "unlink", crash_unlink)
    with pytest.raises(OSError, match="after rejection commit"):
        bridge._adopt_submission(request_path, record, [])
    assert submission.exists()
    committed = _read(request_path)
    assert committed["reply_submission_digest"] and committed["reply_close_reason"] == "storage_limit"

    monkeypatch.setattr(Path, "unlink", original_unlink)
    assert Bridge(tmp_path)._adopt_submission(request_path, committed, []) is False
    assert not submission.exists()


def test_final_protocol_ordinal_closes_without_materializing_prior_history(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(chat_module, "_MAX_REQUEST_REPLY_ITEMS", 1_000_000)
    monkeypatch.setattr(chat_module, "_MAX_STATE_REPLY_ITEMS", 1_000_000)
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _records(tmp_path)["one"]
    path = tmp_path / "requests" / f"{record['key']}.json"
    record.update(reply_item_count=999_998, reply_sent_count=999_998,
                  reply_total_bytes=0, reply_pending_bytes=0,
                  reply_next_ordinal=999_999)
    _write(path, record)
    bridge._rebuild_reply_usage([record])
    assert bridge._append_sequenced_replies(record, [(999_999, "last")], [])
    saved = _read(path)
    assert saved["reply_next_ordinal"] == 1_000_000
    assert saved["reply_close_reason"] == "ordinal_exhausted"
    assert _read(chat_module._reply_item_path(
        tmp_path, str(record["key"]), 999_998, sent=False))["text"] == "last"


def test_close_frees_protocol_v3_subscription_capacity_without_deleting_history(tmp_path: Path) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    template = _records(tmp_path)["one"]
    first_clone = ""
    for index in range(1, 129):
        clone = copy.deepcopy(template)
        key = f"{index:064x}"
        if key == template["key"]:
            raise AssertionError("fixture key collision")
        if not first_clone:
            first_clone = key
        clone.update(key=key, reply_nonce=f"{index:022d}", reply_protocol=3,
                     reply_next_ordinal=1, phase="awaiting_reply",
                     queue_id=f"{index:020d}-{key}")
        chat_module._write(tmp_path / "requests" / f"{key}.json", clone)
    reply_path = tmp_path / "replies" / f"{first_clone}.json"
    chat_module._write(reply_path, {"text": "retained history"})
    history = reply_path.read_bytes()

    with pytest.raises(ValueError, match="129 active requests"):
        bridge.validate_continuous_output()
    chat_module.close_replies(tmp_path, first_clone)

    bridge.validate_continuous_output()
    assert len(bridge.output_markers()) == 128
    assert reply_path.read_bytes() == history
    status_document = bridge.status()
    assert status_document["reply_subscriptions"] == {"active": 128, "limit": 128}
    status = [as_mapping(item, "request")
              for item in as_sequence(status_document["requests"], "requests")]
    assert len(status) == 129
    closed, = [item for item in status if item["key"] == first_clone]
    assert closed["reply_closed_at"] and closed["reply_close_reason"] == "operator"


def test_closed_v3_retained_history_is_quiet_but_new_reuse_gets_feedback(tmp_path: Path) -> None:
    bridge, harness, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _records(tmp_path)["one"]
    historical = _snapshot(_block(record, "old answer", ordinal=1))
    assert not bridge.capture_output(historical)["errors"]
    chat_module.close_replies(tmp_path, str(record["key"]))

    bridge.capture_output(historical)
    assert len(harness.prompts) == 1
    assert not list((tmp_path / "feedback").glob("*.json"))

    bridge.capture_output(_snapshot(_block(record, "new closed answer", ordinal=1)))
    assert len(harness.prompts) == 2
    assert f'{record["reply_nonce"]}_1' in harness.prompts[-1]
    assert "not available" in harness.prompts[-1]


def test_pattern_size_bound_fails_before_resolving_herdr(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, harness, _ = setup(tmp_path)
    monkeypatch.setattr(chat_module, "_MAX_OUTPUT_PATTERN_BYTES", 1)
    monkeypatch.setattr(harness, "pane_info",
                        lambda pane: (_ for _ in ()).throw(AssertionError("external Herdr access")))
    with pytest.raises(ValueError, match="32 KiB safety bound"):
        bridge.open_output(("A" * 22 + "_1",))


class _StopLoop(BaseException):
    pass


class _Clock:
    def __init__(self, *, allowed_sleeps: int = 0) -> None:
        self.now = 0.0
        self.sleeps: list[float] = []
        self.allowed_sleeps = allowed_sleeps

    def monotonic(self) -> float:
        return self.now

    def sleep(self, seconds: float) -> None:
        self.sleeps.append(seconds)
        if len(self.sleeps) > self.allowed_sleeps:
            raise _StopLoop()
        self.now += seconds


class _ObserverClock:
    def __init__(self) -> None:
        self.now = 0.0
        self.wall = datetime(2026, 1, 2, tzinfo=timezone.utc)

    def monotonic(self) -> float:
        return self.now

    def utc(self) -> str:
        return self.wall.isoformat().replace("+00:00", "Z")

    def advance(self, seconds: float) -> None:
        self.now += seconds
        self.wall += timedelta(seconds=seconds)


def test_output_observer_rate_limits_and_flushes_latest_transition(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    setup(tmp_path)
    clock = _ObserverClock()
    writes: list[dict[str, object]] = []
    original = chat_module._write

    def write(path: Path, document: dict[str, object]) -> None:
        if path == tmp_path / "output.json":
            writes.append(dict(document))
        original(path, document)

    monkeypatch.setattr(chat_module, "_write", write)
    observer = _OutputObserver(tmp_path / "output.json", 60, monotonic=clock.monotonic,
                               utc=clock.utc)
    assert observer.observe("connected", None)
    first_mtime = (tmp_path / "output.json").stat().st_mtime_ns
    clock.advance(1)
    assert not observer.observe("connected", None)
    assert len(writes) == 1
    assert (tmp_path / "output.json").stat().st_mtime_ns == first_mtime

    clock.advance(1)
    assert not observer.observe("retrying", "socket unavailable")
    clock.advance(1)
    assert not observer.observe("retrying", "permission denied")
    assert len(writes) == 1
    clock.advance(57)
    assert observer.flush()
    assert len(writes) == 2
    assert writes[-1] == {"state": "retrying", "error": "permission denied",
                          "updated_at": "2026-01-02T00:01:00Z"}
    clock.advance(600)
    assert not observer.flush(), "unchanged state must never create a periodic write"
    assert len(writes) == 2


def test_output_observer_recovers_saved_deadline_after_restart(tmp_path: Path) -> None:
    setup(tmp_path)
    clock = _ObserverClock()
    observer = _OutputObserver(tmp_path / "output.json", 60, monotonic=clock.monotonic,
                               utc=clock.utc)
    assert observer.observe("connected", None)
    saved_mtime = (tmp_path / "output.json").stat().st_mtime_ns

    clock.advance(30)
    restarted = _OutputObserver(tmp_path / "output.json", 60, monotonic=clock.monotonic,
                                utc=clock.utc)
    assert not restarted.observe("retrying", "socket unavailable")
    assert (tmp_path / "output.json").stat().st_mtime_ns == saved_mtime
    clock.advance(30)
    assert restarted.flush()
    assert _read(tmp_path / "output.json")["state"] == "retrying"

    clock.advance(120)
    overdue = _OutputObserver(tmp_path / "output.json", 60, monotonic=clock.monotonic,
                              utc=clock.utc)
    assert overdue.observe("connected", None)


def test_output_observer_migrates_compatible_older_error_once(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    setup(tmp_path)
    older_error = "older frame: " + "x" * 3000
    chat_module._write(tmp_path / "output.json", {
        "state": "retrying", "error": older_error,
        "updated_at": "2025-12-31T23:59:00Z",
    })
    clock = _ObserverClock()
    writes: list[dict[str, object]] = []
    original = chat_module._write

    def write(path: Path, document: dict[str, object]) -> None:
        if path == tmp_path / "output.json":
            writes.append(dict(document))
        original(path, document)

    monkeypatch.setattr(chat_module, "_write", write)
    observer = _OutputObserver(tmp_path / "output.json", 60, monotonic=clock.monotonic,
                               utc=clock.utc)
    assert len(writes) == 1
    migrated = _read(tmp_path / "output.json")
    assert migrated == {
        "state": "retrying", "error": older_error[:2000],
        "updated_at": "2026-01-02T00:00:00Z",
    }
    assert not observer.observe("retrying", older_error[:2000])
    _OutputObserver(tmp_path / "output.json", 60, monotonic=clock.monotonic,
                    utc=clock.utc)
    assert len(writes) == 1


def test_output_observer_refuses_oversized_older_error(
    tmp_path: Path,
) -> None:
    setup(tmp_path)
    legacy_error = "legacy frame: " + "x" * (2 * 1024 * 1024)
    path = tmp_path / "output.json"
    path.write_text(json.dumps({
        "state": "retrying", "error": legacy_error,
        "updated_at": "2025-12-31T23:59:00Z",
    }), encoding="utf-8")
    path.chmod(0o600)
    with pytest.raises(AgentDeliveryError, match="exceeds max_artifact_bytes=8192"):
        _OutputObserver(path, 60)


@pytest.mark.parametrize(
    "saved_error",
    [True, 7, ["not", "an", "error string"], {"error": "not a string"}],
    ids=("boolean", "integer", "list", "object"),
)
def test_output_observer_refuses_nonstring_saved_error_without_rewriting(
    tmp_path: Path, saved_error: object,
) -> None:
    setup(tmp_path)
    path = tmp_path / "output.json"
    document: dict[str, object] = {
        "state": "retrying", "error": saved_error,
        "updated_at": "2026-01-02T00:00:00Z",
    }
    chat_module._write(path, document)
    before = path.read_bytes()

    with pytest.raises(ValueError, match="saved output observer error must be null or a string"):
        _OutputObserver(path, 60)

    assert path.read_bytes() == before


def _instrument(bridge: Bridge, harness: Harness, clock: _Clock,
                monkeypatch: pytest.MonkeyPatch) -> tuple[list[float], list[str]]:
    monkeypatch.setattr(chat_module, "time", clock)
    ticks: list[float] = []
    reads: list[str] = []
    original = bridge.tick

    def tick() -> dict[str, object]:
        ticks.append(clock.now)
        return original()

    def read(pane_id: str, *, source: str = "recent-unwrapped", lines: int | None = None) -> str:
        reads.append(pane_id)
        raise AssertionError("the event loop must not poll pane reads")

    monkeypatch.setattr(bridge, "tick", tick)
    monkeypatch.setattr(harness, "read", read)
    return ticks, reads


def test_daemon_keeps_output_subscription_across_provider_poll_cycles(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, harness, chat = setup(tmp_path)
    chat.message()
    clock = _Clock()
    ticks, reads = _instrument(bridge, harness, clock, monkeypatch)
    opens: list[float] = []
    output_writes: list[dict[str, object]] = []
    waits = 0
    original_write = chat_module._write

    def write(path: Path, document: dict[str, object]) -> None:
        if path == tmp_path / "output.json":
            output_writes.append(dict(document))
        original_write(path, document)

    monkeypatch.setattr(chat_module, "_write", write)

    def wait(timeout: float) -> tuple[PaneOutputSnapshot, ...]:
        nonlocal waits
        waits += 1
        if waits == 3:
            raise _StopLoop()
        clock.now += timeout
        return ()

    stream = _FakeStream(wait)

    def open_output(nonces: Sequence[str]) -> PaneOutputStream:
        opens.append(clock.now)
        return stream

    monkeypatch.setattr(bridge, "_open_cached_output",
                        lambda subscription: open_output(subscription.markers))
    with pytest.raises(_StopLoop):
        chat_module._run_bridge(bridge, 60, "test-chat")
    assert ticks == [0, 60, 120]
    assert opens == [0] and stream.waits == [60, 60, 60]
    assert not reads and len(harness.prompts) == 1
    assert stream.closes == 1
    assert [item["state"] for item in output_writes] == ["connected"]


def test_daemon_sends_output_before_long_provider_poll_deadline(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, harness, chat = setup(tmp_path)
    chat.message()
    clock = _Clock()
    ticks, reads = _instrument(bridge, harness, clock, monkeypatch)
    waits = 0

    def wait(timeout: float) -> tuple[PaneOutputSnapshot, ...]:
        nonlocal waits
        waits += 1
        if waits == 2:
            assert timeout == 59.75
            raise _StopLoop()
        assert timeout == 60
        clock.now += 0.25
        return (_snapshot(_block(_records(tmp_path)["one"])),)

    streams = [_FakeStream(wait), _FakeStream(wait)]
    subscriptions: list[tuple[str, ...]] = []

    def open_output(nonces: Sequence[str]) -> PaneOutputStream:
        subscriptions.append(tuple(nonces))
        return streams[len(subscriptions) - 1]

    monkeypatch.setattr(bridge, "_open_cached_output",
                        lambda subscription: open_output(subscription.markers))
    with pytest.raises(_StopLoop):
        chat_module._run_bridge(bridge, 60, "test-chat")
    assert ticks == [0] and clock.now == 0.25
    assert len(chat.sent) == 1 and not reads
    nonce = _records(tmp_path)["one"]["reply_nonce"]
    assert subscriptions == [(f"{nonce}_1",), (f"{nonce}_2",)]
    assert [stream.closes for stream in streams] == [1, 1]
    assert not clock.sleeps
    assert _read(tmp_path / "output.json")["state"] == "connected"
    assert bridge.output_requests()


def test_daemon_delivers_later_progress_before_next_provider_poll_without_status(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, harness, chat = setup(tmp_path)
    chat.message()
    clock = _Clock()
    ticks, reads = _instrument(bridge, harness, clock, monkeypatch)
    opens: list[float] = []
    bodies = ["Started", "Milestone reached", "Finished"]
    subscriptions: list[tuple[str, ...]] = []

    def open_output(nonces: Sequence[str]) -> PaneOutputStream:
        record = _records(tmp_path)["one"]
        ordinal = len(subscriptions) + 1
        assert tuple(nonces) == (f"{record['reply_nonce']}_{ordinal}",)
        subscriptions.append(tuple(nonces))
        opens.append(clock.now)

        def wait(timeout: float) -> tuple[PaneOutputSnapshot | PaneAgentStatus, ...]:
            if len(chat.sent) == len(bodies):
                raise _StopLoop()
            clock.now += 0.1
            retained = "\n".join(_block(record, body, ordinal=index)
                                  for index, body in enumerate(bodies[:ordinal], start=1))
            return (_snapshot(retained),)

        return _FakeStream(wait)

    monkeypatch.setattr(bridge, "_open_cached_output",
                        lambda subscription: open_output(subscription.markers))
    with pytest.raises(_StopLoop):
        chat_module._run_bridge(bridge, 60, "test-chat")
    assert ticks == [0] and opens == pytest.approx([0, 0.1, 0.2, 0.3])
    assert [call["text"] for call in chat.calls if call["action"] == "send"] == [
        "[test-agent] " + body for body in bodies
    ]
    assert len(chat.sent) == 3 and len(harness.prompts) == 1 and not reads


def test_daemon_eof_backs_off_then_reconnects_without_reprompt(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, harness, chat = setup(tmp_path)
    chat.message()
    clock = _Clock(allowed_sleeps=1)
    ticks, reads = _instrument(bridge, harness, clock, monkeypatch)
    opens: list[float] = []
    def disconnected(timeout: float) -> tuple[PaneOutputSnapshot, ...]:
        clock.now += 0.25
        raise OutputStreamError("stream_eof", "server restarted")

    def recovered(timeout: float) -> tuple[PaneOutputSnapshot, ...]:
        clock.now += 0.25
        return (_snapshot(_block(_records(tmp_path)["one"])),)

    streams = [_FakeStream(disconnected), _FakeStream(recovered),
               _FakeStream(lambda timeout: (_ for _ in ()).throw(_StopLoop()))]

    def open_output(nonces: Sequence[str]) -> PaneOutputStream:
        opens.append(clock.now)
        return streams[len(opens) - 1]

    monkeypatch.setattr(bridge, "_open_cached_output",
                        lambda subscription: open_output(subscription.markers))
    with pytest.raises(_StopLoop):
        chat_module._run_bridge(bridge, 60, "test-chat")
    assert opens == [0, 1.25, 1.5] and ticks == [0]
    assert clock.now == 1.5 and clock.sleeps == [1]
    assert [stream.closes for stream in streams] == [1, 1, 1]
    assert not reads and len(harness.prompts) == 1 and len(chat.sent) == 1


def test_daemon_failed_connections_use_bounded_exponential_backoff(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, harness, chat = setup(tmp_path)
    chat.message()
    clock = _Clock(allowed_sleeps=2)
    _instrument(bridge, harness, clock, monkeypatch)
    opens: list[float] = []

    def stopped(timeout: float) -> tuple[PaneOutputSnapshot, ...]:
        raise _StopLoop()

    stream = _FakeStream(stopped)

    def open_output(nonces: Sequence[str]) -> PaneOutputStream:
        opens.append(clock.now)
        if len(opens) < 3:
            raise OSError("socket unavailable")
        return stream

    monkeypatch.setattr(bridge, "_open_cached_output",
                        lambda subscription: open_output(subscription.markers))
    with pytest.raises(_StopLoop):
        chat_module._run_bridge(bridge, 60, "test-chat")
    assert opens == [0, 1, 3] and clock.sleeps == [1, 2]
    assert len(harness.prompts) == 1 and stream.closes == 1


def test_daemon_rebuilds_subscription_after_bad_capture_to_avoid_starving_pending_reply(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, harness, chat = setup(tmp_path)
    chat.message("broken")
    chat.message("valid")
    clock = _Clock()
    ticks, reads = _instrument(bridge, harness, clock, monkeypatch)
    subscriptions: list[tuple[str, ...]] = []
    complete_waits = 0

    def clipped(timeout: float) -> tuple[PaneOutputSnapshot, ...]:
        clock.now += 0.1
        nonce = _records(tmp_path)["broken"]["reply_nonce"]
        return (_snapshot(f"Only the end remains\n</GCHAT_REPLY_{nonce}>", truncated=True),)

    def complete(timeout: float) -> tuple[PaneOutputSnapshot, ...]:
        nonlocal complete_waits
        complete_waits += 1
        if complete_waits > 1:
            raise _StopLoop()
        clock.now += 0.1
        return (_snapshot(_block(_records(tmp_path)["valid"])),)

    def stopped(timeout: float) -> tuple[PaneOutputSnapshot, ...]:
        raise _StopLoop()

    streams = [_FakeStream(clipped), _FakeStream(complete), _FakeStream(stopped)]

    def open_output(nonces: Sequence[str]) -> PaneOutputStream:
        subscriptions.append(tuple(nonces))
        return streams[len(subscriptions) - 1]

    monkeypatch.setattr(bridge, "_open_cached_output",
                        lambda subscription: open_output(subscription.markers))
    with pytest.raises(_StopLoop):
        chat_module._run_bridge(bridge, 60, "test-chat")
    records = _records(tmp_path)
    broken_first = f'{records["broken"]["reply_nonce"]}_1'
    valid_first = f'{records["valid"]["reply_nonce"]}_1'
    assert set(subscriptions[0]) == {broken_first, valid_first}
    assert subscriptions[1] == (valid_first,)
    assert subscriptions[2] == (f'{records["valid"]["reply_nonce"]}_2',)
    assert len(subscriptions) == 3
    assert [stream.closes for stream in streams] == [1, 1, 1]
    assert records["broken"]["capture_error"] and records["valid"]["phase"] == "replied"
    assert ticks == [0] and not reads
    assert len(chat.sent) == 1 and len(harness.prompts) == 2


def test_early_fenced_close_recovers_on_idle_and_continues_watching_later_replies(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, harness, chat = setup(tmp_path)
    chat.message()
    clock = _Clock()
    ticks, _ = _instrument(bridge, harness, clock, monkeypatch)
    subscriptions: list[tuple[str, ...]] = []
    reads: list[float] = []
    statuses = 0

    def quoted_example(timeout: float) -> tuple[PaneOutputSnapshot | PaneAgentStatus, ...]:
        clock.now += 0.1
        harness.state = "working"
        return (_snapshot("```text\n" + _block(_records(tmp_path)["one"], "Example only") + "\n```"),)

    def settled(timeout: float) -> tuple[PaneOutputSnapshot | PaneAgentStatus, ...]:
        nonlocal statuses
        statuses += 1
        clock.now += 0.1
        # A queued stale idle hint must not read a still-working target.
        if statuses == 2:
            harness.state = "idle"
        return (PaneAgentStatus("w1:p1", "idle"),)

    def read(pane_id: str, *, source: str = "recent-unwrapped", lines: int | None = None) -> str:
        assert pane_id == "w1:p1" and source == "recent-unwrapped" and lines == 4000
        reads.append(clock.now)
        record = _records(tmp_path)["one"]
        return "```text\n" + _block(record, "Example only") + "\n```\n" + _block(record, "Actual final answer")

    def stopped(timeout: float) -> tuple[PaneOutputSnapshot | PaneAgentStatus, ...]:
        raise _StopLoop()

    streams = [_FakeStream(quoted_example), _FakeStream(settled), _FakeStream(stopped)]

    def open_output(nonces: Sequence[str]) -> PaneOutputStream:
        subscriptions.append(tuple(nonces))
        return streams[len(subscriptions) - 1]

    monkeypatch.setattr(bridge, "_open_cached_output",
                        lambda subscription: open_output(subscription.markers))
    monkeypatch.setattr(harness, "read", read)
    with pytest.raises(_StopLoop):
        chat_module._run_bridge(bridge, 60, "test-chat")
    assert len(subscriptions[0]) == 1 and subscriptions[1] == ()
    first = subscriptions[0][0]
    assert subscriptions[2] == (first[:-1] + "2",)
    assert [stream.closes for stream in streams] == [1, 1, 1]
    assert len(reads) == 1 and reads[0] == pytest.approx(0.3)
    assert ticks == [0] and len(harness.prompts) == 1
    assert next(iter(chat.sent.values()))["text"] == "[test-agent] Actual final answer"
    saved = _records(tmp_path)["one"]
    assert "capture_error" not in saved and saved["phase"] == "replied"
    assert as_mapping(saved["reply_capture"], "capture")["snapshot_truncated"] is None


def test_already_idle_bad_capture_rearms_watch_without_automatic_pane_rereads(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, harness, chat = setup(tmp_path)
    chat.message()
    clock = _Clock()
    ticks, _ = _instrument(bridge, harness, clock, monkeypatch)
    subscriptions: list[tuple[str, ...]] = []
    reads: list[float] = []
    statuses = 0

    def clipped_text() -> str:
        return f"Missing opening\n</GCHAT_REPLY_{_records(tmp_path)['one']['reply_nonce']}>"

    def clipped(timeout: float) -> tuple[PaneOutputSnapshot | PaneAgentStatus, ...]:
        return (_snapshot(clipped_text(), truncated=True),)

    def idle_then_wait(timeout: float) -> tuple[PaneOutputSnapshot | PaneAgentStatus, ...]:
        nonlocal statuses
        statuses += 1
        if statuses == 1:
            return (PaneAgentStatus("w1:p1", "idle"),)
        if statuses == 2:
            clock.now += timeout
            return ()
        raise _StopLoop()

    def read(pane_id: str, *, source: str = "recent-unwrapped", lines: int | None = None) -> str:
        reads.append(clock.now)
        return clipped_text()

    streams = [_FakeStream(clipped), _FakeStream(idle_then_wait)]

    def open_output(nonces: Sequence[str]) -> PaneOutputStream:
        subscriptions.append(tuple(nonces))
        return streams[len(subscriptions) - 1]

    monkeypatch.setattr(bridge, "_open_cached_output",
                        lambda subscription: open_output(subscription.markers))
    monkeypatch.setattr(harness, "read", read)
    with pytest.raises(_StopLoop):
        chat_module._run_bridge(bridge, 60, "test-chat")
    nonce = next(iter(_records(tmp_path).values()))["reply_nonce"]
    assert subscriptions == [(f"{nonce}_1",), ()]
    assert reads == [0] and ticks == [0, 60]
    assert [stream.closes for stream in streams] == [1, 1]
    assert len(harness.prompts) == 1 and not chat.sent
    assert _records(tmp_path)["one"]["capture_error"]


def test_settled_hint_from_wrong_pane_does_not_read_or_send(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, harness, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    reads: list[str] = []

    def read(pane_id: str, *, source: str = "recent-unwrapped", lines: int | None = None) -> str:
        reads.append(pane_id)
        return _block(_records(tmp_path)["one"])

    monkeypatch.setattr(harness, "read", read)
    before = len(chat.calls)
    with pytest.raises(HerdrUnavailable, match="different coordinator pane"):
        bridge.capture_event(PaneAgentStatus("w9:p1", "idle"))
    assert not reads and len(chat.calls) == before
