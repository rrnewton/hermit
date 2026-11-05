"""Bounded thread history, exact cutoffs, and transport response boundaries."""

from __future__ import annotations

import pytest

from agentctl.chat_context import google_context_params, read_context
from agentctl.jsonx import as_mapping, as_sequence


_SPACE = "spaces/test"
_THREAD = _SPACE + "/threads/thread-one"
_SOURCE = _SPACE + "/messages/incoming"


def _message(number: int, **fields: object) -> dict[str, object]:
    row: dict[str, object] = {
        "id": f"{_SPACE}/messages/message-{number}", "thread": _THREAD,
        "sender": "users/another-participant", "text": f"message {number}",
        "created_at": f"2026-01-02T00:00:{number:02d}Z", "thread_reply": True,
    }
    row.update(fields)
    return row


def _source(**fields: object) -> dict[str, object]:
    source = _message(26, id=_SOURCE)
    source.update(fields)
    return source


def _request(**fields: object) -> dict[str, object]:
    request: dict[str, object] = {"action": "context", "space": _SPACE, "thread": _THREAD,
                                 "before": "2026-01-02T00:00:26Z", "limit": 10, "cursor": None}
    request.update(fields)
    return request


class History:
    """A descending provider with older pages and another thread in the space."""

    def __init__(self) -> None:
        self.calls: list[dict[str, object]] = []
        self.messages = [_message(number) for number in range(1, 31)]
        self.messages.append(_message(25, thread=_SPACE + "/threads/unrelated"))

    def __call__(self, request: dict[str, object]) -> dict[str, object]:
        self.calls.append(request)
        assert request["action"] == "context"
        matching = [row for row in self.messages if row["thread"] == request["thread"]
                    and str(row["created_at"]) < str(request["before"])]
        matching.sort(key=lambda row: str(row["created_at"]), reverse=True)
        start = int(str(request["cursor"] or "0"))
        end = start + int(str(request["limit"]))
        return {"messages": matching[start:end], "cursor": str(end) if end < len(matching) else None}


def test_nearest_ten_prior_messages_and_older_pages_are_chronological() -> None:
    upstream = History()
    first = read_context(upstream, _source())
    assert first == {
        "source": _SOURCE, "space": _SPACE, "thread": _THREAD,
        "before": "2026-01-02T00:00:26Z", "order": "chronological",
        "messages": [_message(number) for number in range(16, 26)], "cursor": "10",
    }
    second = read_context(upstream, _source(), cursor=str(first["cursor"]))
    assert second["messages"] == [_message(number) for number in range(6, 16)]
    third = read_context(upstream, _source(), cursor=str(second["cursor"]))
    assert third["messages"] == [_message(number) for number in range(1, 6)]
    assert third["cursor"] is None
    assert upstream.calls == [_request(), _request(cursor="10"), _request(cursor="20")]


def test_rest_query_preserves_nanoseconds_offset_and_opaque_cursor() -> None:
    before = "2026-01-02T01:00:00.123456789+01:00"
    cursor = 'opaque+/= cursor & filter="different"'
    assert google_context_params(_request(before=before, limit=200, cursor=cursor)) == {
        "pageSize": "200", "orderBy": "createTime DESC",
        "filter": f'createTime < "{before}" AND thread.name = {_THREAD}', "pageToken": cursor,
    }
    assert "pageToken" not in google_context_params(_request())


def test_single_page_never_fetches_automatically_to_fill_limit() -> None:
    calls: list[dict[str, object]] = []

    def upstream(request: dict[str, object]) -> dict[str, object]:
        calls.append(request)
        return {"messages": [_message(25)], "cursor": "next+/="}

    result = read_context(upstream, _source())
    assert len(calls) == 1
    assert result["messages"] == [_message(25)]
    assert result["cursor"] == "next+/="


def test_nanosecond_cutoff_keeps_earlier_message_in_same_microsecond() -> None:
    before = "2026-01-02T01:00:00.123456789+01:00"
    row = _message(1, created_at="2026-01-02T00:00:00.123456788Z")

    def upstream(request: dict[str, object]) -> dict[str, object]:
        assert request["before"] == before
        return {"messages": [row], "cursor": None}

    assert read_context(upstream, _source(created_at=before))["messages"] == [row]


@pytest.mark.parametrize("stamp", [
    "2026-01-02T00:00:00.123456789Z", "2026-01-02T00:00:00.123456790Z",
    "2026-01-02T01:00:00.123456789+01:00", "2026-01-02T00:00:01Z",
])
def test_equal_or_later_message_is_never_returned(stamp: str) -> None:
    with pytest.raises(ValueError, match="not before"):
        read_context(lambda _: {"messages": [_message(1, created_at=stamp)]},
                     _source(created_at="2026-01-02T00:00:00.123456789Z"))


@pytest.mark.parametrize(("key", "value"), [
    ("space", "spaces/test/../other"), ("space", "spaces/"),
    ("thread", "spaces/other/threads/thread-one"), ("thread", _SOURCE),
    ("before", "2026-01-02T00:00:26"), ("before", "2026-01-02 00:00:26Z"),
    ("before", "2026-01-02T00:00:26.1234567890Z"),
    ("before", "2026-01-02T00:00:26+01:99"), ("before", "2026-02-30T00:00:00Z"),
    ("before", '2026-01-02T00:00:26Z" AND thread.name = other'),
    ("limit", True), ("limit", 0), ("limit", 201), ("limit", 10.0),
    ("cursor", ""), ("cursor", 3), ("cursor", "x" * 8193), ("action", "poll"),
])
def test_rest_context_query_rejects_invalid_fields(key: str, value: object) -> None:
    with pytest.raises((ValueError, TypeError)):
        google_context_params(_request(**{key: value}))


@pytest.mark.parametrize(("key", "value"), [
    ("id", _SPACE + "/messages/../other"), ("id", "spaces/other/messages/one"),
    ("thread", "spaces/other/threads/thread-one"), ("thread", "not-a-thread"),
    ("created_at", "not-a-time"), ("thread_reply", "true"), ("thread_reply", None),
])
def test_invalid_source_fails_before_transport(key: str, value: object) -> None:
    called = False

    def upstream(_: dict[str, object]) -> dict[str, object]:
        nonlocal called
        called = True
        return {"messages": []}

    source = _source()
    source[key] = value
    with pytest.raises((ValueError, TypeError)):
        read_context(upstream, source)
    assert not called


@pytest.mark.parametrize(("key", "value", "message"), [
    ("id", "spaces/other/messages/one", "selected space"),
    ("id", _SOURCE, "source message"),
    ("thread", "spaces/test/threads/another", "another thread"),
    ("thread", "spaces/other/threads/one", "another thread"),
    ("sender", "unverified-display-name", "users/ID"),
    ("text", "π" * 16001, "32000"),
    ("text", None, "text"),
    ("created_at", "not-a-time", "RFC3339"),
    ("thread_reply", "true", "boolean"),
    ("thread_reply", 1, "boolean"),
    ("thread_reply", None, "boolean"),
])
def test_invalid_context_row_is_not_exposed(key: str, value: object, message: str) -> None:
    with pytest.raises((ValueError, TypeError), match=message):
        read_context(lambda _: {"messages": [_message(1, **{key: value})]}, _source())


@pytest.mark.parametrize("rows", [
    [_message(1), _message(2)], [_message(2), _message(2)],
    [_message(number) for number in range(25, 14, -1)],
])
def test_invalid_order_duplicates_or_oversized_page_are_refused(rows: list[dict[str, object]]) -> None:
    with pytest.raises(ValueError):
        read_context(lambda _: {"messages": rows}, _source())


@pytest.mark.parametrize("response", [
    {"messages": {}, "cursor": None}, {"messages": ["not-an-object"]},
    {"messages": [], "cursor": 3}, {"messages": [], "cursor": ""},
    {"messages": [], "cursor": "x" * 8193},
    {"messages": [], "cursor": "same"},
])
def test_malformed_or_nonprogressing_pages_are_refused(response: dict[str, object]) -> None:
    with pytest.raises((ValueError, TypeError)):
        read_context(lambda _: response, _source(), cursor="same")


def test_empty_page_is_explicit_and_upstream_failures_are_not_empty_successes() -> None:
    result = read_context(lambda _: {"messages": [], "cursor": None}, _source())
    assert result["messages"] == []
    assert result["cursor"] is None
    assert result["thread"] == _THREAD

    def upstream(_: dict[str, object]) -> dict[str, object]:
        raise OSError("permission denied")

    with pytest.raises(OSError, match="permission denied"):
        read_context(upstream, _source())


def test_historical_sender_is_context_and_optional_thread_flag_remains_optional() -> None:
    row = _message(1, sender="users/other-participant", text="")
    del row["thread_reply"]
    source = _source(thread_reply=False)
    result = read_context(lambda _: {"messages": [row], "cursor": None}, source)
    message = as_mapping(as_sequence(result["messages"], "messages")[0], "message")
    assert message == row
    assert "thread_reply" not in message


def test_limit_rejected_before_transport() -> None:
    def upstream(_: dict[str, object]) -> dict[str, object]:
        pytest.fail("invalid limit must not call the transport")

    for limit in (False, 0, 201):
        with pytest.raises(ValueError, match="limit"):
            read_context(upstream, _source(), limit=limit)
