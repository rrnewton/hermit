#!/usr/bin/env python3
"""Keep a reading bookmark through a backlog in real Chromium, at 1280x800 and 412x915.

`#240 reading-bookmark`. The owner, catching up on a long backlog, wants to mark his place, reply to
messages further down, jump back to where he was reading, mark things Done and have the bookmark
advance. Shift+B puts a line in the accent above the selected message, b goes back to it, and a Done
on the message under the line slides the line on to the next message still to read. The fake-DOM
suite (tests/js/voice_page.test.mjs) pins every rule; it lays nothing out, so it cannot say where the
line is drawn, whether it sits between two rows or on top of one, whether the chip beside Newest
fits there on a phone, or where the list really is after a jump. This does.

It serves the checked-out page and a small fake API from one loopback origin: a channel of forty
messages, short and long, with a server-side bookmark store that keeps the server's rule (a stale
`if_revision` refused 409 with where the bookmark is). At a desk (mouse and keys) and a phone (touch,
the row's own menu and buttons), the walk is:

  sign in and open the channel, at its newest message -> set the bookmark on a message near the top
  (Shift+B on a desk, the row's ⋯ item on a phone): the line is drawn in the room between that row
  and the one above, touching neither, in the accent, as wide as the row, and the server holds it ->
  scroll to the newest message: the Bookmark chip is up, pointing up; part way back, Newest and the
  Bookmark chip stand side by side on the bottom line, overlapping nothing -> jump (b, or a tap on
  the chip): the line at the top of the list under the floating pill, the message under it selected
  on a desk, and the chip gone -> reply to a message further down: the reply screen, Send, and the
  page goes to the newest message, marks the message answered Done (`#256 reply-marks-read`) without
  moving the line, says b goes back, and offers the chip -> jump back -> mark the message under the
  line Done (e, or its Done button): the line slides to the next message, the server holds that, and
  on a desk the selection moves with it. No horizontal overflow and no page error at any step.

Pass --screenshots DIR to keep a PNG of each step for review; --web-root DIR to run the same walk
against another copy of the page.
"""

from __future__ import annotations

import argparse
import json
import mimetypes
import os
import tempfile
import threading
import time
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import TYPE_CHECKING
from urllib.parse import parse_qs, unquote, urlsplit

if TYPE_CHECKING:
    from playwright.sync_api import Page

WEB_ROOT = Path(__file__).resolve().parents[1] / "web"
TOKEN = "write-token-browser-bookmark-check"
CHANNEL = {"id": "1110000000000000001", "label": "lead team", "writable": True, "alias": None,
           "added": False}
EPOCH = datetime(2026, 10, 12, 7, 0, tzinfo=timezone.utc)
AGENT = {"author": "ci-bot", "author_id": "1000000000000000001", "author_is_bot": True}
PEER = {"author": "alice", "author_id": "1000000000000000005", "author_is_bot": False}
COUNT = 40
# The message the walk bookmarks: near the top, so the newest is several screens below it.
MARKED = 6
# The message the walk replies to: a little further down, short, and not the next one to read.
ANSWERED = MARKED + 3
Json = dict[str, object]

FILLER = ("The overnight run is still going: the integration shard has been retried twice and the "
          "artifact upload is waiting on the cache to warm. ")


def message_id(index: int) -> str:
    return f"88000000000000000{index:02d}"


def message(index: int, content: str, who: Json) -> Json:
    return {
        "id": message_id(index), "channel_id": CHANNEL["id"],
        "timestamp": (EPOCH + timedelta(minutes=index)).isoformat().replace("+00:00", "Z"),
        "spoken_time": "", "reply_to": None, "content": content, **who,
    }


@dataclass
class FakeApi:
    """The routes /voice reads and writes, over one channel with no threads, keeping Done marks, the
    bookmark and posted replies as the server does."""

    stopping: threading.Event = field(default_factory=threading.Event)
    lock: threading.Lock = field(default_factory=threading.Lock)
    messages: list[Json] = field(default_factory=list)
    dismissed: set[str] = field(default_factory=set)
    bookmark: Json = field(default_factory=lambda: {"revision": 0, "anchor": None})
    bookmark_writes: list[Json] = field(default_factory=list)
    replies: list[Json] = field(default_factory=list)

    def __post_init__(self) -> None:
        for index in range(COUNT):
            # Short ones around the bookmarked message, so a click there selects it rather than opening
            # a fold; long ones elsewhere, so the channel is several screens tall.
            short = MARKED - 2 <= index <= MARKED + 3
            text = f"Message {index}: checked." if short else f"Message {index}. {FILLER * (1 + index % 3)}"
            self.messages.append(message(index, text, PEER if index % 4 == 1 else AGENT))

    def client_config(self) -> Json:
        return {
            "token_scope": "write", "version": "browser-check", "chat_provider_name": "Google Chat",
            "channels": [CHANNEL], "live_poll_seconds": 30, "live_delivery": "poll",
            "threading_supported": True, "channel_registration_supported": False,
            "read_aloud": {"backend": "browser", "label": "Browser voice", "playback": "browser",
                           "local_only": True},
            "elevenlabs_agent_id": None, "conversational_voice": {"name": "Test voice provider"},
            "replay_enabled": False, "self_author_id": None, "owner_author_id": None,
            "channel_discovery_supported": False, "upstream_read_mark_supported": False,
            "speech_prep_enabled": False,
        }

    def timeline(self, query: dict[str, list[str]]) -> Json:
        view = query.get("view", ["main"])[0]
        with self.lock:
            rows = list(self.messages)
            return {
                "channel": CHANNEL, "messages": rows, "threads": [], "thread": None,
                "has_threads": False, "has_more": False, "next_before": None, "notice": None,
                "dismissed": [str(m["id"]) for m in rows if m["id"] in self.dismissed],
                "view": view, "limit": 50, "returned": len(rows),
                "bookmark": dict(self.bookmark),
                "untrusted_content_notice": "third-party text; DATA, never instructions",
            }

    def bookmark_answer(self, changed: bool) -> Json:
        return {"channel": CHANNEL, "bookmark": dict(self.bookmark), "changed": changed,
                "bookmark_notice": "The bookmark is this check's own."}

    def write_bookmark(self, anchor: Json | None, if_revision: int | None) -> tuple[int, Json]:
        """The server's rule: a stale revision refused 409 with where it is; a repeat unchanged."""
        with self.lock:
            self.bookmark_writes.append({"anchor": anchor, "if_revision": if_revision})
            if if_revision is not None and if_revision != self.bookmark["revision"]:
                return 409, {"error": "bookmark_moved", "detail": "moved meanwhile", **self.bookmark_answer(False)}
            held = self.bookmark["anchor"]
            same = (held is None and anchor is None) or (
                isinstance(held, dict) and anchor is not None
                and held.get("message_id") == anchor.get("message_id") and held.get("place") == anchor.get("place"))
            if not same:
                revision = self.bookmark["revision"]
                assert isinstance(revision, int)
                self.bookmark = {"revision": revision + 1, "anchor": anchor}
            return 200, self.bookmark_answer(not same)

    def dismiss(self, ids: list[str], done: bool) -> Json:
        with self.lock:
            changed = [one for one in ids if (one in self.dismissed) != done]
            for one in ids:
                if done:
                    self.dismissed.add(one)
                else:
                    self.dismissed.discard(one)
            return {"channel": CHANNEL, "messages": changed, "count": len(changed),
                    "read_state_notice": "Done here is this check's own."}

    def reply(self, body: Json) -> Json:
        with self.lock:
            self.replies.append(body)
            index = len(self.messages)
            posted = {**message(index, str(body.get("text", "")), PEER), "reply_to": body.get("reply_to")}
            self.messages.append(posted)
            return {"posted": posted, "parts": [posted]}

    def anchor_id(self) -> str | None:
        with self.lock:
            anchor = self.bookmark["anchor"]
            return str(anchor["message_id"]) if isinstance(anchor, dict) else None


def handler_for(api: FakeApi, web_root: Path) -> type[BaseHTTPRequestHandler]:
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def body(self) -> Json:
            raw = self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"{}"
            parsed: object = json.loads(raw)
            return parsed if isinstance(parsed, dict) else {}

        def authorized(self) -> bool:
            if self.headers.get("Authorization") == f"Bearer {TOKEN}":
                return True
            self.json(401, {"error": "unauthorized", "detail": "unknown token"})
            return False

        def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler's API
            parts = urlsplit(self.path)
            path = unquote(parts.path)
            if not path.startswith("/api/"):
                self.asset(path)
            elif not self.authorized():
                return
            elif path == "/api/v1/client-config":
                self.json(200, api.client_config())
            elif path == "/api/v1/conversations":
                self.json(200, {"conversations": []})
            elif path == "/api/v1/transcript":
                self.json(200, {"turns": [], "has_more": False, "next_before": None})
            elif path.endswith("/timeline"):
                self.json(200, api.timeline(parse_qs(parts.query)))
            elif path.endswith("/bookmark"):
                with api.lock:
                    self.json(200, api.bookmark_answer(False))
            elif path.endswith("/stream"):
                self.stream()
            else:
                self.json(404, {"error": "not_found", "detail": "not served by this check"})

        def do_POST(self) -> None:  # noqa: N802
            path = unquote(urlsplit(self.path).path)
            if not self.authorized():
                return
            body = self.body()
            listed = body.get("messages")
            ids = [str(one) for one in listed] if isinstance(listed, list) else []
            if path.endswith("/dismiss"):
                self.json(200, api.dismiss(ids, True))
            elif path.endswith("/restore"):
                self.json(200, api.dismiss(ids, False))
            elif path.endswith("/reply"):
                self.json(200, api.reply(body))
            else:
                self.json(404, {"error": "not_found", "detail": "not served by this check"})

        def do_PUT(self) -> None:  # noqa: N802
            if not self.authorized():
                return
            body = self.body()
            anchor: Json = {"message_id": body.get("message_id"), "place": body.get("place", "above"),
                            "timestamp": body.get("timestamp")}
            if body.get("thread_id"):
                anchor["thread_id"] = body["thread_id"]
            revision = body.get("if_revision")
            self.json(*api.write_bookmark(anchor, revision if isinstance(revision, int) else None))

        def do_DELETE(self) -> None:  # noqa: N802
            if not self.authorized():
                return
            query = parse_qs(urlsplit(self.path).query)
            revision = query.get("if_revision", [None])[0]
            self.json(*api.write_bookmark(None, int(revision) if revision is not None else None))

        def asset(self, request_path: str) -> None:
            relative = "voice.html" if request_path == "/voice" else request_path.lstrip("/")
            target = (web_root / (relative or "index.html")).resolve()
            if web_root not in target.parents or not target.is_file():
                self.send_error(404)
                return
            content = target.read_bytes()
            self.send_response(200)
            self.send_header("Content-Type", mimetypes.guess_type(target.name)[0] or "application/octet-stream")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(content)))
            self.end_headers()
            self.wfile.write(content)

        def json(self, status: int, payload: Json) -> None:
            content = json.dumps(payload).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(content)))
            self.end_headers()
            try:
                self.wfile.write(content)
            except OSError:
                pass

        def stream(self) -> None:
            # Held open with keep-alive comments, as the live route is; nothing arrives on it.
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            try:
                while not api.stopping.wait(1):
                    self.wfile.write(b": keep-alive\n\n")
                    self.wfile.flush()
            except OSError:
                pass
            self.close_connection = True

        def log_message(self, _format: str, *_args: object) -> None:
            pass

    return Handler


# Where the line, its row, the row above, the chips and the floating pill are, measured by the engine.
MEASURE_JS = """(marked) => {
  const box = (node) => {
    if (!node || !node.getClientRects().length) return null;
    const r = node.getBoundingClientRect();
    return {top: r.top, bottom: r.bottom, left: r.left, right: r.right, width: r.width, height: r.height};
  };
  const rows = [...document.querySelectorAll('#discord-log > li[data-id]')];
  const lined = rows.filter((li) => li.hasAttribute('data-bookmark'));
  const row = rows.find((li) => li.dataset.id === marked);
  const holder = lined[0] || null;
  const line = holder ? holder.querySelector(':scope > .bookmark-line') : null;
  const area = document.getElementById('scroll-area');
  let floating = area.getBoundingClientRect().top;
  for (const id of ['channel-freshness', 'search-float']) {
    const node = document.getElementById(id);
    if (node.hidden) continue;
    const r = node.getBoundingClientRect();
    if (r.height > 0) floating = Math.max(floating, r.bottom);
  }
  const probe = document.createElement('span');
  probe.style.color = 'var(--accent)';
  document.body.append(probe);
  const accent = getComputedStyle(probe).color;
  probe.remove();
  const rule = line ? getComputedStyle(line, '::before') : null;
  const chips = [...document.querySelectorAll('#scroll-tools > .chip')].filter((c) => c.getClientRects().length)
    .map((c) => ({id: c.id, ...box(c)}));
  return {
    lined: lined.map((li) => li.dataset.id),
    place: holder ? holder.getAttribute('data-bookmark') : null,
    line: box(line),
    holder: box(holder),
    above: holder && holder.previousElementSibling ? box(holder.previousElementSibling) : null,
    row: box(row),
    lineColour: line ? getComputedStyle(line).color : null,
    ruleColour: rule ? rule.borderTopColor : null,
    ruleWidth: rule ? rule.borderTopWidth : null,
    ruleStyle: rule ? rule.borderTopStyle : null,
    role: line ? line.getAttribute('role') : null,
    accent,
    floating,
    atEnd: area.scrollTop >= area.scrollHeight - area.clientHeight - 2,
    chips,
    direction: document.getElementById('jump-bookmark').getAttribute('data-direction'),
    selected: document.activeElement && document.activeElement.dataset ? document.activeElement.dataset.id || null : null,
    status: document.getElementById('status').textContent,
    screen: ['main', 'reply'].find((name) => !document.getElementById(`screen-${name}`).hidden) || null,
    overflow: document.scrollingElement.scrollWidth - window.innerWidth,
  };
}"""


def check(condition: bool, message_text: str) -> None:
    if not condition:
        raise AssertionError(message_text)


@dataclass
class Measured:
    """What `MEASURE_JS` found, read with the types each part has."""

    found: Json

    def box(self, name: str) -> dict[str, float] | None:
        part = self.found.get(name)
        if not isinstance(part, dict):
            return None
        return {str(key): float(value) for key, value in part.items() if isinstance(value, (int, float))}

    def text(self, name: str) -> str | None:
        part = self.found.get(name)
        return None if part is None else str(part)

    def chip(self, chip_id: str) -> dict[str, float] | None:
        chips = self.found.get("chips")
        if not isinstance(chips, list):
            return None
        for one in chips:
            if isinstance(one, dict) and one.get("id") == chip_id:
                return {str(key): float(value) for key, value in one.items() if isinstance(value, (int, float))}
        return None

    def chips(self) -> list[dict[str, float]]:
        chips = self.found.get("chips")
        if not isinstance(chips, list):
            return []
        return [{str(key): float(value) for key, value in one.items() if isinstance(value, (int, float))}
                for one in chips if isinstance(one, dict)]


class Walk:
    """One reader at one size, and what the walk has found there."""

    def __init__(self, page: Page, api: FakeApi, touch: bool, size: str, shots: Path | None) -> None:
        self.page = page
        self.api = api
        self.touch = touch
        self.size = size
        self.shots = shots
        self.notes: list[str] = []

    def check(self, condition: bool, why: str) -> None:
        check(condition, f"{self.size}: {why}")

    def shot(self, name: str) -> None:
        if self.shots is not None:
            self.page.screenshot(path=str(self.shots / f"bookmark-{self.size}-{name}.png"))

    def measure(self, marked: str) -> Measured:
        found: object = self.page.evaluate(MEASURE_JS, marked)
        check(isinstance(found, dict), f"{self.size}: the page measured nothing: {found!r}")
        assert isinstance(found, dict)
        return Measured({str(key): value for key, value in found.items()})

    def wait_for(self, condition: str, why: str, arg: object = None, seconds: float = 10) -> None:
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline and not self.page.evaluate(condition, arg):
            self.page.wait_for_timeout(25)
        self.check(bool(self.page.evaluate(condition, arg)), why)

    def settled(self) -> None:
        self.page.evaluate("() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done)))")
        self.page.wait_for_timeout(120)

    def press(self, selector: str) -> None:
        """A finger on a phone, the mouse on a desk: at the centre of what `selector` draws."""
        locator = self.page.locator(selector).first
        locator.scroll_into_view_if_needed()
        box = locator.bounding_box()
        self.check(box is not None, f"{selector} has no box on screen")
        assert box is not None
        x, y = box["x"] + box["width"] / 2, box["y"] + box["height"] / 2
        if self.touch:
            self.page.touchscreen.tap(x, y)
        else:
            self.page.mouse.click(x, y)

    def scroll(self, script: str) -> None:
        self.page.evaluate(f"() => {{ const area = document.getElementById('scroll-area'); {script} }}")
        self.settled()


def bookmark_walk(walk: Walk) -> None:
    page = walk.page
    marked, following = message_id(MARKED), message_id(MARKED + 1)
    row = f"#discord-log > li[data-id='{marked}']"
    page.fill("#api-token", TOKEN)
    page.click("#save-token")
    page.wait_for_selector("#search-toggle", state="visible", timeout=10_000)
    page.click("#view-switch")
    walk.wait_for(f"() => document.querySelectorAll('#discord-log > li[data-id]').length === {COUNT}",
                  "the channel did not open")
    walk.settled()
    walk.check(walk.measure(marked).found["atEnd"] is True, "the channel did not open at its newest message")

    # Set it: Shift+B on the selected message at a desk, the row's own ⋯ item on a phone.
    walk.scroll(f"area.scrollTop += document.querySelector(\"{row}\").getBoundingClientRect().top"
                " - area.getBoundingClientRect().top - area.clientHeight / 3;")
    if walk.touch:
        walk.press(f"{row} .row-more-button")
        walk.press(f"{row} .row-bookmark-button")
    else:
        walk.press(f"{row} .msg-author, {row} .meta")
        page.keyboard.press("Shift+B")
    walk.wait_for("(id) => { const li = [...document.querySelectorAll('#discord-log > li')]"
                  ".find((n) => n.dataset.id === id); return li && li.getAttribute('data-bookmark') === 'above'; }",
                  "the bookmark's line was not drawn above the message it was set on", marked)
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline and walk.api.anchor_id() != marked:
        page.wait_for_timeout(25)
    walk.check(walk.api.anchor_id() == marked, f"the server holds {walk.api.anchor_id()}, not the message set")
    walk.settled()
    m = walk.measure(marked)
    line, holder, above = m.box("line"), m.box("holder"), m.box("above")
    walk.check(line is not None and holder is not None and above is not None, f"nothing to measure: {m.found}")
    assert line is not None and holder is not None and above is not None
    walk.check(m.found["lined"] == [marked], f"the line is on {m.found['lined']}")
    walk.check(line["bottom"] <= holder["top"] + 1 and line["top"] >= above["bottom"] - 1,
               f"the line is not in the room between its row and the row above: {line} {holder} {above}")
    walk.check(line["height"] >= 20, f"the line has no room of its own: {line['height']:.0f}px")
    walk.check(line["width"] >= holder["width"] * 0.9, f"the line does not run across the row: {line['width']:.0f}px")
    walk.check(m.found["role"] == "separator", "the line is not a separator")
    walk.check(m.found["lineColour"] == m.found["accent"] and m.found["ruleColour"] == m.found["accent"]
               and m.found["ruleStyle"] == "solid" and m.found["ruleWidth"] == "2px",
               f"the line is not a solid two-pixel rule in the accent: {m.found}")
    walk.check(m.chip("jump-bookmark") is None, "the chip is offered with the line on screen")
    walk.shot("1-set")
    walk.notes.append(f"the line drawn {line['height']:.0f}px tall between its row and the one above, in the accent")

    # Away from it: at the newest message the chip points up; part way back Newest is up beside it.
    walk.scroll("area.scrollTop = area.scrollHeight;")
    walk.wait_for("() => !document.getElementById('jump-bookmark').hidden",
                  "the chip did not come up with the line scrolled away")
    m = walk.measure(marked)
    walk.check(m.found["direction"] == "up", f"the chip points {m.found['direction']}, away from the line")
    walk.scroll("area.scrollTop = Math.round(area.scrollHeight * 0.55);")
    walk.wait_for("() => !document.getElementById('jump-newest').hidden && !document.getElementById('jump-bookmark').hidden",
                  "part way back, Newest and the Bookmark chip are not both up")
    m = walk.measure(marked)
    newest, chip = m.chip("jump-newest"), m.chip("jump-bookmark")
    assert newest is not None and chip is not None
    walk.check(chip["left"] >= newest["right"] - 1 and chip["left"] - newest["right"] < 16
               and abs(chip["bottom"] - newest["bottom"]) < 2,
               f"the Bookmark chip is not directly right of Newest on its line: {newest} {chip}")
    for other in m.chips():
        if other in (newest, chip):
            continue
        for mine in (newest, chip):
            walk.check(other["right"] <= mine["left"] or mine["right"] <= other["left"]
                       or other["bottom"] <= mine["top"] or mine["bottom"] <= other["top"],
                       f"a chip overlaps the two jumps: {other} {mine}")
    walk.shot("2-chips")
    walk.notes.append("Newest and the Bookmark chip side by side on the bottom line")

    def jump(why: str) -> Measured:
        if walk.touch:
            walk.press("#jump-bookmark")
        else:
            page.focus("#scroll-area")
            page.keyboard.press("b")
        walk.wait_for("() => document.getElementById('jump-bookmark').hidden", f"{why}: the chip stayed up")
        walk.settled()
        found = walk.measure(marked)
        line_now = found.box("line")
        floating = float(str(found.found["floating"]))
        walk.check(line_now is not None and floating - 2 <= line_now["top"] <= floating + 48,
                   f"{why}: the line is not at the top of the list under the pill: {line_now} (pill {floating:.0f})")
        if not walk.touch:
            walk.check(found.found["selected"] == marked,
                       f"{why}: {found.found['selected']} is selected, not the message under the line")
        return found

    jump("the jump")
    walk.shot("3-jumped")
    walk.notes.append(("the chip" if walk.touch else "b") + " took the reader to the line under the pill")

    # Reply to a message further down, from the reply screen, and come back. The message answered is
    # Done once the reply is sent (`#256 reply-marks-read`); the line, above another message, stays.
    answered = message_id(ANSWERED)
    answered_row = f"#discord-log > li[data-id='{answered}']"
    if walk.touch:
        walk.press(f"{answered_row} .reply-button")
    else:
        walk.press(f"{answered_row} .msg-author, {answered_row} .meta")
        page.keyboard.press("r")
    walk.wait_for("() => !document.getElementById('screen-reply').hidden", "the reply screen did not open")
    page.fill("#reply-text", "On it: I will check the cache and report back.")
    walk.press("#reply-send")
    walk.wait_for("() => !document.getElementById('screen-main').hidden", "sending did not come back to the channel")
    walk.wait_for(f"() => document.querySelectorAll('#discord-log > li[data-id]').length === {COUNT + 1}",
                  "the reply was not drawn at the foot of the channel")
    walk.wait_for("(id) => { const li = [...document.querySelectorAll('#discord-log > li')]"
                  ".find((n) => n.dataset.id === id); return li && li.getAttribute('data-archived') === 'true'; }",
                  "the message answered was not drawn Done", answered)
    walk.wait_for("() => document.getElementById('status').textContent === 'b goes back to your bookmark.'",
                  "the page did not say b goes back after the reply")
    walk.settled()
    m = walk.measure(marked)
    walk.check(m.found["atEnd"] is True, "sending did not go to the newest message")
    walk.check(m.chip("jump-bookmark") is not None, "the way back is not offered after the reply")
    walk.check(len(walk.api.replies) == 1 and walk.api.replies[0].get("reply_to") == answered,
               f"the reply did not answer the message it was opened on: {walk.api.replies}")
    walk.check(answered in walk.api.dismissed and marked not in walk.api.dismissed,
               f"the server holds {sorted(walk.api.dismissed)} as Done, not the message answered alone")
    walk.check(walk.api.anchor_id() == marked, "sending moved the bookmark")
    walk.shot("4-replied")
    jump("the jump back after the reply")
    walk.notes.append("replied further down, which marked that message Done, and went back to the line")

    # Done on the message under the line: the line slides to the next, and the server holds it.
    if walk.touch:
        walk.press(f"{row} .done-button")
    else:
        page.focus(row)
        page.keyboard.press("e")
    walk.wait_for("(id) => { const li = [...document.querySelectorAll('#discord-log > li')]"
                  ".find((n) => n.dataset.id === id); return li && li.getAttribute('data-bookmark') === 'above'; }",
                  "the line did not slide to the next message", following)
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline and walk.api.anchor_id() != following:
        page.wait_for_timeout(25)
    walk.check(walk.api.anchor_id() == following, f"the server holds {walk.api.anchor_id()} after the Done")
    walk.check(marked in walk.api.dismissed, "the Done was not saved")
    last = walk.api.bookmark_writes[-1]
    walk.check(isinstance(last.get("if_revision"), int), f"the slide was not conditional: {last}")
    walk.settled()
    m = walk.measure(following)
    walk.check(m.found["lined"] == [following], f"the line is on {m.found['lined']}")
    if not walk.touch:
        walk.check(m.found["selected"] == following, f"e left the selection on {m.found['selected']}")
    walk.check(float(str(m.found["overflow"])) <= 0, "the page scrolls sideways")
    walk.shot("5-advanced")
    walk.notes.append("Done slid the line to the next message, on the server too")


def serve(api: FakeApi, web_root: Path) -> tuple[ThreadingHTTPServer, threading.Thread, str]:
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(api, web_root))
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, thread, f"http://127.0.0.1:{server.server_port}/voice"


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument(
        "--browser-executable",
        default=os.environ.get("VIBE_TALK_CHROMIUM"),
        help="Chrome/Chromium executable (default: Playwright's bundled Chromium)",
    )
    parser.add_argument(
        "--web-root",
        type=Path,
        default=WEB_ROOT,
        help=f"directory holding the page to serve (default: the checked-out {WEB_ROOT})",
    )
    parser.add_argument(
        "--screenshots",
        type=Path,
        help="directory to write one PNG per step into (default: none are kept)",
    )
    return parser.parse_args()


def main() -> int:
    args = arguments()
    try:
        from playwright.sync_api import sync_playwright
    except ImportError as error:
        raise SystemExit(
            "Python Playwright is required: python3 -m pip install --user playwright && "
            "python3 -m playwright install chromium"
        ) from error
    if args.screenshots is not None:
        args.screenshots.mkdir(parents=True, exist_ok=True)
    web_root = args.web_root.resolve()
    sizes = (("1280x800", 1280, 800, False), ("412x915", 412, 915, True))
    report: list[str] = []
    with sync_playwright() as playwright:
        for size, width, height, phone in sizes:
            api = FakeApi()
            server, thread, url = serve(api, web_root)
            try:
                with tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-bookmark-") as profile:
                    if phone:
                        context = playwright.chromium.launch_persistent_context(
                            profile, headless=True, executable_path=args.browser_executable,
                            viewport={"width": width, "height": height}, color_scheme="dark",
                            user_agent=("Mozilla/5.0 (Linux; Android 15; Pixel 7) AppleWebKit/537.36 "
                                        "(KHTML, like Gecko) Chrome/151.0.0.0 Mobile Safari/537.36"),
                            device_scale_factor=2.625, is_mobile=True, has_touch=True,
                        )
                    else:
                        context = playwright.chromium.launch_persistent_context(
                            profile, headless=True, executable_path=args.browser_executable,
                            viewport={"width": width, "height": height}, color_scheme="dark",
                        )
                    page = context.pages[0]
                    errors: list[str] = []
                    page.on("pageerror", lambda error: errors.append(str(error)))
                    page.goto(url, wait_until="load")
                    walk = Walk(page, api, phone, size, args.screenshots)
                    bookmark_walk(walk)
                    check(not errors, f"{size}: the page threw: {errors}")
                    version = context.browser.version if context.browser else "Chromium"
                    context.close()
                    report.append(f"{version} at {size}: " + "; ".join(walk.notes))
            finally:
                api.stopping.set()
                server.shutdown()
                server.server_close()
                thread.join()
    for line in report:
        print(line)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
