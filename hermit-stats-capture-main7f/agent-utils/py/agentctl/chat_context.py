"""Read one bounded page of prior thread messages without a harness dependency."""

from __future__ import annotations

import calendar
import re
from collections.abc import Callable
from datetime import datetime

from agentctl.jsonx import as_mapping, as_sequence, get_str


_STAMP = re.compile(
    r"(\d{4}-\d{2}-\d{2})[Tt](\d{2}:\d{2}:\d{2})(?:\.(\d{1,9}))?([Zz]|[+-]\d{2}:\d{2})"
)
_SPACE = re.compile(r"spaces/[A-Za-z0-9_-]+")
_MAX_MESSAGES = 200
_MAX_CURSOR_BYTES = 8 * 1024


def _instant(value: str) -> tuple[int, int]:
    """Compare RFC3339 timestamps without discarding their nanoseconds."""
    match = _STAMP.fullmatch(value)
    if match is None:
        raise ValueError("context timestamps must be RFC3339 with a timezone and at most nine fractional digits")
    day, clock, fraction, zone = match.groups()
    # Parse whole seconds and offset separately from the fractional component:
    # Python datetime otherwise silently truncates sub-microsecond precision.
    if zone.lower() == "z":
        zone = "+00:00"
    elif int(zone[1:3]) > 23 or int(zone[4:]) > 59:
        raise ValueError("context timestamp has an invalid timezone offset")
    parsed = datetime.fromisoformat(f"{day}T{clock}{zone}")
    return calendar.timegm(parsed.utctimetuple()), int((fraction or "").ljust(9, "0"))


def _limit(value: object) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or not 1 <= value <= _MAX_MESSAGES:
        raise ValueError("context limit must be an integer between 1 and 200")
    return value


def _cursor(value: object) -> str | None:
    if value is not None and (not isinstance(value, str) or not value):
        raise ValueError("context cursor must be a nonempty string or null")
    if isinstance(value, str) and len(value.encode("utf-8")) > _MAX_CURSOR_BYTES:
        raise ValueError(f"context cursor must not exceed {_MAX_CURSOR_BYTES} UTF-8 bytes")
    return value


def _thread_reply(message: dict[str, object]) -> None:
    if "thread_reply" in message and not isinstance(message["thread_reply"], bool):
        raise ValueError("context thread_reply must be a boolean when present")


def _resource(value: str, space: str, kind: str) -> None:
    if re.fullmatch(re.escape(space) + "/" + kind + r"/[A-Za-z0-9_.-]+", value) is None:
        raise ValueError(f"context {kind} resource must belong to the selected space")


def google_context_params(request: dict[str, object]) -> dict[str, str]:
    """Validate one context request and return public Google Chat query parameters.

    The provider must return the nearest preceding messages in descending
    creation order. A cursor continues the original cutoff, limit, and thread;
    callers must preserve those values between pages.
    """
    if request.get("action") != "context":
        raise ValueError("expected a context transport request")
    space = get_str(request, "space", "context request")
    if _SPACE.fullmatch(space) is None:
        raise ValueError("context space must be an exact spaces/ID resource")
    thread = get_str(request, "thread", "context request")
    _resource(thread, space, "threads")
    before = get_str(request, "before", "context request")
    _instant(before)
    limit = _limit(request.get("limit", 10))
    cursor = _cursor(request.get("cursor"))
    params = {"pageSize": str(limit), "orderBy": "createTime DESC",
              "filter": f'createTime < "{before}" AND thread.name = {thread}'}
    if cursor is not None:
        params["pageToken"] = cursor
    return params


def read_context(
    transport: Callable[[dict[str, object]], dict[str, object]],
    source: dict[str, object], *, limit: int = 10, cursor: str | None = None,
) -> dict[str, object]:
    """Read the nearest prior thread messages, returning one chronological page.

    Only the source message chooses the space, thread, and strict timestamp
    cutoff. The transport is called once; an opaque cursor permits a later read
    of older messages. Other participants' messages are returned as context,
    without applying the bridge's authorization filter for incoming requests.
    """
    identifier = get_str(source, "id", "context source")
    match = re.fullmatch(r"(spaces/[A-Za-z0-9_-]+)/messages/[A-Za-z0-9_.-]+", identifier)
    if match is None:
        raise ValueError("context source must name one exact message resource")
    space = match.group(1)
    thread = get_str(source, "thread", "context source")
    before = get_str(source, "created_at", "context source")
    _thread_reply(source)
    request: dict[str, object] = {"action": "context", "space": space, "thread": thread,
                                  "before": before, "limit": limit, "cursor": cursor}
    google_context_params(request)
    cutoff = _instant(before)
    document = as_mapping(transport(request), "context response")
    continuation = _cursor(document.get("cursor"))
    if cursor is not None and continuation == cursor:
        raise ValueError("context response repeated its input cursor")
    rows = as_sequence(document.get("messages"), "context messages")
    if len(rows) > limit:
        raise ValueError("context response exceeds the requested message limit")
    messages: list[dict[str, object]] = []
    seen: set[str] = set()
    previous = cutoff
    for value in rows:
        row = as_mapping(value, "context message")
        name = get_str(row, "id", "context message")
        _resource(name, space, "messages")
        if name == identifier:
            raise ValueError("context response included the source message")
        if name in seen:
            raise ValueError("context response repeated a message")
        seen.add(name)
        if get_str(row, "thread", "context message") != thread:
            raise ValueError("context response included a message from another thread")
        stamp = get_str(row, "created_at", "context message")
        instant = _instant(stamp)
        if instant >= cutoff:
            raise ValueError("context response included a message not before the source")
        if instant > previous:
            raise ValueError("context response must arrive in descending creation order")
        previous = instant
        sender = get_str(row, "sender", "context message")
        if re.fullmatch(r"users/[A-Za-z0-9_-]+", sender) is None:
            raise ValueError("context message sender must be an exact users/ID resource")
        text = get_str(row, "text", "context message")
        if len(text.encode()) > 32000:
            raise ValueError("context message text exceeds 32000 UTF-8 bytes")
        _thread_reply(row)
        message: dict[str, object] = {"id": name, "thread": thread, "created_at": stamp,
                                      "sender": sender, "text": text}
        if "thread_reply" in row:
            message["thread_reply"] = row["thread_reply"]
        messages.append(message)
    messages.reverse()
    return {"source": identifier, "space": space, "thread": thread, "before": before,
            "order": "chronological", "messages": messages, "cursor": continuation}
