"""Chat boundary, restart and reply contracts through the actual durable queue."""

from __future__ import annotations

from pathlib import Path

import pytest

from agentctl.agent import Target
from agentctl.chat import Bridge, Config, _read, submit_reply
from agentctl.client import AgentPaneInfo, GuardedInput, HerdrClient
from agentctl.errors import AgentDeliveryError, HerdrUnavailable


class Harness(HerdrClient):
    def __init__(self) -> None:
        self.prompts: list[str] = []
        self.cwd = "/work/project"
        self.state = "idle"
        self.fail_confirmation = False

    def pane_info(self, pane_id: str) -> AgentPaneInfo:
        return AgentPaneInfo(pane_id, "w1", self.cwd, "codex", self.state, None, None)

    def workspace_label(self, workspace_id: str) -> str:
        return "project"

    def prompt_agent(
        self, pane_id: str, text: str, *, terminal: GuardedInput | None = None,
    ) -> None:
        self.prompts.append(text)

    def wait_agent_status(self, pane_id: str, state: str, timeout_ms: int) -> None:
        assert state == "working"
        if self.fail_confirmation:
            raise HerdrUnavailable("working transition was not observed")


class Chat:
    def __init__(self) -> None:
        self.messages: list[dict[str, object]] = []
        self.sent: dict[str, dict[str, object]] = {}
        self.calls: list[dict[str, object]] = []
        self.lose_ack = False
        self.echo_as_owner = False

    def __call__(self, request: dict[str, object]) -> dict[str, object]:
        self.calls.append(request)
        if request["action"] == "poll":
            return {"messages": self.messages, "cursor": None}
        if request["action"] == "react":
            return {"id": str(request["message"]) + "/reactions/robot"}
        request_id = str(request["request_id"])
        self.sent[request_id] = request
        reply_name = "reply-" + request_id
        reply_id = "spaces/test/messages/" + reply_name
        if self.echo_as_owner and not any(message["id"] == reply_id
                                          for message in self.messages):
            self.message(reply_name, timestamp="2026-01-02T00:00:05Z")
        if self.lose_ack:
            self.lose_ack = False
            raise OSError("reply accepted but acknowledgement lost")
        return {"id": reply_id}

    def message(self, identifier: str = "one", sender: str = "users/owner",
                timestamp: str = "2026-01-02T00:00:00Z") -> None:
        self.messages.append({"id": f"spaces/test/messages/{identifier}", "text": identifier,
                              "sender": sender, "thread": f"spaces/test/threads/{identifier}",
                              "created_at": timestamp})


def setup(state: Path) -> tuple[Bridge, Harness, Chat]:
    harness, transport = Harness(), Chat()
    target = Target(pane_id="w1:p1", expected_agent="codex",
                    expected_cwd="/work/project", expected_workspace="project")
    Bridge.initialize(state, Config("spaces/test", ("users/owner",), target, "test-agent"),
                      after="2026-01-01T00:00:00Z")
    return Bridge(state, harness, transport), harness, transport


def test_roundtrip_deduplicates_input_and_threads_reply_after_restart(tmp_path: Path) -> None:
    bridge, harness, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _read(next((tmp_path / "requests").glob("*.json")))
    assert record["phase"] == "awaiting_reply"
    submit_reply(tmp_path, str(record["key"]), "finished")
    restarted = Bridge(tmp_path, harness, chat)
    restarted.tick()
    restarted.tick()
    assert len(harness.prompts) == 1
    assert len(chat.sent) == 1
    sent = next(iter(chat.sent.values()))
    assert sent["thread"] == "spaces/test/threads/one"
    assert sent["text"] == "[test-agent] finished"


def test_lost_reply_ack_reuses_idempotency_key_without_redelivering_prompt(tmp_path: Path) -> None:
    bridge, harness, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _read(next((tmp_path / "requests").glob("*.json")))
    submit_reply(tmp_path, str(record["key"]), "finished")
    chat.lose_ack = True
    with pytest.raises(OSError, match="acknowledgement lost"):
        bridge.tick()
    Bridge(tmp_path, harness, chat).tick()
    assert len(chat.sent) == 1
    assert len(harness.prompts) == 1
    assert len([call for call in chat.calls if call["action"] == "send"]) == 2


@pytest.mark.parametrize("lose_ack", [False, True])
def test_own_reply_from_allowed_user_never_becomes_an_input(tmp_path: Path, lose_ack: bool) -> None:
    bridge, harness, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    record = _read(next((tmp_path / "requests").glob("*.json")))
    submit_reply(tmp_path, str(record["key"]), "finished")
    chat.echo_as_owner = True
    chat.lose_ack = lose_ack
    if lose_ack:
        with pytest.raises(OSError):
            bridge.tick()
    else:
        bridge.tick()
    Bridge(tmp_path, harness, chat).tick()
    assert len(harness.prompts) == 1
    assert len(list((tmp_path / "requests").glob("*.json"))) == 1


def test_only_authorized_new_messages_enter_the_coordinator(tmp_path: Path) -> None:
    bridge, harness, chat = setup(tmp_path)
    chat.message("old", timestamp="2025-12-31T23:59:59Z")
    chat.message("stranger", sender="users/stranger")
    chat.message("bot", sender="users/bot")
    chat.message("owner")
    bridge.tick()
    assert len(harness.prompts) == 1
    assert "threads" not in harness.prompts[0]
    assert "messages/owner" in harness.prompts[0]
    assert len(list((tmp_path / "requests").glob("*.json"))) == 1


def test_keyed_reply_proves_execution_after_lost_working_confirmation(tmp_path: Path) -> None:
    bridge, harness, chat = setup(tmp_path)
    harness.fail_confirmation = True
    chat.message()
    bridge.tick()
    path = next((tmp_path / "requests").glob("*.json"))
    record = _read(path)
    assert record["phase"] == "delivery_uncertain"
    submit_reply(tmp_path, str(record["key"]), "I received the request")
    bridge.tick()
    assert _read(path)["phase"] == "replied"
    assert _read(path)["delivery_confirmed_by"] == "reply_artifact"
    assert len(harness.prompts) == 1
    assert len(list((tmp_path / "queue" / "failed").glob("*.json"))) == 1


def test_busy_coordinator_keeps_requests_in_creation_order(tmp_path: Path) -> None:
    bridge, harness, chat = setup(tmp_path)
    harness.state = "working"
    chat.message("later", timestamp="2026-01-02T00:00:01Z")
    chat.message("earlier")
    bridge.tick()
    assert harness.prompts == []
    harness.state = "idle"
    bridge.tick()
    assert "messages/earlier" in harness.prompts[0]
    assert "messages/later" in harness.prompts[1]


def test_stale_target_is_refused_before_chat_access(tmp_path: Path) -> None:
    bridge, harness, chat = setup(tmp_path)
    harness.cwd = "/wrong/project"
    with pytest.raises(AgentDeliveryError, match="cwd"):
        bridge.tick()
    assert not chat.calls


def test_wrong_space_does_not_advance_checkpoint(tmp_path: Path) -> None:
    bridge, harness, chat = setup(tmp_path)
    before = _read(tmp_path / "bridge.json")
    chat.message()
    chat.messages[0]["thread"] = "spaces/other/threads/one"
    with pytest.raises(ValueError, match="outside"):
        bridge.tick()
    assert _read(tmp_path / "bridge.json") == before
    assert not harness.prompts


def test_reply_cannot_be_replaced_or_escape_request_directory(tmp_path: Path) -> None:
    bridge, _, chat = setup(tmp_path)
    chat.message()
    bridge.tick()
    key = str(_read(next((tmp_path / "requests").glob("*.json")))["key"])
    submit_reply(tmp_path, key, "first answer")
    submit_reply(tmp_path, key, "first answer")
    with pytest.raises(ValueError, match="different reply"):
        submit_reply(tmp_path, key, "changed answer")
    with pytest.raises(ValueError, match="invalid request"):
        submit_reply(tmp_path, "../escape", "answer")


def test_state_directory_symlink_is_refused(tmp_path: Path) -> None:
    real = tmp_path / "real"
    real.mkdir()
    link = tmp_path / "link"
    link.symlink_to(real, target_is_directory=True)
    with pytest.raises(AgentDeliveryError, match="unsafe"):
        setup(link)
