"""One chat request can produce multiple independently durable public replies."""

from __future__ import annotations

from pathlib import Path

import pytest

import agentctl.agent as agent_module
import agentctl.chat as chat_module
from agentctl.agent import Target
from agentctl.chat import Bridge, Config, _read, _write, submit_reply
from agentctl.chat_output import PaneOutputSnapshot
from agentctl.jsonx import as_mapping
from tests.test_herdr_chat import Chat, Harness


class MultipleReplyChat(Chat):
    """Give each idempotency key a distinct message resource, as providers do."""

    def __init__(self) -> None:
        super().__init__()
        self.lose_ack_for: str | None = None

    def __call__(self, request: dict[str, object]) -> dict[str, object]:
        if request["action"] != "send":
            return super().__call__(request)
        self.calls.append(request)
        request_id = str(request["request_id"])
        self.sent[request_id] = request
        if self.lose_ack_for == request["text"]:
            self.lose_ack_for = None
            raise OSError("reply accepted but acknowledgement lost")
        return {"id": f"spaces/test/messages/reply-{request_id}"}


def _setup(state: Path) -> tuple[Bridge, Harness, MultipleReplyChat]:
    harness, transport = Harness(), MultipleReplyChat()
    target = Target(pane_id="w1:p1", expected_agent="codex",
                    expected_cwd="/work/project", expected_workspace="project")
    Bridge.initialize(state, Config("spaces/test", ("users/owner",), target, "test-agent"),
                      after="2026-01-01T00:00:00Z")
    return Bridge(state, harness, transport), harness, transport


def _record(state: Path, identifier: str = "one") -> dict[str, object]:
    for path in (state / "requests").glob("*.json"):
        record = _read(path)
        message = as_mapping(record["message"], "message")
        if message["id"] == f"spaces/test/messages/{identifier}":
            return record
    raise AssertionError(f"missing request {identifier}")


def _block(
    record: dict[str, object], text: str, *, ordinal: int | None = None, legacy: bool = False,
) -> str:
    prefix = "GCHAT_REPLY" if legacy else "CHAT_REPLY"
    nonce = record["reply_nonce"]
    if record.get("reply_protocol") == 3:
        selected = record.get("reply_next_ordinal", 1) if ordinal is None else ordinal
        nonce = f"{nonce}_{selected}"
    return f"<{prefix}_{nonce}>\n{text}\n</{prefix}_{nonce}>"


def _snapshot(text: str) -> PaneOutputSnapshot:
    return PaneOutputSnapshot("w1:p1", text, False, 0)


def _sent_texts(chat: MultipleReplyChat) -> list[str]:
    return [str(request["text"]) for request in chat.sent.values()]


def test_multiple_blocks_become_separate_messages_and_restart_does_not_repost(tmp_path: Path) -> None:
    bridge, harness, chat = _setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _record(tmp_path)
    snapshot = _snapshot(harness.prompts[0] + "\n" + _block(record, "First milestone", ordinal=1)
                         + "\n" + _block(record, "Second milestone", ordinal=2))
    assert not bridge.capture_output(snapshot)["errors"]
    assert _sent_texts(chat) == ["[test-agent] First milestone", "[test-agent] Second milestone"]
    assert {request["thread"] for request in chat.sent.values()} == {"spaces/test/threads/one"}
    restarted = Bridge(tmp_path, harness, chat)
    restarted.capture_output(snapshot)
    restarted.tick()
    assert len(chat.sent) == 2
    assert len([call for call in chat.calls if call["action"] == "send"]) == 2


def test_identical_blocks_are_distinct_occurrences_but_replayed_snapshot_is_not(tmp_path: Path) -> None:
    bridge, harness, chat = _setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _record(tmp_path)
    block = _block(record, "Still working", ordinal=1)
    bridge.capture_output(_snapshot(harness.prompts[0] + "\n" + block))
    two = _snapshot(harness.prompts[0] + "\n" + block + "\n"
                    + _block(record, "Still working", ordinal=2))
    bridge.capture_output(two)
    Bridge(tmp_path, harness, chat).capture_output(two)
    assert _sent_texts(chat) == ["[test-agent] Still working", "[test-agent] Still working"]
    assert len([call for call in chat.calls if call["action"] == "send"]) == 2


def test_later_update_remains_routable_after_first_reply_and_new_request(tmp_path: Path) -> None:
    bridge, harness, chat = _setup(tmp_path)
    chat.message("one")
    bridge.tick()
    first = _record(tmp_path)
    bridge.capture_output(_snapshot(_block(first, "Started")))
    chat.message("two", timestamp="2026-01-02T00:01:00Z")
    bridge.tick()
    restarted = Bridge(tmp_path, harness, chat)
    restarted.capture_output(_snapshot(_block(first, "Finished original work", ordinal=2)))
    restarted.capture_output(_snapshot(_block(_record(tmp_path, "two"), "Handled follow-up")))
    assert _sent_texts(chat) == ["[test-agent] Started", "[test-agent] Finished original work",
                               "[test-agent] Handled follow-up"]
    assert [request["thread"] for request in chat.sent.values()] == [
        "spaces/test/threads/one", "spaces/test/threads/one", "spaces/test/threads/two",
    ]
    assert len(harness.prompts) == 2


def test_original_prompt_echo_remains_filterable_after_ordinal_advance_and_restart(
    tmp_path: Path,
) -> None:
    bridge, harness, chat = _setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _record(tmp_path)
    bridge._prime_prompt_cache()
    first = harness.prompts[0] + "\n" + _block(record, "First", ordinal=1)
    assert bridge.capture_output(_snapshot(first), deliver=False)["errors"] == []

    restarted = Bridge(tmp_path, harness, chat)
    restarted._prime_prompt_cache()
    second = first + "\n" + _block(record, "Second", ordinal=2)
    outcome = restarted.capture_output(_snapshot(second), deliver=False)
    assert outcome["errors"] == [] and outcome["captured"] == [record["key"]]
    restarted._deliver()
    assert _sent_texts(chat) == ["[test-agent] First", "[test-agent] Second"]


@pytest.mark.parametrize("first_prompt_retained", [True, False])
def test_new_prompt_echo_does_not_hide_an_uncaptured_earlier_request_reply(
    tmp_path: Path, first_prompt_retained: bool,
) -> None:
    bridge, harness, chat = _setup(tmp_path)
    chat.message("one")
    bridge.tick()
    first = _record(tmp_path)
    chat.message("two", timestamp="2026-01-02T00:01:00Z")
    bridge.tick()
    second = _record(tmp_path, "two")
    prefix = harness.prompts[0] + "\n" if first_prompt_retained else ""
    snapshot = _snapshot(prefix + _block(first, "First answer")
                         + "\n" + harness.prompts[1] + "\n" + _block(second, "Second answer"))
    bridge.capture_output(snapshot)
    assert _sent_texts(chat) == ["[test-agent] First answer", "[test-agent] Second answer"]
    assert len(harness.prompts) == 2


def test_uncertain_send_retries_its_identity_before_later_messages(tmp_path: Path) -> None:
    bridge, harness, chat = _setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _record(tmp_path)
    chat.lose_ack_for = "[test-agent] Second"
    snapshot = _snapshot("\n".join(
        _block(record, text, ordinal=ordinal)
        for ordinal, text in enumerate(("First", "Second", "Third"), start=1)
    ))
    with pytest.raises(OSError, match="acknowledgement lost"):
        bridge.capture_output(snapshot)
    assert _sent_texts(chat) == ["[test-agent] First", "[test-agent] Second"]
    restarted = Bridge(tmp_path, harness, chat)
    restarted.tick()
    restarted.capture_output(snapshot)
    sends = [call for call in chat.calls if call["action"] == "send"]
    assert [call["text"] for call in sends] == ["[test-agent] First", "[test-agent] Second",
                                               "[test-agent] Second", "[test-agent] Third"]
    assert sends[1]["request_id"] == sends[2]["request_id"]
    assert len({call["request_id"] for call in sends}) == 3


@pytest.mark.parametrize("manual_text", ["Manual recovery", "Automatic reply"])
def test_concurrent_manual_recovery_cannot_be_overwritten_by_first_capture(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, manual_text: str,
) -> None:
    bridge, harness, chat = _setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _record(tmp_path)
    key = str(record["key"])
    create = agent_module._atomic_json_create
    raced = False

    def create_with_recovery(
        path: str, document: dict[str, object], *, max_artifact_bytes: int | None = None,
    ) -> None:
        nonlocal raced
        if not raced:
            raced = True
            submit_reply(tmp_path, key, manual_text)
        create(path, document, max_artifact_bytes=max_artifact_bytes)

    monkeypatch.setattr(agent_module, "_atomic_json_create", create_with_recovery)
    snapshot = _snapshot(_block(record, "Automatic reply"))
    bridge.capture_output(snapshot)
    assert raced
    expected = [f"[test-agent] {manual_text}"]
    if manual_text != "Automatic reply":
        expected.append("[test-agent] Automatic reply")
    assert _sent_texts(chat) == expected
    assert next(iter(chat.sent)) == record["request_id"]
    assert _read(chat_module._reply_item_path(tmp_path, key, 0, sent=True))["text"] == manual_text
    restarted = Bridge(tmp_path, harness, chat)
    restarted.capture_output(snapshot)
    restarted.tick()
    assert _sent_texts(chat) == expected
    assert len([call for call in chat.calls if call["action"] == "send"]) == len(expected)


def test_crash_during_manual_first_shift_recovers_both_replies_in_order(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, harness, chat = _setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _record(tmp_path)
    key = str(record["key"])
    create = agent_module._atomic_json_create
    raced = False

    def create_with_submission(
        path: str, document: dict[str, object], *, max_artifact_bytes: int | None = None,
    ) -> None:
        nonlocal raced
        if not raced:
            raced = True
            submit_reply(tmp_path, key, "Manual recovery")
        create(path, document, max_artifact_bytes=max_artifact_bytes)

    monkeypatch.setattr(agent_module, "_atomic_json_create", create_with_submission)
    bridge.capture_output(_snapshot(_block(record, "Automatic reply")), deliver=False)
    assert raced
    original_summary = Bridge._write_reply_summary

    def crash_before_shift_summary(
        self: Bridge, path: Path, saved: dict[str, object], *, count: int, sent: int,
        total_bytes: int, pending_bytes: int, next_ordinal: int | None = None,
        offset: int | None = None,
    ) -> None:
        if count == 2 and offset == 1:
            raise OSError("crash before shift summary")
        original_summary(
            self, path, saved, count=count, sent=sent, total_bytes=total_bytes,
            pending_bytes=pending_bytes, next_ordinal=next_ordinal, offset=offset)

    monkeypatch.setattr(Bridge, "_write_reply_summary", crash_before_shift_summary)
    with pytest.raises(OSError, match="before shift summary"):
        bridge._deliver()
    monkeypatch.setattr(Bridge, "_write_reply_summary", original_summary)
    crashed = _read(tmp_path / "requests" / f"{key}.json")
    assert crashed["reply_item_count"] == 1 and crashed["reply_ordinal_offset"] == 0
    assert _read(chat_module._reply_item_path(tmp_path, key, 0, sent=False))["text"] == "Manual recovery"
    assert _read(chat_module._reply_item_path(
        tmp_path, key, 1, sent=False))["reply_shift_from"] == 0

    restarted = Bridge(tmp_path, harness, chat)
    restarted.tick()
    assert [call["text"] for call in chat.calls if call["action"] == "send"] == [
        "[test-agent] Manual recovery", "[test-agent] Automatic reply"]
    recovered = _read(tmp_path / "requests" / f"{key}.json")
    assert (recovered["reply_item_count"], recovered["reply_sent_count"],
            recovered["reply_ordinal_offset"], recovered["reply_next_ordinal"]) == (2, 2, 1, 2)


def test_crash_after_shift_journal_create_recovers_before_index_zero_overwrite(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, harness, chat = _setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _record(tmp_path)
    key = str(record["key"])
    create = agent_module._atomic_json_create
    raced = False

    def create_with_submission(
        path: str, document: dict[str, object], *, max_artifact_bytes: int | None = None,
    ) -> None:
        nonlocal raced
        if not raced:
            raced = True
            submit_reply(tmp_path, key, "Manual recovery")
        create(path, document, max_artifact_bytes=max_artifact_bytes)

    monkeypatch.setattr(agent_module, "_atomic_json_create", create_with_submission)
    bridge.capture_output(_snapshot(_block(record, "Automatic reply")), deliver=False)
    monkeypatch.setattr(agent_module, "_atomic_json_create", create)
    assert raced
    original_write = Bridge._write_pending_reply

    def crash_before_manual_overwrite(
        self: Bridge, saved: dict[str, object], item: dict[str, object],
    ) -> None:
        if item.get("reply_key") == "0" and item.get("reply_source") == "file":
            raise OSError("crash after shift journal create")
        original_write(self, saved, item)

    monkeypatch.setattr(Bridge, "_write_pending_reply", crash_before_manual_overwrite)
    with pytest.raises(OSError, match="after shift journal create"):
        bridge._deliver()
    monkeypatch.setattr(Bridge, "_write_pending_reply", original_write)
    crashed = _read(tmp_path / "requests" / f"{key}.json")
    assert crashed["reply_item_count"] == 1 and crashed["reply_ordinal_offset"] == 0
    assert _read(chat_module._reply_item_path(
        tmp_path, key, 0, sent=False))["text"] == "Automatic reply"
    assert _read(chat_module._reply_item_path(
        tmp_path, key, 1, sent=False))["reply_shift_from"] == 0

    Bridge(tmp_path, harness, chat).tick()
    assert [call["text"] for call in chat.calls if call["action"] == "send"] == [
        "[test-agent] Manual recovery", "[test-agent] Automatic reply"]


def test_crash_after_shift_summary_cleans_journal_without_losing_either_reply(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, harness, chat = _setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _record(tmp_path)
    key = str(record["key"])
    create = agent_module._atomic_json_create
    raced = False

    def create_with_submission(
        path: str, document: dict[str, object], *, max_artifact_bytes: int | None = None,
    ) -> None:
        nonlocal raced
        if not raced:
            raced = True
            submit_reply(tmp_path, key, "Manual recovery")
        create(path, document, max_artifact_bytes=max_artifact_bytes)

    monkeypatch.setattr(agent_module, "_atomic_json_create", create_with_submission)
    bridge.capture_output(_snapshot(_block(record, "Automatic reply")), deliver=False)
    monkeypatch.setattr(agent_module, "_atomic_json_create", create)
    assert raced
    original_write = Bridge._write_pending_reply

    def crash_before_shift_cleanup(
        self: Bridge, saved: dict[str, object], item: dict[str, object],
    ) -> None:
        if (item.get("reply_key") == "1" and item.get("reply_source") == "capture"
                and item.get("reply_shift_from") is None):
            raise OSError("crash after shift summary")
        original_write(self, saved, item)

    monkeypatch.setattr(Bridge, "_write_pending_reply", crash_before_shift_cleanup)
    with pytest.raises(OSError, match="after shift summary"):
        bridge._deliver()
    monkeypatch.setattr(Bridge, "_write_pending_reply", original_write)
    crashed = _read(tmp_path / "requests" / f"{key}.json")
    assert (crashed["reply_item_count"], crashed["reply_ordinal_offset"],
            crashed["reply_next_ordinal"]) == (2, 1, 2)
    assert _read(chat_module._reply_item_path(
        tmp_path, key, 1, sent=False))["reply_shift_from"] == 0

    Bridge(tmp_path, harness, chat).tick()
    assert [call["text"] for call in chat.calls if call["action"] == "send"] == [
        "[test-agent] Manual recovery", "[test-agent] Automatic reply"]
    assert "reply_shift_from" not in _read(chat_module._reply_item_path(
        tmp_path, key, 1, sent=True))


def test_crash_during_identical_file_binding_rebases_next_ordinal(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, harness, chat = _setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _record(tmp_path)
    key = str(record["key"])
    submit_reply(tmp_path, key, "Same reply")
    original_summary = Bridge._write_reply_summary

    def crash_before_bind_summary(
        self: Bridge, path: Path, saved: dict[str, object], *, count: int, sent: int,
        total_bytes: int, pending_bytes: int, next_ordinal: int | None = None,
        offset: int | None = None,
    ) -> None:
        if offset == 0 and next_ordinal == 2:
            raise OSError("crash before bind summary")
        original_summary(
            self, path, saved, count=count, sent=sent, total_bytes=total_bytes,
            pending_bytes=pending_bytes, next_ordinal=next_ordinal, offset=offset)

    monkeypatch.setattr(Bridge, "_write_reply_summary", crash_before_bind_summary)
    with pytest.raises(OSError, match="before bind summary"):
        bridge.capture_output(_snapshot(_block(record, "Same reply")), deliver=False)
    monkeypatch.setattr(Bridge, "_write_reply_summary", original_summary)
    crashed = _read(tmp_path / "requests" / f"{key}.json")
    assert (crashed["reply_ordinal_offset"], crashed["reply_next_ordinal"]) == (1, 1)
    assert _read(chat_module._reply_item_path(
        tmp_path, key, 0, sent=False))["reply_ordinal"] == 1

    restarted = Bridge(tmp_path, harness, chat)
    restarted.tick()
    recovered = _read(tmp_path / "requests" / f"{key}.json")
    assert (recovered["reply_ordinal_offset"], recovered["reply_next_ordinal"]) == (0, 2)
    assert [call["text"] for call in chat.calls if call["action"] == "send"] == [
        "[test-agent] Same reply"]


def test_all_provider_reply_ids_suppress_self_echo_after_restart(tmp_path: Path) -> None:
    bridge, harness, chat = _setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _record(tmp_path)
    bridge.capture_output(_snapshot(_block(record, "One", ordinal=1) + "\n"
                                    + _block(record, "Two", ordinal=2)))
    assert len(chat.sent) == 2
    for index, (request_id, sent) in enumerate(chat.sent.items()):
        chat.messages.append({
            "id": f"spaces/test/messages/reply-{request_id}", "text": sent["text"],
            "sender": "users/owner", "thread": "spaces/test/threads/one",
            "created_at": f"2026-01-02T00:00:0{index + 1}Z",
        })
    Bridge(tmp_path, harness, chat).tick()
    assert len(harness.prompts) == 1
    assert len(list((tmp_path / "requests").glob("*.json"))) == 1


def test_new_prompt_is_generic_and_explicitly_allows_progress_replies(tmp_path: Path) -> None:
    bridge, harness, chat = _setup(tmp_path)
    chat.message()
    bridge.tick()
    prompt = harness.prompts[0]
    identifier = f"{_record(tmp_path)['reply_nonce']}_1"
    assert f"`{identifier}`" in prompt
    assert f"<CHAT_REPLY_{identifier}>" not in prompt
    assert "GCHAT" not in prompt and "Google Chat" not in prompt and "Discord" not in prompt
    assert "multiple" in prompt.lower()
    assert "progress" in prompt.lower()


def test_legacy_fence_for_already_delivered_prompt_can_still_reply(tmp_path: Path) -> None:
    bridge, _, chat = _setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _record(tmp_path)
    record.pop("reply_protocol", None)
    _write(tmp_path / "requests" / f"{record['key']}.json", record)
    bridge.capture_output(_snapshot(_block(record, "Compatible reply", legacy=True)))
    bridge.capture_output(_snapshot(_block(record, "Later compatible update", legacy=True)))
    assert _sent_texts(chat) == ["[test-agent] Compatible reply", "[test-agent] Later compatible update"]


@pytest.mark.parametrize("unknown", ["XXXXXXXXXXXXXXXXXXXXXX", "idx"])
def test_unknown_reply_id_injects_one_routing_error_and_lists_available_ids(
    tmp_path: Path, unknown: str,
) -> None:
    bridge, harness, chat = _setup(tmp_path)
    chat.message("one")
    chat.message("two", timestamp="2026-01-02T00:01:00Z")
    bridge.tick()
    records = [_record(tmp_path, name) for name in ("one", "two")]
    original_prompts = len(harness.prompts)
    invalid = _snapshot(f"<CHAT_REPLY_{unknown}>\nWrong destination\n</CHAT_REPLY_{unknown}>")
    bridge.capture_output(invalid)
    bridge.tick()
    feedback = harness.prompts[original_prompts:]
    assert len(feedback) == 1
    assert unknown in feedback[0]
    assert all(str(record["reply_nonce"]) in feedback[0] for record in records)
    assert all(str(record["key"])[:12] in feedback[0] for record in records)
    assert not chat.sent
    restarted = Bridge(tmp_path, harness, chat)
    restarted.capture_output(invalid)
    restarted.tick()
    assert len(harness.prompts) == original_prompts + 1


def test_unknown_id_feedback_does_not_block_a_valid_reply_in_same_snapshot(tmp_path: Path) -> None:
    bridge, harness, chat = _setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _record(tmp_path)
    invalid = "<CHAT_REPLY_wrong-id>\nWrong destination\n</CHAT_REPLY_wrong-id>"
    bridge.capture_output(_snapshot(invalid + "\n" + _block(record, "Correct destination")))
    bridge.tick()
    assert _sent_texts(chat) == ["[test-agent] Correct destination"]
    assert len(harness.prompts) == 2
    assert "wrong-id" in harness.prompts[-1]


def test_unknown_reply_with_no_available_ids_reports_that_to_agent(tmp_path: Path) -> None:
    bridge, harness, chat = _setup(tmp_path)
    bridge.capture_output(_snapshot("<CHAT_REPLY_unknown>\nNowhere to send\n</CHAT_REPLY_unknown>"))
    bridge.tick()
    assert len(harness.prompts) == 1
    assert "unknown" in harness.prompts[0]
    assert "none" in harness.prompts[0].lower() or "no outstanding" in harness.prompts[0].lower()
    assert not chat.sent


def test_feedback_can_refresh_when_the_available_destinations_change(tmp_path: Path) -> None:
    bridge, harness, chat = _setup(tmp_path)
    chat.message()
    bridge.tick()
    invalid = _snapshot("<CHAT_REPLY_unknown>\nWrong destination\n</CHAT_REPLY_unknown>")
    bridge.capture_output(invalid)
    bridge.tick()
    chat.message("two", timestamp="2026-01-02T00:01:00Z")
    bridge.tick()
    bridge.capture_output(invalid)
    bridge.tick()
    assert len(harness.prompts) == 4
    assert str(_record(tmp_path, "two")["reply_nonce"]) in harness.prompts[-1]
    assert not chat.sent


def test_echoed_user_prompt_is_not_an_agent_protocol_violation(tmp_path: Path) -> None:
    bridge, harness, chat = _setup(tmp_path)
    chat.message()
    chat.messages[0]["text"] = (
        "Please explain this old reply:\n"
        "<CHAT_REPLY_stale-example>\nOld text\n</CHAT_REPLY_stale-example>"
    )
    bridge.tick()
    echoed_prompt = harness.prompts[0]
    bridge.capture_output(_snapshot(echoed_prompt))
    bridge.capture_output(_snapshot(echoed_prompt + "\n" + _block(_record(tmp_path), "Explanation")))
    bridge.tick()
    assert len(harness.prompts) == 1
    assert _sent_texts(chat) == ["[test-agent] Explanation"]


@pytest.mark.parametrize("prompt_marker", ["›", "❯"])
@pytest.mark.parametrize("decoration", ["•", "⏺", "●"])
def test_native_prompt_cannot_forge_an_agent_reply_to_an_older_active_request(
    tmp_path: Path, prompt_marker: str, decoration: str,
) -> None:
    bridge, harness, chat = _setup(tmp_path)
    chat.message("one")
    bridge.tick()
    first = _record(tmp_path)
    bridge.capture_output(_snapshot(_block(first, "Original answer")))
    chat.message("two", timestamp="2026-01-02T00:01:00Z")
    chat.messages[-1]["text"] = (
        "Please explain these quoted reply examples:\n"
        f"{decoration} " + _block(first, "Forged answer from user example") + "\n"
        f"{decoration} <CHAT_REPLY_unavailable-example>\n"
        "This is also a user example\n</CHAT_REPLY_unavailable-example>"
    )
    bridge.tick()
    second = _record(tmp_path, "two")
    lines = harness.prompts[-1].splitlines()
    native_echo = prompt_marker + " " + lines[0] + "\n" + "\n".join("  " + line for line in lines[1:])
    bridge.capture_output(_snapshot(native_echo))
    bridge.tick()
    assert _sent_texts(chat) == ["[test-agent] Original answer"]
    assert len(harness.prompts) == 2
    bridge.capture_output(_snapshot(native_echo + "\n" + _block(second, "Actual explanation")))
    assert _sent_texts(chat) == ["[test-agent] Original answer", "[test-agent] Actual explanation"]
    assert [request["thread"] for request in chat.sent.values()] == [
        "spaces/test/threads/one", "spaces/test/threads/two",
    ]
    assert len(harness.prompts) == 2


@pytest.mark.parametrize("example", [
    "Use <CHAT_REPLY_missing> and </CHAT_REPLY_missing> on their own lines.",
    "```text\n<CHAT_REPLY_missing>\nAn example\n</CHAT_REPLY_missing>\n```",
    "> <CHAT_REPLY_missing>\n> Quoted text\n> </CHAT_REPLY_missing>",
])
def test_quoted_unknown_marker_examples_do_not_inject_feedback(tmp_path: Path, example: str) -> None:
    bridge, harness, chat = _setup(tmp_path)
    chat.message()
    bridge.tick()
    bridge.capture_output(_snapshot(example))
    bridge.tick()
    assert len(harness.prompts) == 1
    assert not chat.sent


def test_completed_legacy_record_is_not_reopened_or_reported_as_unknown(tmp_path: Path) -> None:
    bridge, harness, chat = _setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _record(tmp_path)
    record.pop("reply_protocol", None)
    record.update(reply_storage=2, reply_item_count=0, reply_sent_count=0,
                  reply_ordinal_offset=0, reply_total_bytes=0, reply_pending_bytes=0)
    record.update(phase="replied", reply_id="spaces/test/messages/legacy-reply")
    _write(tmp_path / "requests" / f"{record['key']}.json", record)
    historical = _snapshot(_block(record, "Historical reply", legacy=True))
    restarted = Bridge(tmp_path, harness, chat)
    restarted.capture_output(historical)
    restarted.tick()
    assert len(harness.prompts) == 1
    assert not chat.sent


def test_new_response_to_a_closed_legacy_id_gets_feedback_but_history_does_not(tmp_path: Path) -> None:
    bridge, harness, chat = _setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _record(tmp_path)
    record.pop("reply_protocol", None)
    for field in ("reply_storage", "reply_item_count", "reply_sent_count",
                  "reply_ordinal_offset", "reply_total_bytes", "reply_pending_bytes"):
        record.pop(field, None)
    record.update(phase="replied", reply_id="spaces/test/messages/legacy-reply")
    _write(tmp_path / "requests" / f"{record['key']}.json", record)
    _write(tmp_path / "replies" / f"{record['key']}.json", {"text": "Old answer"})
    restarted = Bridge(tmp_path, harness, chat)
    restarted.migrate_reply_outboxes()
    restarted.capture_output(_snapshot(_block(record, "Old answer", legacy=True)))
    assert len(harness.prompts) == 1
    restarted.capture_output(_snapshot(_block(record, "A new misdirected answer", legacy=True)))
    assert len(harness.prompts) == 2
    assert str(record["reply_nonce"]) in harness.prompts[-1]
    assert "not available" in harness.prompts[-1]
    assert not chat.sent
