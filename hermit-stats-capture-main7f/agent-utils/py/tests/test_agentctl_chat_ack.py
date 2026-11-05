"""Durable intake ACKs, bounded failures, and public REST reaction reconciliation."""

from __future__ import annotations

import io
import json
import uuid
from dataclasses import asdict
from datetime import datetime, timedelta, timezone, tzinfo
from pathlib import Path
from urllib.parse import parse_qs, urlsplit
from urllib.request import Request

import pytest

import agentctl.chat as chat_module
from agentctl.agent import Target
from agentctl.chat import Bridge, CommandTransport, Config, GoogleChatTransport, _read, _write, submit_reply
from agentctl.jsonx import as_mapping
from tests.test_herdr_chat import Harness


_START = "2026-01-01T00:00:00Z"
_MESSAGE = "spaces/test/messages/one"
_REACTION = _MESSAGE + "/reactions/robot"


def _message(identifier: str = "one", *, sender: str = "users/owner",
             text: str = "Please investigate", created: str = "2026-01-02T00:00:00Z") -> dict[str, object]:
    return {"id": f"spaces/test/messages/{identifier}", "thread": "spaces/test/threads/one",
            "sender": sender, "text": text, "created_at": created}


class Chat:
    def __init__(self, state: Path, harness: Harness) -> None:
        self.state, self.harness = state, harness
        self.messages = [_message()]
        self.calls: list[dict[str, object]] = []
        self.reactions: dict[str, str] = {}
        self.fail = False
        self.lose_response = False

    def __call__(self, request: dict[str, object]) -> dict[str, object]:
        self.calls.append(request)
        action = request["action"]
        if action == "poll":
            return {"messages": self.messages, "cursor": None}
        if action == "react":
            # No ACK may escape before the source record and attempt are durable.
            records = [_read(path) for path in (self.state / "requests").glob("*.json")]
            source = next(record for record in records if as_mapping(record["message"], "message")["id"] == request["message"])
            ack = as_mapping(source["ack"], "ack")
            assert ack["state"] == "pending"
            assert isinstance(ack["attempts"], int) and ack["attempts"] > 0
            assert ack["next_retry_at"]
            if self.fail:
                raise OSError("reaction permission denied")
            key = str(request["request_id"])
            self.reactions[key] = str(request["message"]) + "/reactions/robot"
            if self.lose_response:
                self.lose_response = False
                raise OSError("reaction accepted; response lost")
            return {"id": self.reactions[key]}
        assert action == "send"
        return {"id": "spaces/test/messages/final"}


def _config(**fields: object) -> Config:
    target = Target(pane_id="w1:p1", expected_agent="codex",
                    expected_cwd="/work/project", expected_workspace="project")
    document = as_mapping(json.loads(json.dumps(asdict(Config("spaces/test", ("users/owner",), target, "test-agent")))), "config")
    document.update(fields)
    return Config.parse(document)


def _setup(state: Path, **fields: object) -> tuple[Bridge, Harness, Chat]:
    harness = Harness()
    chat = Chat(state, harness)
    Bridge.initialize(state, _config(**fields), after=_START)
    return Bridge(state, harness, chat), harness, chat


def _record(state: Path) -> dict[str, object]:
    return _read(next((state / "requests").glob("*.json")))


def _ack(state: Path) -> dict[str, object]:
    return as_mapping(_record(state)["ack"], "ack")


@pytest.fixture
def clock(monkeypatch: pytest.MonkeyPatch) -> list[datetime]:
    times = [datetime(2026, 1, 3, tzinfo=timezone.utc)]

    class Clock(datetime):
        @classmethod
        def now(cls, tz: tzinfo | None = None) -> Clock:
            return cls.fromtimestamp(times[0].timestamp(), tz=tz)

    monkeypatch.setattr(chat_module, "datetime", Clock)
    return times


def test_default_robot_ack_is_durable_and_precedes_prompt(tmp_path: Path) -> None:
    bridge, harness, chat = _setup(tmp_path)
    observations: list[int] = []
    original = chat.__call__

    def transport(request: dict[str, object]) -> dict[str, object]:
        if request["action"] == "react":
            observations.append(len(harness.prompts))
        return original(request)

    bridge.transport = transport
    result = bridge.tick()
    assert observations == [0]
    assert len(harness.prompts) == 1
    ack = _ack(tmp_path)
    assert ack["state"] == "acked"
    assert ack["emoji"] == "🤖"
    assert ack["attempts"] == 1
    assert ack["error"] is None
    assert ack["reaction_id"] == _REACTION
    assert ack["next_retry_at"] is None
    assert result["ack_reaction"] == "🤖"
    request = next(call for call in chat.calls if call["action"] == "react")
    assert request == {"action": "react", "space": "spaces/test", "message": _MESSAGE,
                       "emoji": "🤖", "request_id": ack["request_id"]}
    assert uuid.UUID(str(request["request_id"]))
    assert request["request_id"] != _record(tmp_path)["request_id"]


@pytest.mark.parametrize("emoji", ["😎", "👩🏽‍💻", None, ""])
def test_custom_or_disabled_ack(tmp_path: Path, emoji: str | None) -> None:
    bridge, harness, chat = _setup(tmp_path, ack_reaction=emoji)
    bridge.tick()
    assert len(harness.prompts) == 1
    reactions = [call for call in chat.calls if call["action"] == "react"]
    if emoji:
        assert len(reactions) == 1 and reactions[0]["emoji"] == emoji
        assert _ack(tmp_path)["state"] == "acked"
    else:
        assert reactions == []
        assert _ack(tmp_path)["state"] == "disabled"
        assert _ack(tmp_path)["attempts"] == 0


@pytest.mark.parametrize("value", [False, 1, [], " ", "🤖\n", "x" * 129])
def test_invalid_ack_configuration_is_refused(value: object) -> None:
    with pytest.raises(ValueError, match="ack_reaction"):
        _config(ack_reaction=value)


def test_legacy_configuration_defaults_to_robot() -> None:
    document = as_mapping(json.loads(json.dumps(asdict(_config()))), "config")
    del document["ack_reaction"]
    del document["reaction_user"]
    del document["outbound_mode"]
    parsed = Config.parse(document)
    assert parsed.ack_reaction == "🤖" and parsed.outbound_mode == "enabled"


@pytest.mark.parametrize("value", [None, "", "read-only", False, 1])
def test_invalid_outbound_mode_is_refused(value: object) -> None:
    with pytest.raises(ValueError, match="outbound_mode"):
        _config(outbound_mode=value)


def test_outbound_disabled_persists_input_but_refuses_every_chat_write(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    bridge, harness, chat = _setup(tmp_path, outbound_mode="disabled")
    status = bridge.tick()
    record = _record(tmp_path)
    key = str(record["key"])

    assert [call["action"] for call in chat.calls] == ["poll"]
    assert record["phase"] == "awaiting_reply"
    assert "reply_nonce" not in record
    assert as_mapping(record["ack"], "ack")["state"] == "disabled"
    assert as_mapping(record["ack"], "ack")["emoji"] is None
    assert status["outbound_mode"] == "disabled" and status["ack_reaction"] is None
    assert len(harness.prompts) == 1
    assert "Outbound Chat is disabled" in harness.prompts[0]
    assert "no Chat reply will be published" in harness.prompts[0]
    assert "<CHAT_REPLY_" not in harness.prompts[0] and " --file " not in harness.prompts[0]
    with pytest.raises(ValueError, match="reply artifacts are refused"):
        submit_reply(tmp_path, key, "must not be saved")
    assert list((tmp_path / "replies").glob("*.json")) == []
    assert list((tmp_path / "replies" / "items").rglob("*.json")) == []
    assert list((tmp_path / "submissions").glob("*.json")) == []

    # Corrupt or legacy pending-looking state must not bypass the mode.
    ack = as_mapping(record["ack"], "ack")
    ack.update(state="pending", emoji="🤖", error="retry", next_retry_at=None)
    record.update(phase="reply_pending", ack=ack, reply_nonce="A" * 22, reply_protocol=2)
    _write(tmp_path / "requests" / f"{key}.json", record)
    _write(tmp_path / "replies" / f"{key}.json", {
        "text": "preexisting", "items": [{
            "reply_key": "0", "request_id": record["request_id"], "text": "preexisting",
        }],
    })
    bridge.tick()
    assert set(call["action"] for call in chat.calls) == {"poll"}
    assert as_mapping(_record(tmp_path)["ack"], "ack")["state"] == "disabled"
    assert bridge.output_requests(retry_failed=True) == {}
    monkeypatch.setattr(bridge, "open_output",
                        lambda nonces: (_ for _ in ()).throw(AssertionError("capture subscribed")))
    assert bridge.capture_once() == {"captured": [], "errors": []}
    for action in ("react", "send"):
        before = len(chat.calls)
        with pytest.raises(ValueError, match="outbound Chat is disabled"):
            bridge._transport_request({"action": action})
        assert len(chat.calls) == before


def test_ack_only_for_authorized_new_nonempty_messages(tmp_path: Path) -> None:
    bridge, _, chat = _setup(tmp_path)
    chat.messages += [_message("stranger", sender="users/stranger"),
                      _message("empty", text=""),
                      _message("old", created="2025-12-31T00:00:00Z")]
    bridge.tick()
    assert [call["message"] for call in chat.calls if call["action"] == "react"] == [_MESSAGE]
    assert len(list((tmp_path / "requests").glob("*.json"))) == 1


def test_busy_agent_receives_ack_without_waiting_for_readiness(tmp_path: Path) -> None:
    bridge, harness, chat = _setup(tmp_path)
    harness.state = "working"
    bridge.tick()
    assert _record(tmp_path)["phase"] == "queued"
    assert _ack(tmp_path)["state"] == "acked"
    assert not harness.prompts
    harness.state = "idle"
    Bridge(tmp_path, harness, chat).tick()
    assert len(harness.prompts) == 1
    assert len([call for call in chat.calls if call["action"] == "react"]) == 1


def test_ack_failure_retries_after_restart_without_blocking_prompt_or_reply(
    tmp_path: Path, clock: list[datetime],
) -> None:
    bridge, harness, chat = _setup(tmp_path)
    chat.fail = True
    bridge.tick()
    assert len(harness.prompts) == 1
    ack = _ack(tmp_path)
    assert ack["state"] == "pending"
    assert ack["attempts"] == 1
    assert ack["error"] == "reaction permission denied"
    assert ack["next_retry_at"] == "2026-01-03T00:00:03Z"
    submit_reply(tmp_path, str(_record(tmp_path)["key"]), "Finished")
    bridge.tick()
    assert _record(tmp_path)["phase"] == "replied"
    assert _ack(tmp_path)["attempts"] == 1
    assert len([call for call in chat.calls if call["action"] == "send"]) == 1
    clock[0] += timedelta(seconds=3)
    chat.fail = False
    Bridge(tmp_path, harness, chat).tick()
    assert _ack(tmp_path)["state"] == "acked"
    assert _ack(tmp_path)["attempts"] == 2
    assert _ack(tmp_path)["error"] is None
    assert len(harness.prompts) == 1
    assert len(chat.reactions) == 1
    requests = [call for call in chat.calls if call["action"] == "react"]
    assert requests[0] == requests[1]


def test_lost_ack_response_reuses_identity_after_restart(
    tmp_path: Path, clock: list[datetime],
) -> None:
    bridge, harness, chat = _setup(tmp_path)
    chat.lose_response = True
    bridge.tick()
    assert _ack(tmp_path)["state"] == "pending"
    assert len(chat.reactions) == len(harness.prompts) == 1
    clock[0] += timedelta(seconds=3)
    restarted = Bridge(tmp_path, harness, chat)
    restarted.tick()
    restarted.tick()
    assert _ack(tmp_path)["state"] == "acked"
    assert len(chat.reactions) == len(harness.prompts) == 1
    requests = [call for call in chat.calls if call["action"] == "react"]
    assert len(requests) == 2
    assert requests[0] == requests[1]


def test_ack_attempt_survives_process_exit_before_response(
    tmp_path: Path, clock: list[datetime],
) -> None:
    bridge, harness, chat = _setup(tmp_path)

    def crash(request: dict[str, object]) -> dict[str, object]:
        if request["action"] == "react":
            raise SystemExit("simulated bridge process exit")
        return chat(request)

    bridge.transport = crash
    with pytest.raises(SystemExit):
        bridge.tick()
    ack = _ack(tmp_path)
    assert ack["state"] == "pending"
    assert ack["attempts"] == 1
    assert ack["next_retry_at"]
    clock[0] += timedelta(seconds=3)
    Bridge(tmp_path, harness, chat).tick()
    assert _ack(tmp_path)["attempts"] == 2
    assert _ack(tmp_path)["state"] == "acked"
    assert len(harness.prompts) == 1


def test_wrong_reaction_resource_stays_pending_without_blocking_prompt(tmp_path: Path) -> None:
    bridge, harness, chat = _setup(tmp_path)

    def wrong_message(request: dict[str, object]) -> dict[str, object]:
        if request["action"] == "react":
            return {"id": "spaces/test/messages/other/reactions/robot"}
        return chat(request)

    bridge.transport = wrong_message
    bridge.tick()
    assert len(harness.prompts) == 1
    assert _ack(tmp_path)["state"] == "pending"
    assert "outside" in str(_ack(tmp_path)["error"])


@pytest.mark.parametrize("complete", [False, True])
def test_restart_upgrades_unfinished_legacy_records_without_reacting_to_old_final_answers(
    tmp_path: Path, complete: bool,
) -> None:
    bridge, harness, chat = _setup(tmp_path)
    harness.state = "working"
    bridge._ingest(_read(tmp_path / "bridge.json"))
    path = next((tmp_path / "requests").glob("*.json"))
    record = _read(path)
    del record["ack"]
    if complete:
        record.update(phase="replied", reply_id="spaces/test/messages/old-final")
    _write(path, record)
    Bridge(tmp_path, harness, chat).tick()
    assert _ack(tmp_path)["state"] == ("disabled" if complete else "acked")
    assert len(chat.reactions) == (0 if complete else 1)


def test_identical_final_reply_retry_after_posting_succeeds(tmp_path: Path) -> None:
    bridge, _, chat = _setup(tmp_path)
    bridge.tick()
    key = str(_record(tmp_path)["key"])
    submit_reply(tmp_path, key, "Finished")
    bridge.tick()
    assert _record(tmp_path)["phase"] == "replied"
    submit_reply(tmp_path, key, "Finished")
    with pytest.raises(ValueError, match="different reply"):
        submit_reply(tmp_path, key, "Different")
    bridge.tick()
    assert len([call for call in chat.calls if call["action"] == "send"]) == 1


def _react() -> dict[str, object]:
    return {"action": "react", "space": "spaces/test", "message": _MESSAGE,
            "emoji": "🤖", "request_id": "f17fb68a-5597-49a9-a1ab-d14b26331b0e"}


class Http:
    def __init__(self, *responses: dict[str, object] | OSError) -> None:
        self.responses = list(responses)
        self.requests: list[Request] = []

    def __call__(self, request: Request, *, timeout: float) -> io.BytesIO:
        assert timeout == 45
        self.requests.append(request)
        response = self.responses.pop(0)
        if isinstance(response, OSError):
            raise response
        return io.BytesIO(json.dumps(response).encode())


def test_rest_reaction_posts_unicode_to_message_without_invented_request_id(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    http = Http({"name": _REACTION})
    monkeypatch.setenv("CHAT_TEST_TOKEN", "fixture-token")
    monkeypatch.setattr(chat_module, "urlopen", http)
    assert GoogleChatTransport("CHAT_TEST_TOKEN")(_react()) == {"id": _REACTION}
    request = http.requests[0]
    assert request.full_url == f"https://chat.googleapis.com/v1/{_MESSAGE}/reactions"
    assert request.get_method() == "POST"
    assert request.get_header("Authorization") == "Bearer fixture-token"
    assert isinstance(request.data, bytes)
    assert json.loads(request.data) == {"emoji": {"unicode": "🤖"}}


def test_rest_reconciles_same_user_emoji_after_lost_create_response(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    http = Http({"reactions": []}, OSError("create response lost"), {"reactions": [{
        "name": _REACTION, "user": {"name": "users/oauth-owner"}, "emoji": {"unicode": "🤖"},
    }]})
    monkeypatch.setenv("CHAT_TEST_TOKEN", "fixture-token")
    monkeypatch.setattr(chat_module, "urlopen", http)
    transport = GoogleChatTransport("CHAT_TEST_TOKEN", reaction_user="users/oauth-owner")
    with pytest.raises(OSError, match="lost"):
        transport(_react())
    restarted = GoogleChatTransport("CHAT_TEST_TOKEN", reaction_user="users/oauth-owner")
    assert restarted(_react()) == {"id": _REACTION}
    assert [request.get_method() for request in http.requests] == ["GET", "POST", "GET"]
    for request in (http.requests[0], http.requests[2]):
        assert parse_qs(urlsplit(request.full_url).query) == {
            "pageSize": ["200"], "filter": ['emoji.unicode = "🤖" AND user.name = "users/oauth-owner"'],
        }


def test_rest_does_not_accept_somebody_elses_reaction(monkeypatch: pytest.MonkeyPatch) -> None:
    http = Http({"reactions": [{"name": _MESSAGE + "/reactions/other",
        "user": {"name": "users/other"}, "emoji": {"unicode": "🤖"}}]}, {"name": _REACTION})
    monkeypatch.setenv("CHAT_TEST_TOKEN", "fixture-token")
    monkeypatch.setattr(chat_module, "urlopen", http)
    assert GoogleChatTransport("CHAT_TEST_TOKEN", reaction_user="users/oauth-owner")(_react()) == {"id": _REACTION}
    assert [request.get_method() for request in http.requests] == ["GET", "POST"]


@pytest.mark.parametrize("value", ["users/me", "users/app", "users/owner/../other", 7])
def test_reaction_user_requires_an_explicit_canonical_user(value: object) -> None:
    with pytest.raises(ValueError, match="reaction_user"):
        _config(reaction_user=value)


def test_rest_does_not_create_when_reconciliation_is_incomplete(monkeypatch: pytest.MonkeyPatch) -> None:
    http = Http({"reactions": [], "nextPageToken": "more"})
    monkeypatch.setenv("CHAT_TEST_TOKEN", "fixture-token")
    monkeypatch.setattr(chat_module, "urlopen", http)
    with pytest.raises(ValueError, match="pagination"):
        GoogleChatTransport("CHAT_TEST_TOKEN", reaction_user="users/oauth-owner")(_react())
    assert [request.get_method() for request in http.requests] == ["GET"]


@pytest.mark.parametrize(("field", "value"), [
    ("message", "spaces/other/messages/one"), ("message", _MESSAGE + "/../two"),
    ("emoji", ""), ("request_id", "invalid"),
])
def test_rest_reaction_validates_authority_before_http(
    monkeypatch: pytest.MonkeyPatch, field: str, value: str,
) -> None:
    http = Http()
    monkeypatch.setenv("CHAT_TEST_TOKEN", "fixture-token")
    monkeypatch.setattr(chat_module, "urlopen", http)
    request = _react()
    request[field] = value
    with pytest.raises(ValueError):
        GoogleChatTransport("CHAT_TEST_TOKEN")(request)
    assert not http.requests


def test_rest_rejects_reaction_on_another_message(monkeypatch: pytest.MonkeyPatch) -> None:
    http = Http({"name": "spaces/test/messages/other/reactions/robot"})
    monkeypatch.setenv("CHAT_TEST_TOKEN", "fixture-token")
    monkeypatch.setattr(chat_module, "urlopen", http)
    with pytest.raises(ValueError, match="outside"):
        GoogleChatTransport("CHAT_TEST_TOKEN")(_react())


def test_command_adapter_react_contract() -> None:
    import sys
    script = "import json,sys; r=json.load(sys.stdin); print(json.dumps({'id':r['message']+'/reactions/robot','request':r}))"
    assert CommandTransport((sys.executable, "-c", script))(_react()) == {"id": _REACTION, "request": _react()}


@pytest.mark.parametrize("command", ["launch", "init", "tick", "run", "status", "reply", "close", "quickstart", "userguide"])
def test_each_chat_subcommand_has_specific_help(command: str, capsys: pytest.CaptureFixture[str]) -> None:
    with pytest.raises(SystemExit) as result:
        chat_module.run_cli([command, "--help"])
    assert result.value.code == 0
    text = capsys.readouterr().out
    assert f"agentctl chat {command}" in text
    assert "Example:" in text
    if command in ("launch", "run"):
        assert "0.1–86400" in text
    else:
        assert "--interval" not in text
    if command not in ("reply", "close"):
        assert "--request" not in text


def test_init_and_reply_required_options_are_parser_errors() -> None:
    for arguments in (["launch"], ["init"], ["reply", "--file", "answer.txt"]):
        with pytest.raises(SystemExit) as result:
            chat_module.run_cli(arguments)
        assert result.value.code == 2


def test_quickstart_explains_ack_configuration(capsys: pytest.CaptureFixture[str]) -> None:
    assert chat_module.run_cli(["quickstart"]) == 0
    text = capsys.readouterr().out
    assert "ack_reaction" in text and "🤖" in text
    assert "reaction_user" in text
    assert "outbound_mode" in text and "disabled" in text
    assert "Herdr" in text


def test_state_flag_works_before_or_after_command_and_legacy_default_is_preserved(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path,
) -> None:
    states: list[Path] = []

    class StatusBridge:
        def __init__(self, state: Path) -> None:
            states.append(state)

        def status(self) -> dict[str, object]:
            return {}

    monkeypatch.setattr(chat_module, "Bridge", StatusBridge)
    assert chat_module.run_cli(["--state", str(tmp_path), "status"]) == 0
    assert chat_module.run_cli(["status", "--state", str(tmp_path)]) == 0
    assert chat_module.run_cli(["status"]) == 0
    assert chat_module.main(["status"]) == 0
    assert states == [tmp_path, tmp_path, Path(".agentctl/.chat"), Path(".herdr-chat")]
