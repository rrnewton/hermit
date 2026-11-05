"""The installed context interface and prompt hint across real command boundaries."""

from __future__ import annotations

import hashlib
import io
import json
import os
import shlex
import stat
import subprocess
import sys
from dataclasses import asdict
from pathlib import Path
from urllib.parse import parse_qs, urlsplit
from urllib.request import Request

import pytest

import agentctl.chat as chat_module
from agentctl.agent import Target
from agentctl.chat import Bridge, Config, GoogleChatTransport, _read, _timestamp, _write, run_cli
from agentctl.jsonx import as_mapping, as_sequence


_SPACE = "spaces/test"
_THREAD = _SPACE + "/threads/original-thread"
_SOURCE = _SPACE + "/messages/incoming.message"
_TIME = "2026-01-02T00:00:00.123456789Z"
_KEY = hashlib.sha256(_SOURCE.encode()).hexdigest()
_LAUNCHER = Path(__file__).resolve().parents[1] / "bin" / "agentctl"


def _source(**fields: object) -> dict[str, object]:
    result: dict[str, object] = {
        "id": _SOURCE, "thread": _THREAD, "created_at": _TIME,
        "sender": "users/owner", "text": "Please investigate", "thread_reply": True,
    }
    result.update(fields)
    return result


def _earlier(identifier: str, stamp: str) -> dict[str, object]:
    return {"id": _SPACE + "/messages/" + identifier, "thread": _THREAD,
            "created_at": stamp, "sender": "users/participant", "text": identifier,
            "thread_reply": True}


def _saved(tmp_path: Path, *, reply_mode: str = "tagged") -> tuple[Path, Path]:
    state = tmp_path / "state 'quotes' $(touch INJECTED) ; space"
    state.mkdir(mode=0o700)
    (state / "requests").mkdir(mode=0o700)
    transcript = tmp_path / "transport-calls.jsonl"
    adapter = tmp_path / "context-adapter.py"
    adapter.write_text(
        "import json,sys\n"
        "from pathlib import Path\n"
        "request=json.load(sys.stdin)\n"
        "assert request['action']=='context'\n"
        "with Path(sys.argv[1]).open('a') as log: log.write(json.dumps(request)+'\\n')\n"
        "if request['cursor'] is None:\n"
        "    response=" + repr({"messages": [
            _earlier("nearest", "2026-01-02T00:00:00.123456788Z"),
            _earlier("older", "2026-01-01T12:00:00Z")], "cursor": "older+/= token"}) + "\n"
        "else:\n"
        "    assert request['cursor']=='older+/= token'\n"
        "    response=" + repr({"messages": [
            _earlier("oldest", "2026-01-01T00:00:00Z")], "cursor": None}) + "\n"
        "print(json.dumps(response))\n",
        encoding="utf-8",
    )
    target = Target(pane_id="missing:pane", expected_agent="codex",
                    expected_cwd="/not-a-live-workspace", expected_workspace="missing-workspace")
    config = Config(_SPACE, ("users/owner",), target, "fixture-agent",
                    transport_command=(sys.executable, str(adapter), str(transcript)), reply_mode=reply_mode)
    _write(state / "bridge.json", {
        "version": 1, "config": asdict(config),
        "after": "2026-01-01T00:00:00Z", "cursor": None,
        "high_water": "2026-01-01T00:00:00Z",
        "started_at": "2026-01-01T00:00:00Z",
    })
    record: dict[str, object] = {
        "key": _KEY, "message": _source(), "phase": "awaiting_reply",
        "queue_id": "00000000000000000000-" + _KEY,
        "request_id": "00000000-0000-0000-0000-000000000001",
        "received_at": _TIME,
    }
    if reply_mode == "tagged":
        record["reply_nonce"] = "a" * 22
    _write(state / "requests" / (_KEY + ".json"), record)
    return state, transcript


def _snapshot(state: Path) -> dict[str, tuple[int, int, bytes | None]]:
    return {str(path.relative_to(state)): (stat.S_IMODE(path.stat().st_mode),
            path.stat().st_mtime_ns, path.read_bytes() if path.is_file() else None)
            for path in [state, *state.rglob("*")]}


def _environment(tmp_path: Path) -> tuple[dict[str, str], Path]:
    bin_dir = tmp_path / "command-bin"
    bin_dir.mkdir()
    (bin_dir / "python3").symlink_to(sys.executable)
    called = tmp_path / "herdr-must-not-run"
    herdr = bin_dir / "herdr"
    herdr.write_text(f"#!{sys.executable}\nfrom pathlib import Path\n"
                     f"Path({str(called)!r}).touch()\nraise SystemExit(99)\n", encoding="utf-8")
    herdr.chmod(0o700)
    environment = dict(os.environ)
    environment["PATH"] = str(bin_dir)
    return environment, called


def test_real_context_cli_reads_prefix_and_older_page_without_herdr_or_state_changes(tmp_path: Path) -> None:
    state, transcript = _saved(tmp_path)
    before = _snapshot(state)
    environment, herdr_called = _environment(tmp_path)
    command = [str(_LAUNCHER), "chat", "context", "--state", str(state), "--request", _KEY[:12]]
    first = subprocess.run(command, capture_output=True, text=True, env=environment, cwd=tmp_path, timeout=15)
    assert first.returncode == 0, first.stderr
    result = as_mapping(json.loads(first.stdout), "context")
    rows = as_sequence(result["messages"], "messages")
    assert [as_mapping(row, "message")["text"] for row in rows] == ["older", "nearest"]
    assert result["source"] == _SOURCE
    assert result["thread"] == _THREAD
    assert result["before"] == _TIME
    assert result["order"] == "chronological"
    assert result["cursor"] == "older+/= token"
    second = subprocess.run([*command, "--cursor", str(result["cursor"])], capture_output=True,
                            text=True, env=environment, cwd=tmp_path, timeout=15)
    assert second.returncode == 0, second.stderr
    assert as_mapping(json.loads(second.stdout), "context")["cursor"] is None
    calls = [json.loads(line) for line in transcript.read_text().splitlines()]
    assert calls == [
        {"action": "context", "space": _SPACE, "thread": _THREAD, "before": _TIME, "limit": 10, "cursor": None},
        {"action": "context", "space": _SPACE, "thread": _THREAD, "before": _TIME, "limit": 10,
         "cursor": "older+/= token"},
    ]
    assert _snapshot(state) == before
    assert not herdr_called.exists()


def test_context_does_not_construct_a_harness_client(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str],
) -> None:
    state, _ = _saved(tmp_path)
    before = _snapshot(state)

    def fail_client() -> None:
        pytest.fail("history is a transport read and must not construct a Herdr client")

    monkeypatch.setattr(chat_module, "HerdrClient", fail_client)
    assert run_cli(["context", "--state", str(state), "--request", _KEY]) == 0
    assert json.loads(capsys.readouterr().out)["source"] == _SOURCE
    assert _snapshot(state) == before


@pytest.mark.parametrize("reply_mode", ["tagged", "file"])
def test_thread_hint_is_shell_safe_and_runs_the_source_checkout_command(
    tmp_path: Path, reply_mode: str,
) -> None:
    state, transcript = _saved(tmp_path, reply_mode=reply_mode)
    bridge = Bridge(state)
    record = _read(state / "requests" / (_KEY + ".json"))
    prompt = bridge._prompt(record)
    heading = "Run this for the prior 10 messages in this thread; read further back if needed:"
    command = prompt.split(heading + "\n", 1)[1].splitlines()[0]
    assert shlex.split(command) == [
        str(_LAUNCHER), "chat", "context", "--state", str(state), "--request", _KEY[:12], "--limit", "10",
    ]
    assert _LAUNCHER.is_file() and os.access(_LAUNCHER, os.X_OK)
    environment, called = _environment(tmp_path)
    before = _snapshot(state)
    result = subprocess.run(["/bin/sh", "-c", command], cwd=tmp_path, env=environment,
                            capture_output=True, text=True, timeout=15)
    assert result.returncode == 0, result.stderr
    assert json.loads(result.stdout)["source"] == _SOURCE
    assert transcript.exists()
    assert not (tmp_path / "INJECTED").exists()
    assert not called.exists()
    assert _snapshot(state) == before
    assert ("PATH_TO_YOUR_REPLY" in prompt) is (reply_mode == "file")


@pytest.mark.parametrize("reply_mode", ["tagged", "file"])
@pytest.mark.parametrize("flag", [False, None])
def test_root_or_unknown_thread_metadata_has_no_context_hint(
    tmp_path: Path, reply_mode: str, flag: bool | None,
) -> None:
    state, transcript = _saved(tmp_path, reply_mode=reply_mode)
    bridge = Bridge(state)
    record = _read(state / "requests" / (_KEY + ".json"))
    message = as_mapping(record["message"], "message")
    if flag is None:
        message.pop("thread_reply")
    else:
        message["thread_reply"] = flag
    record["message"] = message
    prompt = bridge._prompt(record)
    assert "prior 10 messages" not in prompt
    assert " chat context " not in prompt
    assert not transcript.exists()


@pytest.mark.parametrize(("change", "diagnostic"), [
    ("ambiguous", "exactly one"), ("short-prefix", "at least 12"), ("non-hex", "hexadecimal"),
    ("missing-prefix", "exactly one"), ("outside-space", "different configured space"),
    ("outside-thread", "selected space"), ("malformed-bool", "boolean"), ("wrong-key", "does not match"),
    ("invalid-limit", "limit"),
])
def test_cli_rejects_unbound_or_malformed_requests_before_transport(
    tmp_path: Path, change: str, diagnostic: str,
) -> None:
    state, transcript = _saved(tmp_path)
    request_path = state / "requests" / (_KEY + ".json")
    record = _read(request_path)
    source = as_mapping(record["message"], "source")
    prefix = _KEY[:12]
    extra: list[str] = []
    if change == "ambiguous":
        other = _KEY[:12] + ("0" if _KEY[12] != "0" else "1") + _KEY[13:]
        _write(state / "requests" / (other + ".json"), {"key": other, "message": source})
    elif change == "short-prefix":
        prefix = _KEY[:11]
    elif change == "non-hex":
        prefix = "../" + _KEY
    elif change == "missing-prefix":
        prefix = "f" * 64
    elif change == "outside-space":
        source.update(id="spaces/other/messages/one", thread="spaces/other/threads/one")
    elif change == "outside-thread":
        source["thread"] = "spaces/other/threads/one"
    elif change == "malformed-bool":
        source["thread_reply"] = "true"
    elif change == "wrong-key":
        record["key"] = "f" * 64
    elif change == "invalid-limit":
        extra = ["--limit", "201"]
    record["message"] = source
    _write(request_path, record)
    before = _snapshot(state)
    environment, called = _environment(tmp_path)
    result = subprocess.run([str(_LAUNCHER), "chat", "context", "--state", str(state),
                             "--request", prefix, *extra], cwd=tmp_path, env=environment,
                            capture_output=True, text=True, timeout=15)
    assert result.returncode != 0
    assert diagnostic in result.stderr
    assert result.stdout == ""
    assert not transcript.exists()
    assert not called.exists()
    assert _snapshot(state) == before


class Http:
    def __init__(self, document: dict[str, object]) -> None:
        self.document = document
        self.requests: list[Request] = []

    def __call__(self, request: Request, *, timeout: float) -> io.BytesIO:
        assert timeout == 45
        self.requests.append(request)
        return io.BytesIO(json.dumps(self.document).encode())


@pytest.mark.parametrize(("stamp", "microsecond"), [
    ("2026-01-02T00:00:00.1Z", 100_000),
    ("2026-01-02T00:00:00.123456Z", 123_456),
    ("2026-01-02T01:00:00.123456789+01:00", 123_456),
])
def test_chat_timestamp_accepts_rfc3339_fraction_precision_on_minimum_python(
    stamp: str, microsecond: int,
) -> None:
    parsed = _timestamp(stamp)
    assert parsed.microsecond == microsecond
    assert parsed.tzinfo is not None


def test_chat_timestamp_refuses_more_than_rfc3339_nanosecond_precision() -> None:
    with pytest.raises(ValueError, match="at most nine fractional digits"):
        _timestamp("2026-01-02T00:00:00.1234567890Z")


@pytest.mark.parametrize("action", ["poll", "context"])
@pytest.mark.parametrize("reply", [True, False, None])
def test_public_rest_normalizes_thread_metadata_and_empty_cursor(
    monkeypatch: pytest.MonkeyPatch, action: str, reply: bool | None,
) -> None:
    raw: dict[str, object] = {
        "name": _SOURCE, "thread": {"name": _THREAD}, "sender": {"name": "users/owner"},
        "text": "request", "createTime": _TIME,
    }
    if reply is not None:
        raw["threadReply"] = reply
    http = Http({"messages": [raw], "nextPageToken": ""})
    monkeypatch.setattr(chat_module, "urlopen", http)
    monkeypatch.setenv("CONTEXT_TEST_TOKEN", "fixture-token")
    request: dict[str, object] = {"action": action, "space": _SPACE, "cursor": "opaque+/= page"}
    if action == "poll":
        request["after"] = _TIME
    else:
        request.update(thread=_THREAD, before=_TIME, limit=10)
    result = GoogleChatTransport("CONTEXT_TEST_TOKEN")(request)
    assert result["cursor"] is None
    row = as_mapping(as_sequence(result["messages"], "messages")[0], "message")
    expected: dict[str, object] = {
        "id": _SOURCE, "thread": _THREAD, "sender": "users/owner",
        "text": "request", "created_at": _TIME,
    }
    if reply is not None:
        expected["thread_reply"] = reply
    assert row == expected
    sent = http.requests[0]
    parsed = urlsplit(sent.full_url)
    assert (parsed.scheme, parsed.netloc, parsed.path) == (
        "https", "chat.googleapis.com", "/v1/spaces/test/messages",
    )
    assert sent.get_method() == "GET" and sent.data is None
    assert parse_qs(parsed.query) == (
        {"pageSize": ["10"], "orderBy": ["createTime DESC"], "pageToken": ["opaque+/= page"],
         "filter": [f'createTime < "{_TIME}" AND thread.name = {_THREAD}']}
        if action == "context" else
        {"pageSize": ["100"], "orderBy": ["createTime asc"], "pageToken": ["opaque+/= page"],
         "filter": [f'createTime > "{_TIME}"']}
    )


@pytest.mark.parametrize("action", ["poll", "context"])
@pytest.mark.parametrize("value", ["true", 1, None])
def test_public_rest_rejects_malformed_reply_metadata(
    monkeypatch: pytest.MonkeyPatch, action: str, value: object,
) -> None:
    http = Http({"messages": [{
        "name": _SOURCE, "thread": {"name": _THREAD}, "sender": {"name": "users/owner"},
        "createTime": _TIME, "threadReply": value,
    }]})
    monkeypatch.setattr(chat_module, "urlopen", http)
    monkeypatch.setenv("CONTEXT_TEST_TOKEN", "fixture-token")
    request: dict[str, object] = {
        "action": action, "space": _SPACE, "after": _TIME, "thread": _THREAD, "before": _TIME, "limit": 10,
    }
    with pytest.raises(ValueError, match="threadReply must be a boolean"):
        GoogleChatTransport("CONTEXT_TEST_TOKEN")(request)


def test_ingestion_refuses_malformed_reply_metadata_without_queuing(tmp_path: Path) -> None:
    state, _ = _saved(tmp_path)

    def upstream(request: dict[str, object]) -> dict[str, object]:
        assert request["action"] == "poll"
        return {"messages": [_source(thread_reply="true")], "cursor": None}

    bridge = Bridge(state, transport=upstream)
    before = _snapshot(state)
    with pytest.raises(ValueError, match="thread_reply must be a boolean"):
        bridge._ingest({"after": "2026-01-01T00:00:00Z", "high_water": "2026-01-01T00:00:00Z",
                        "started_at": "2026-01-01T00:00:00Z", "cursor": None})
    assert _snapshot(state) == before
