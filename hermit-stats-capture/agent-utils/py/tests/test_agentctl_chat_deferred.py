"""Possible reply echoes must be valid durable input before a cursor advances."""

from __future__ import annotations

import hashlib
import threading
from collections.abc import Iterator
from contextlib import contextmanager
from pathlib import Path

import pytest

import agentctl.chat_runtime as runtime_module
from agentctl.chat import _read, _write, submit_reply
from agentctl.chat_runtime import _Completion, _Runtime
from tests.test_herdr_chat import setup


_SPACE = "spaces/test"
_SOURCE = _SPACE + "/messages/source"


def _message(identifier: str, **fields: object) -> dict[str, object]:
    message: dict[str, object] = {
        "id": identifier, "thread": _SPACE + "/threads/one", "sender": "users/owner",
        "text": "Inspect the change", "created_at": "2026-01-02T00:00:00Z", "thread_reply": True,
    }
    message.update(fields)
    return message


@contextmanager
def _pending_reply(state: Path) -> Iterator[tuple[_Runtime, dict[str, object]]]:
    bridge, _, _ = setup(state)
    bridge._ingest_result({"messages": [_message(_SOURCE)]})
    key = hashlib.sha256(_SOURCE.encode()).hexdigest()
    path = state / "requests" / f"{key}.json"
    record = _read(path)
    record["phase"] = "awaiting_reply"
    _write(path, record)
    submit_reply(state, key, "Completed")
    record["phase"] = "reply_pending"
    _write(path, record)
    runtime = _Runtime(bridge, 300, "test-chat", threading.Event())
    runtime._input_event({"type": "checkpoint", "cursor": "last-valid"})
    try:
        reply = bridge._next_reply(runtime.reply_items_by_key[key])
        assert reply is not None
        yield runtime, _message(_SPACE + "/messages/echo", text=runtime._outbound_text(reply))
    finally:
        runtime.stop.set()
        runtime.local.close()
        for pool in runtime.pools.values():
            pool.shutdown(wait=True, cancel_futures=True)


@pytest.mark.parametrize("fields", [
    {"thread": "spaces/other/threads/one"},
    {"created_at": "not-a-timestamp"},
    {"created_at": "2026-01-02T00:00:00"},
    {"thread_reply": "true"},
    {"thread_reply": 1},
])
def test_invalid_echo_is_neither_deferred_nor_checkpointed(
    tmp_path: Path, fields: dict[str, object],
) -> None:
    with _pending_reply(tmp_path) as (runtime, message):
        message.update(fields)
        before_input = _read(tmp_path / "input.json")
        before_poll = _read(tmp_path / "bridge.json")
        assert runtime._possible_echo(message)
        with pytest.raises((ValueError, TypeError)):
            runtime._input_event({"type": "message", "message": message, "cursor": "invalid-position"})
        assert _read(tmp_path / "input.json") == before_input
        assert _read(tmp_path / "bridge.json") == before_poll
        assert list(runtime.deferred.glob("*.json")) == []
        assert len(list((tmp_path / "requests").glob("*.json"))) == 1


@pytest.mark.parametrize("fields", [
    {"thread": "spaces/other/threads/one"},
    {"created_at": "not-a-timestamp"},
    {"thread_reply": "true"},
])
def test_reconciliation_does_not_checkpoint_or_defer_invalid_echo(
    tmp_path: Path, fields: dict[str, object],
) -> None:
    with _pending_reply(tmp_path) as (runtime, message):
        message.update(fields)
        before = _read(tmp_path / "bridge.json")
        runtime.poll_checkpoint = dict(before)
        runtime._complete(_Completion("poll", "page", {"messages": [message], "cursor": "next-page"},
                                      None, "2026-01-02T00:01:00Z"))
        assert _read(tmp_path / "bridge.json") == before
        observer = _read(tmp_path / "input.json")
        assert observer["cursor"] == "last-valid"
        assert "reconcile_error" not in observer
        assert runtime.input_state["reconcile_error"]
        runtime.input_observer._next_write = 0
        assert runtime.input_observer.flush()
        assert _read(tmp_path / "input.json")["reconcile_error"]
        assert runtime.reconcile_requested
        assert list(runtime.deferred.glob("*.json")) == []


def test_echo_survives_crash_before_cursor_commit_and_replays_once(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    with _pending_reply(tmp_path) as (runtime, message):
        def crash_cursor(path: Path, document: dict[str, object]) -> None:
            if path == tmp_path / "input.json":
                saved = list(runtime.deferred.glob("*.json"))
                assert len(saved) == 1 and _read(saved[0]) == message
                raise OSError("crash before cursor commit")
            _write(path, document)

        monkeypatch.setattr(runtime_module, "_write", crash_cursor)
        with pytest.raises(OSError, match="before cursor commit"):
            runtime._input_event({"type": "message", "message": message, "cursor": "valid-position"})
        paths = list(runtime.deferred.glob("*.json"))
        assert len(paths) == 1
        assert _read(paths[0]) == message
        assert _read(tmp_path / "input.json")["cursor"] == "last-valid"
        monkeypatch.setattr(runtime_module, "_write", _write)
        runtime._input_event({"type": "message", "message": message, "cursor": "valid-position"})
        assert list(runtime.deferred.glob("*.json")) == paths
        assert _read(tmp_path / "input.json")["cursor"] == "valid-position"
        assert len(list((tmp_path / "requests").glob("*.json"))) == 1


@pytest.mark.parametrize("fields", [
    {"sender": "users/unapproved", "text": 123, "thread_reply": "invalid"},
    {"created_at": "2025-01-01T00:00:00Z", "text": 123, "thread_reply": "invalid"},
    {"text": "", "thread_reply": "invalid"},
])
def test_ordinary_ingestion_validates_before_filtering(tmp_path: Path, fields: dict[str, object]) -> None:
    bridge, _, _ = setup(tmp_path)
    with pytest.raises((TypeError, ValueError)):
        bridge._ingest_result({"messages": [_message(_SOURCE, **fields)]})
    assert list((tmp_path / "requests").glob("*.json")) == []
