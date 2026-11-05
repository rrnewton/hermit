"""The Python drain's copy of the chat prompt's send-time opening.

The expected texts are the Rust tests' (rs/agentctl/src/prompt_time.rs): both drains must type the
same opening for the same queue document at the same instant.
"""

from __future__ import annotations

import json
import time
from collections.abc import Iterator
from pathlib import Path
from typing import cast

import pytest

import agentctl.prompt_time as prompt_time
from agentctl.agent import Target, drain, enqueue
from agentctl.client import AgentPaneInfo, HerdrClient, Pane

# 2026-10-07T12:45:00Z, 8:45 AM EDT.
MORNING = 1_791_377_100


def at(seconds: int, nanos: int = 0) -> int:
    return seconds * 1_000_000_000 + nanos


@pytest.fixture(autouse=True)
def eastern(monkeypatch: pytest.MonkeyPatch) -> Iterator[None]:
    monkeypatch.setenv("TZ", "EST5EDT,M3.2.0,M11.1.0")
    time.tzset()
    yield
    monkeypatch.undo()
    time.tzset()


def test_rfc3339_reads_utc_fractions_offsets_and_leap_seconds() -> None:
    assert prompt_time.rfc3339_instant("2026-10-07T12:45:00Z") == (MORNING, 0, False)
    assert prompt_time.rfc3339_instant("2026-10-07T08:45:00-04:00") == (MORNING, 0, False)
    assert prompt_time.rfc3339_instant("2026-10-07T18:15:00+05:30") == (MORNING, 0, False)
    assert prompt_time.rfc3339_instant("1969-12-31T23:59:59Z") == (-1, 0, False)
    assert prompt_time.rfc3339_instant("2026-10-07t12:45:00.123456z") == (MORNING, 123_456_000, False)
    assert prompt_time.rfc3339_instant("2026-10-07T12:44:59.9999999999Z") == (MORNING - 1, 999_999_999, True)
    assert prompt_time.rfc3339_instant("2016-12-31T23:59:60Z") == (1_483_228_799, 999_999_999, False)


@pytest.mark.parametrize("value", [
    "", "2026-10-07", "2026-10-07 12:45:00Z", "2026-10-07T12:45:00", "2026-10-07T12:45:00.Z",
    "2026-13-07T12:45:00Z", "2025-02-29T12:45:00Z", "2026-10-07T24:00:00Z",
    "2026-10-07T12:45:00+0400", "2026-10-07T12:45:00+24:00", "2026-10-07T12:45:00Zjunk",
    # Digits other than ASCII ones, in the fields, the fraction and the offset.
    "\uff12\uff10\uff12\uff16-10-07T12:45:00Z", "2026-10-07T12:45:00.\u0660Z",
    "2026-10-07T12:45:00.5\u0661Z", "2026-10-07T12:45:00+0\u0664:00",
])
def test_rfc3339_rejects_what_the_subscription_crate_rejects(value: str) -> None:
    assert prompt_time.rfc3339_instant(value) is None


def test_the_opening_matches_the_rust_drain() -> None:
    assert prompt_time.opening("2026-10-07T12:45:00Z", at(MORNING + 119)) == "Sent 2026.10.07:08:45 EDT. "
    assert prompt_time.opening("2026-10-07T12:50:00Z", at(MORNING)) == "Sent 2026.10.07:08:50 EDT. "
    assert prompt_time.opening("2026-10-07T12:44:59.9999999999Z", at(MORNING)) == "Sent 2026.10.07:08:44 EDT. "
    assert prompt_time.opening("2026-10-07T12:45:00.5Z", at(MORNING + 120)) == "Sent 2026.10.07:08:45 EDT. "
    assert (prompt_time.opening("2026-10-07T12:45:00.5Z", at(MORNING + 120, 500_000_000))
            == "Sent 2026.10.07:08:45 EDT, delivered 2 min later. ")
    for waited, words in [(120, "2 min"), (3_599, "59 min"), (3_600, "1 h 0 min"),
                          (3_600 + 12 * 60 + 30, "1 h 12 min"), (34 * 3_600 + 5 * 60, "1 d 10 h")]:
        assert (prompt_time.opening("2026-10-07T12:45:00Z", at(MORNING + waited))
                == f"Sent 2026.10.07:08:45 EDT, delivered {words} later. ")
    assert prompt_time.opening("yesterday", at(MORNING)) == ""


def test_the_stamp_uses_calendar_arithmetic_under_a_leap_second_zone(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("TZ", "right/UTC")
    time.tzset()
    assert prompt_time.opening("2017-01-01T00:00:00Z", at(1_483_228_800)) == "Sent 2017.01.01:00:00 UTC. "


def test_a_clock_the_rust_drain_reads_in_milliseconds_is_read_the_same_way() -> None:
    # 119.9995 seconds after creation at millisecond resolution, 120.0000 at nanosecond.
    queued = "Sent 2026.10.07:08:45 EDT. Request."
    recorded = "Sent 2026.10.07:08:45 EDT. "
    sent_at = "2026-10-07T12:45:00.0005Z"
    assert prompt_time.retime(queued, sent_at, recorded, at(MORNING + 120, 500_000)) == queued
    assert (prompt_time.retime(queued, sent_at, recorded, at(MORNING + 120, 1_000_000))
            == "Sent 2026.10.07:08:45 EDT, delivered 2 min later. Request.")


def test_retime_keeps_the_stored_text_when_the_opening_cannot_be_rendered(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    def broken(_created_at: str, _now_nanos: int) -> str:
        raise ValueError("year 0 is out of range")

    monkeypatch.setattr(prompt_time, "opening", broken)
    queued = "Sent 2026.10.07:08:45 EDT. Request."
    assert prompt_time.retime(queued, "2026-10-07T12:45:00Z", "Sent 2026.10.07:08:45 EDT. ", at(MORNING)) == queued


def test_retime_never_raises_once_the_prompt_is_in_flight(monkeypatch: pytest.MonkeyPatch) -> None:
    def broken(_seconds: float | None = None) -> time.struct_time:
        raise OverflowError("timestamp out of range for platform time_t")

    monkeypatch.setattr(time, "localtime", broken)
    queued = "Sent 2026.10.07:08:45 EDT. Request."
    retimed = prompt_time.retime(queued, "2026-10-07T12:45:00Z", "Sent 2026.10.07:08:45 EDT. ", at(MORNING))
    assert retimed == "Sent 2026.10.07:12:45 UTC. Request."


def test_retime_replaces_only_the_exact_recorded_opening() -> None:
    queued = "Sent 2026.10.07:08:45 EDT. The user's request arrived."
    recorded = "Sent 2026.10.07:08:45 EDT. "
    assert (prompt_time.retime(queued, "2026-10-07T12:45:00Z", recorded, at(MORNING + 72 * 60))
            == "Sent 2026.10.07:08:45 EDT, delivered 1 h 12 min later. The user's request arrived.")
    # A record that does not fit leaves the text as stored, never cut. The stale opening is
    # longer than the real one and ends inside the request's text, where cutting by its length
    # alone would lose the start of the request.
    long_request = "Sent 2026.10.07:08:45 EDT. The user wants the lock released before starting a task."
    stale = "Sent 2026.10.07:08:45 EDT, delivered 1 h 12 min later. "
    assert prompt_time.retime(long_request, "2026-10-07T12:45:00Z", stale, at(MORNING)) == long_request
    for sent_at, opening in [("2026-10-07T12:45:00Z", stale), ("2026-10-07T12:45:00Z", ""),
                             ("2026-10-07T12:45:00Z", "Sent"), ("soon", recorded),
                             (None, recorded), ("2026-10-07T12:45:00Z", None)]:
        assert prompt_time.retime(queued, sent_at, opening, at(MORNING)) == queued


class _Pane:
    """A Codex pane that is idle, and records each prompt typed into it."""

    def __init__(self) -> None:
        self.runs: list[str] = []

    def panes(self, workspace_id: str | None = None) -> tuple[Pane, ...]:
        del workspace_id
        return (Pane("w1:p1", "w1:t1", "w1"),)

    def pane_info(self, pane_id: str) -> AgentPaneInfo:
        return AgentPaneInfo(pane_id, "w1", "/work", "codex", "idle", "codex", "session-1")

    def workspace_label(self, workspace_id: str) -> str:
        del workspace_id
        return "acme"

    def prompt_agent(self, pane_id: str, text: str) -> None:
        del pane_id
        self.runs.append(text)

    def wait_agent_status(self, pane_id: str, state: str, timeout_ms: int) -> None:
        del pane_id, timeout_ms
        assert state == "working"

    def read(self, pane_id: str, *, source: str, lines: int) -> str:
        del pane_id, source, lines
        return ""


def test_a_clock_that_fails_leaves_the_prompt_typed_as_stored(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    enqueue(str(tmp_path), "placeholder", message_id="chat-clockless")
    path = tmp_path / "inbox/chat-clockless.json"
    document = json.loads(path.read_text())
    document.update({
        "text": "Sent 2026.10.07:08:45 EDT. The user's request.",
        "sent_at": "2026-10-07T12:45:00Z",
        "opening": "Sent 2026.10.07:08:45 EDT. ",
    })
    path.write_text(json.dumps(document))

    def no_clock() -> int:
        raise OSError("clock_gettime failed")

    monkeypatch.setattr(time, "time_ns", no_clock)
    pane = _Pane()
    target = Target(pane_id="w1:p1", session_agent="codex", session_value="session-1",
                    expected_agent="codex", expected_workspace="acme", expected_cwd="/work")
    result = drain(cast(HerdrClient, pane), target, str(tmp_path))
    assert result.delivered == ("chat-clockless",)
    assert pane.runs == ["Sent 2026.10.07:08:45 EDT. The user's request."]


def _queued_document(tmp_path: Path, message_id: str) -> None:
    enqueue(str(tmp_path), "placeholder", message_id=message_id)
    path = tmp_path / f"inbox/{message_id}.json"
    document = json.loads(path.read_text())
    document.update({
        "text": "Sent 2026.10.07:08:45 EDT. The user's request.",
        "sent_at": "2026-10-07T12:45:00Z",
        "opening": "Sent 2026.10.07:08:45 EDT. ",
    })
    path.write_text(json.dumps(document))


def _target() -> Target:
    return Target(pane_id="w1:p1", session_agent="codex", session_value="session-1",
                  expected_agent="codex", expected_workspace="acme", expected_cwd="/work")


def test_the_service_shutdown_signal_is_not_swallowed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    from agentctl.chat import _ServiceTerminated

    _queued_document(tmp_path, "chat-stopping")

    def stopping() -> int:
        raise _ServiceTerminated()

    monkeypatch.setattr(time, "time_ns", stopping)
    pane = _Pane()
    with pytest.raises(_ServiceTerminated):
        drain(cast(HerdrClient, pane), _target(), str(tmp_path))
    assert pane.runs == []
    assert (tmp_path / "inbox/chat-stopping.json").exists()
    assert not (tmp_path / "inflight/chat-stopping.json").exists()


def test_the_delay_is_read_after_the_in_flight_write(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    # 119 seconds old before the drain's durable writes, which then take ten minutes.
    _queued_document(tmp_path, "chat-slow-write")
    clock = [at(MORNING + 119)]
    monkeypatch.setattr(time, "time_ns", lambda: clock[0])
    import agentctl.agent as agent_module

    original = agent_module._atomic_json

    def slow(path: str, document: dict[str, object], *, max_artifact_bytes: int | None = None) -> None:
        original(path, document, max_artifact_bytes=max_artifact_bytes)
        if "inflight" in path:
            clock[0] += 600 * 1_000_000_000

    monkeypatch.setattr(agent_module, "_atomic_json", slow)
    pane = _Pane()
    drain(cast(HerdrClient, pane), _target(), str(tmp_path))
    assert pane.runs == ["Sent 2026.10.07:08:45 EDT, delivered 11 min later. The user's request."]


def test_a_prompt_the_rust_bridge_queued_is_retimed_when_python_types_it(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    # The document as the Rust enqueue writes it, 30 seconds after the message was created.
    enqueue(str(tmp_path), "placeholder", message_id="chat-retimed")
    path = tmp_path / "inbox/chat-retimed.json"
    document = json.loads(path.read_text())
    document.update({
        "text": "Sent 2026.10.07:08:45 EDT. The user's request.",
        "sent_at": "2026-10-07T12:45:00Z",
        "opening": "Sent 2026.10.07:08:45 EDT. ",
    })
    path.write_text(json.dumps(document))
    monkeypatch.setattr(time, "time_ns", lambda: at(MORNING + 72 * 60))
    pane = _Pane()
    target = Target(pane_id="w1:p1", session_agent="codex", session_value="session-1",
                    expected_agent="codex", expected_workspace="acme", expected_cwd="/work")
    result = drain(cast(HerdrClient, pane), target, str(tmp_path))
    assert result.delivered == ("chat-retimed",)
    assert pane.runs == ["Sent 2026.10.07:08:45 EDT, delivered 1 h 12 min later. The user's request."]
