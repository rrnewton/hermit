#!/usr/bin/env python3
"""Press Reply in real Chromium and measure what moved, at 1280x800 and 412x915.

`#247 reply-continuity`. The owner: "Reply jumps to a different view with different FONT SIZE. This is
jarring ... I'd rather the top of the message I press Reply on to stay still on the screen. Expand it
below and truncate below that (moving up the text entry box underneath)." The fake-DOM suite
(tests/js/voice_page.test.mjs) holds the rules and the arithmetic, in a model that lays out nothing
but heights and gives the list and the reply screen one origin. It cannot say what size the words
are, whether the reply screen's title bar moves the message, or where the box really ends up. This
does, in the engine.

It serves the checked-out page and a small fake API from one loopback origin: a channel of forty
messages, short and long, from the agent and from another person, one of them a reply and one taller
than a phone's screen. At a desk (mouse) and a phone (touch), in the dark scheme, at three of the text
sizes Settings offers, for a message whose top is in the middle of the list, just under where the
title bar will be, half under the list's top edge, low on the screen, and taller than the screen:

  scroll the row there and measure its top and the computed size of its words -> press its Reply ->
  the reply screen is up with its tint; the message is drawn at the same size and the same width;
  its top is where the row's was (within 1px), or at the top of the conversation where the row's
  was above that; where it fits, the conversation ends at its foot and the box is directly under
  it; low on the screen it moves up only as far as shows all of it above the box; taller than the
  screen it keeps its top and the box is still on screen -> Cancel, and the list is back where it
  was, to the pixel.

Then, at 100%: Reply up while the phone is turned and turned back, or the desk window is made
shorter, taller, narrower and as it was, and after each the message is placed again where
Reply put it (never above the conversation, the box directly under it where it fits, its top back
where the row's was at the height Reply opened at, never moved down when the window shrinks) ->
Reply and Send, and the reply leaves with its
reference, the message it answers is marked Done (`#256 reply-marks-read`) and the list is at its
newest, untinted.

Then `#267 quote-reply`: words of a message selected as a reader selects them -- dragged over with
the mouse at the desk, which neither folds the row nor takes the selection, and left as a long press
leaves them on the phone -- make its Reply "Quote reply", in the accent and within its row; r (desk)
and a tap (phone, once as Chromium taps and once as a phone that clears the selection when the finger
lands) open the reply with the words quoted line by line, the box focused with the caret under the
quote; a quote cancelled untouched leaves no draft, and Send posts the quote and what was typed under
it to the message. At the desk a triple-click on a message of one paragraph, whose range Chromium ends
at the very start of the next row, is offered and quoted too.

And on a phone at 393 and then 360 wide, on a channel with long names (a person's and a bot's) and
reply counts: every row's words selected in turn, and Quote reply moves neither the row's height nor
its words' top in it by more than half a pixel. "Quote reply" where the header line has room, and
"Quote" at Reply's own width where it has not, its words inside the button; both are seen at 360.

Then once more in the light scheme, to measure the tint there and keep a picture of it. No
horizontal overflow and no page error at any step.

`#185 reply-new-thread`. Last, at both sizes, a channel where every message heads a thread nobody
has answered, as on Google Chat: Reply offers "Start a new thread from this message" under the box,
unticked, inside the window and above Send -> a press ticks it, the title and the note under the box
say where the reply now goes, the message stays where Reply put it with the box directly under it
(one low on the screen too), and Send is on screen -> Send posts the reply into that message's
thread.

Pass --screenshots DIR to keep a PNG of the reply screen for each case; --web-root DIR to run the
same walk against another copy of the page.
"""

from __future__ import annotations

import argparse
import json
import mimetypes
import os
import tempfile
import threading
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import TYPE_CHECKING, Literal
from urllib.parse import parse_qs, unquote, urlsplit

if TYPE_CHECKING:
    from playwright.sync_api import Page

WEB_ROOT = Path(__file__).resolve().parents[1] / "web"
TOKEN = "write-token-browser-reply-continuity-check"
CHANNEL = {"id": "1110000000000000002", "label": "lead team", "writable": True, "alias": None,
           "added": False}
EPOCH = datetime(2026, 10, 12, 7, 0, tzinfo=timezone.utc)
AGENT = {"author": "ci-bot", "author_id": "1000000000000000001", "author_is_bot": True}
PEER = {"author": "alice", "author_id": "1000000000000000005", "author_is_bot": False}
COUNT = 40
# The message taller than a phone's screen, and the reply (to message 3).
TALL = 20
REPLY = 26
# `#267 quote-reply`: a message of two paragraphs, for a quote over the blank line between them.
PARAGRAPHS = 32
Json = dict[str, object]
Scheme = Literal["dark", "light"]

FILLER = ("The overnight run is still going: the integration shard has been retried twice and the "
          "artifact upload is waiting on the cache to warm. ")


def message_id(index: int) -> str:
    return f"89000000000000000{index:02d}"


def thread_of(message_id_text: str) -> str:
    """The thread a message heads, in the empty-thread runs: a Google Chat thread name."""
    return f"spaces/AAAA/threads/{message_id_text}"


def message(index: int) -> Json:
    if index == TALL:
        text = f"Message {index}, the long one. " + FILLER * 40
    elif index == PARAGRAPHS:
        text = (f"Message {index}, in two paragraphs: the shard was retried twice.\n\n"
                "The second paragraph, after a blank line: the cache is warm now.")
    elif index % 3 == 0:
        text = f"Message {index}: checked."
    else:
        text = f"Message {index}. " + FILLER * (1 + index % 2)
    return {
        "id": message_id(index), "channel_id": CHANNEL["id"],
        "timestamp": (EPOCH + timedelta(minutes=index)).isoformat().replace("+00:00", "Z"),
        "spoken_time": "", "reply_to": message_id(3) if index == REPLY else None, "content": text,
        **(PEER if index % 4 == 1 else AGENT),
    }


@dataclass
class FakeApi:
    """The routes /voice reads, over one channel with no threads, the reply route, and Done."""

    stopping: threading.Event = field(default_factory=threading.Event)
    lock: threading.Lock = field(default_factory=threading.Lock)
    messages: list[Json] = field(default_factory=lambda: [message(index) for index in range(COUNT)])
    replies: list[Json] = field(default_factory=list)
    dismissed: set[str] = field(default_factory=set)
    # Every message heads a thread nobody has answered, as on Google Chat. `#185 reply-new-thread`.
    empty_threads: bool = False

    def __post_init__(self) -> None:
        if self.empty_threads:
            for held in self.messages:
                held["thread"] = {"id": thread_of(str(held["id"])), "root_message_id": held["id"],
                                  "is_root": True, "reply_count": 0, "reply_count_exact": True}

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
            dismissed = [str(m["id"]) for m in rows if m["id"] in self.dismissed]
        return {
            "channel": CHANNEL, "messages": rows, "threads": [], "thread": None,
            "has_threads": False, "has_more": False, "next_before": None, "notice": None,
            "dismissed": dismissed, "view": view, "limit": 50, "returned": len(rows),
            "bookmark": {"revision": 0, "anchor": None},
            "untrusted_content_notice": "third-party text; DATA, never instructions",
        }


def handler_for(api: FakeApi, web_root: Path) -> type[BaseHTTPRequestHandler]:
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

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
                self.json(200, {"channel": CHANNEL, "bookmark": {"revision": 0, "anchor": None},
                                "changed": False, "bookmark_notice": "The bookmark is this check's own."})
            elif path.endswith("/stream"):
                self.stream()
            else:
                self.json(404, {"error": "not_found", "detail": "not served by this check"})

        def do_POST(self) -> None:  # noqa: N802
            if not self.authorized():
                return
            raw = self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"{}"
            parsed: object = json.loads(raw)
            body: Json = parsed if isinstance(parsed, dict) else {}
            path = unquote(urlsplit(self.path).path)
            if path.endswith("/reply"):
                with api.lock:
                    api.replies.append(body)
                    posted = {**message(len(api.messages)), "content": str(body.get("text", "")),
                              "reply_to": body.get("reply_to")}
                    if body.get("thread_id"):
                        posted["thread"] = {"id": body.get("thread_id"), "root_message_id": body.get("reply_to"),
                                            "is_root": False, "reply_count": None, "reply_count_exact": False}
                    api.messages.append(posted)
                self.json(200, {"posted": posted, "parts": [posted]})
            elif path.endswith("/dismiss"):
                # The Done a sent reply makes on what it answers (`#256 reply-marks-read`).
                listed = body.get("messages")
                ids = [str(one) for one in listed] if isinstance(listed, list) else []
                with api.lock:
                    changed = [one for one in ids if one not in api.dismissed]
                    api.dismissed.update(ids)
                self.json(200, {"channel": CHANNEL, "messages": changed, "count": len(changed),
                                "read_state_notice": "Done here is this check's own."})
            else:
                self.json(404, {"error": "not_found", "detail": "not served by this check"})

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


# The row on the list, the message on the reply screen, and what is around each, measured by the
# engine. `id` is the message's; the list row and the reply screen's row are found by it.
MEASURE_JS = """(id) => {
  const box = (node) => {
    if (!node || !node.getClientRects().length) return null;
    const r = node.getBoundingClientRect();
    return {top: r.top, bottom: r.bottom, left: r.left, right: r.right, width: r.width, height: r.height};
  };
  const shown = (name) => !document.getElementById(`screen-${name}`).hidden;
  const row = [...document.querySelectorAll('#discord-log > li[data-id]')].find((li) => li.dataset.id === id);
  const target = document.querySelector('#reply-target > li');
  const words = (li) => li ? li.querySelector('.body .md') || li.querySelector('.body') : null;
  const size = (li) => words(li) ? parseFloat(getComputedStyle(words(li)).fontSize) : null;
  const area = document.getElementById('scroll-area');
  const pane = document.getElementById('reply-scroll');
  const page = getComputedStyle(document.body).backgroundColor;
  return {
    screen: shown('reply') ? 'reply' : shown('main') ? 'main' : null,
    row: box(row), rowSize: size(row), rowWords: box(words(row)),
    above: box(row ? row.previousElementSibling : null),
    contextAbove: box(document.querySelector('#reply-context > li:last-child')),
    target: box(target), targetSize: size(target), targetWords: box(words(target)),
    targetId: target ? target.dataset.id : null,
    targetClamped: target ? !!target.querySelector('.body.clamped') : null,
    list: box(area), listTop: area.scrollTop, listMax: area.scrollHeight - area.clientHeight,
    pane: box(pane), fit: pane.getAttribute('data-fit'),
    paneScrolls: pane.scrollHeight > pane.clientHeight + 1,
    text: box(document.getElementById('reply-text')),
    send: box(document.getElementById('reply-send')),
    tint: getComputedStyle(document.getElementById('frame-body')).backgroundColor,
    page,
    scale: getComputedStyle(document.documentElement).getPropertyValue('--msg-scale').trim(),
    overflow: document.scrollingElement.scrollWidth - window.innerWidth,
    height: window.innerHeight,
  };
}"""


def check(condition: bool, message_text: str) -> None:
    if not condition:
        raise AssertionError(message_text)


@dataclass
class Measured:
    """What `MEASURE_JS` found, read with the types each part has."""

    found: Json

    def box(self, name: str) -> dict[str, float]:
        part = self.found.get(name)
        check(isinstance(part, dict), f"nothing to measure for {name}: {self.found}")
        assert isinstance(part, dict)
        return {str(key): float(value) for key, value in part.items() if isinstance(value, (int, float))}

    def number(self, name: str) -> float:
        value = self.found.get(name)
        check(isinstance(value, (int, float)), f"{name} is not a number: {self.found}")
        assert isinstance(value, (int, float))
        return float(value)

    def text(self, name: str) -> str:
        return str(self.found.get(name))


class Walk:
    """One reader at one size and scheme, and what the walk has found there."""

    def __init__(self, page: Page, api: FakeApi, touch: bool, label: str, shots: Path | None) -> None:
        self.page = page
        self.api = api
        self.touch = touch
        self.label = label
        self.shots = shots
        self.notes: list[str] = []

    def check(self, condition: bool, why: str) -> None:
        check(condition, f"{self.label}: {why}")

    def shot(self, name: str) -> None:
        if self.shots is not None:
            self.page.screenshot(path=str(self.shots / f"reply-continuity-{self.label}-{name}.png"))

    def measure(self, index: int) -> Measured:
        found: object = self.page.evaluate(MEASURE_JS, message_id(index))
        self.check(isinstance(found, dict), f"the page measured nothing: {found!r}")
        assert isinstance(found, dict)
        return Measured({str(key): value for key, value in found.items()})

    def settled(self) -> None:
        self.page.evaluate("() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done)))")
        self.page.wait_for_timeout(80)

    def wait_for(self, condition: str, why: str, seconds: float = 10) -> None:
        try:
            self.page.wait_for_function(condition, timeout=seconds * 1000)
        except Exception as error:  # noqa: BLE001 - Playwright's timeout, said as this check's failure
            raise AssertionError(f"{self.label}: {why}") from error

    def press(self, selector: str) -> None:
        """A finger on a phone, the mouse on a desk: at the centre of what `selector` draws, unscrolled."""
        box = self.page.locator(selector).first.bounding_box()
        self.check(box is not None, f"{selector} has no box on screen")
        assert box is not None
        x, y = box["x"] + box["width"] / 2, box["y"] + box["height"] / 2
        if self.touch:
            self.page.touchscreen.tap(x, y)
        else:
            self.page.mouse.click(x, y)

    def place(self, index: int, y: float) -> Measured:
        """Scroll the list so the row of message `index` has its top at window y `y`."""
        self.page.evaluate(
            """([id, y]) => { const area = document.getElementById('scroll-area');
              const row = [...document.querySelectorAll('#discord-log > li[data-id]')].find((li) => li.dataset.id === id);
              area.scrollTop += row.getBoundingClientRect().top - y; }""",
            [message_id(index), y])
        self.settled()
        return self.measure(index)


def reply_case(walk: Walk, index: int, y: float, case: str, scale: int) -> None:
    """Reply on message `index` with its row's top at `y`: what moved, then Cancel and what came back."""
    before = walk.place(index, y)
    row = before.box("row")
    list_box = before.box("list")
    walk.check(row["top"] >= list_box["top"] - row["height"] and row["bottom"] > list_box["top"],
               f"{case}: the row is not on screen to press: {row}")
    walk.check(abs(float(before.text("scale")) - scale / 100) < 1e-9, f"the text size is {before.text('scale')}, not {scale}%")
    list_top = before.number("listTop")
    # The Reply of that row, pressed where it is drawn. Where the row's top is above the list's edge
    # its buttons may be too: then by the button itself, which a keyboard reaches as well.
    button = f"#discord-log > li[data-id='{message_id(index)}'] .reply-button"
    reply_box = walk.page.locator(button).bounding_box()
    if reply_box is not None and reply_box["y"] >= list_box["top"] and reply_box["y"] + reply_box["height"] <= list_box["bottom"]:
        walk.press(button)
    else:
        walk.page.evaluate("(selector) => document.querySelector(selector).click()", button)
    walk.wait_for("() => !document.getElementById('screen-reply').hidden", f"{case}: the reply screen did not open")
    walk.settled()
    after = walk.measure(index)
    target, pane, text = after.box("target"), after.box("pane"), after.box("text")
    walk.check(after.text("targetId") == message_id(index), f"{case}: the reply screen answers {after.text('targetId')}")
    # The same words at the same size, as wide as they were.
    walk.check(abs(after.number("targetSize") - before.number("rowSize")) < 0.01,
               f"{case}: the words are {after.number('targetSize')}px on Reply and {before.number('rowSize')}px on the list")
    walk.check(abs(target["left"] - row["left"]) <= 1 and abs(target["width"] - row["width"]) <= 1,
               f"{case}: the message is {target['left']:.1f}+{target['width']:.1f} wide on Reply, "
               f"{row['left']:.1f}+{row['width']:.1f} on the list")
    walk.check(after.found["targetClamped"] is False, f"{case}: the message being answered is folded")
    # The tint, and the page's own colour under it.
    walk.check(after.text("tint") != after.text("page") and after.text("tint") not in ("", "rgba(0, 0, 0, 0)"),
               f"{case}: the reply screen is not tinted: {after.text('tint')} on {after.text('page')}")
    # The box is on the screen, under the message's conversation.
    walk.check(text["top"] >= pane["bottom"] - 1 and after.box("send")["bottom"] <= after.number("height") + 1,
               f"{case}: the box is not on screen under the conversation: {text} {pane}")
    expected = max(row["top"], pane["top"])
    if case == "low":
        # Moved up only as far as shows all of it over the box.
        walk.check(target["bottom"] <= pane["bottom"] + 1 and abs(target["bottom"] - pane["bottom"]) <= 1,
                   f"{case}: the message does not end directly over the box: {target} {pane}")
        walk.check(target["top"] < row["top"], f"{case}: the message did not move up, so this asks nothing: {target}")
        walk.check(target["height"] <= pane["height"] + 1, f"{case}: the message is taller than the room")
    elif case == "tall":
        walk.check(target["height"] > pane["height"], f"{case}: the message is not taller than the screen: {target} {pane}")
        walk.check(abs(target["top"] - expected) <= 1, f"{case}: a tall message's top moved from {row['top']:.1f} to {target['top']:.1f}")
        walk.check(after.found["fit"] is None and after.found["paneScrolls"] is True,
                   f"{case}: the rest of the message is not a scroll down in the whole room: {after.found}")
    else:
        walk.check(abs(target["top"] - expected) <= 1,
                   f"{case}: the message's top moved from {row['top']:.1f} (kept at {expected:.1f}) to {target['top']:.1f}")
        walk.check(after.found["fit"] == "message" and abs(pane["bottom"] - target["bottom"]) <= 1,
                   f"{case}: the conversation does not end at the message's foot: {target} {pane} {after.found['fit']}")
        walk.check(abs(after.box("targetWords")["top"] - before.box("rowWords")["top"] - (target["top"] - row["top"])) <= 1,
                   f"{case}: the words moved inside the message: {after.box('targetWords')} {before.box('rowWords')}")
    if case == "middle":
        # What came before it is where it was too: the earlier message over it, drawn as the list drew it.
        above, context_above = before.box("above"), after.box("contextAbove")
        walk.check(abs(context_above["top"] - above["top"]) <= 1 and abs(context_above["height"] - above["height"]) <= 1,
                   f"{case}: the message above moved from {above} to {context_above}")
    walk.check(after.number("overflow") <= 0, f"{case}: the page scrolls sideways")
    walk.shot(f"{scale}-{case}")
    moved = target["top"] - row["top"]
    # Cancel, and the list is where it was.
    walk.press("#reply-cancel")
    walk.wait_for("() => !document.getElementById('screen-main').hidden", f"{case}: Cancel did not go back")
    walk.settled()
    back = walk.measure(index)
    walk.check(abs(back.number("listTop") - list_top) <= 1, f"{case}: Cancel came back to {back.number('listTop')}, not {list_top}")
    walk.check(abs(back.box("row")["top"] - row["top"]) <= 1, f"{case}: after Cancel the row is at {back.box('row')['top']}, not {row['top']}")
    walk.check(back.text("tint") == back.text("page") or back.text("tint") == "rgba(0, 0, 0, 0)",
               f"{case}: the tint outlived the reply screen: {back.text('tint')}")
    walk.notes.append(f"{scale}% {case}: words {after.number('targetSize'):.2f}px both, top {row['top']:.1f} -> "
                      f"{target['top']:.1f} ({moved:+.1f})")


def continuity_walk(walk: Walk, scales: tuple[int, ...]) -> None:
    page = walk.page
    page.fill("#api-token", TOKEN)
    page.click("#save-token")
    page.wait_for_selector("#search-toggle", state="visible", timeout=10_000)
    page.click("#view-switch")
    walk.wait_for(f"() => document.querySelectorAll('#discord-log > li[data-id]').length === {COUNT}",
                  "the channel did not open")
    walk.settled()
    for scale in scales:
        # Through the Settings slider's own listener, as a drag of it does.
        page.evaluate("(pct) => { const s = document.getElementById('msg-scale'); s.value = String(pct); "
                      "s.dispatchEvent(new Event('input', {bubbles: true})); }", scale)
        walk.settled()
        list_box = walk.measure(30).box("list")
        top, height = list_box["top"], list_box["height"]
        reply_case(walk, 30, top + height * 0.4, "middle", scale)
        reply_case(walk, 33, top + 70, "under-title", scale)
        reply_case(walk, 12, top - 20, "clamped", scale)
        reply_case(walk, REPLY, top + height * 0.3, "reply-gutter", scale)
        reply_case(walk, 34, top + height * 0.86, "low", scale)
        reply_case(walk, TALL, top + height * 0.2, "tall", scale)


def open_reply(walk: Walk, index: int, y: float, case: str) -> Measured:
    """Scroll message `index`'s row to `y` and press its Reply by the button itself; the reply screen, measured."""
    walk.place(index, y)
    walk.page.evaluate("(selector) => document.querySelector(selector).click()",
                       f"#discord-log > li[data-id='{message_id(index)}'] .reply-button")
    walk.wait_for("() => !document.getElementById('screen-reply').hidden", f"{case}: the reply screen did not open")
    walk.settled()
    return walk.measure(index)


def resize_case(walk: Walk, sizes: tuple[tuple[int, int], ...]) -> None:
    """A window resized, or a phone turned and back, while Reply is up: the message placed again where Reply put it.

    After each size the message is never above the conversation's top; where it fits, the
    conversation ends at its foot with the box directly under it (nothing clipped, no gap); at a
    height at least the one Reply opened at, its top is back where the row's was; and a window made
    shorter never moves it down.
    """
    index = 30
    list_box = walk.measure(index).box("list")
    before = open_reply(walk, index, list_box["top"] + list_box["height"] * 0.4, "resize")
    walk.check(before.found["fit"] == "message", f"resize: the message did not fit to begin with: {before.found}")
    opened = walk.page.viewport_size
    assert opened is not None
    first = before.box("target")["top"]
    path: list[str] = []
    for width, height in sizes:
        was = walk.page.viewport_size
        assert was is not None
        walk.page.set_viewport_size({"width": width, "height": height})
        walk.settled()
        walk.page.wait_for_timeout(120)
        after = walk.measure(index)
        target, pane, text = after.box("target"), after.box("pane"), after.box("text")
        old_target, old_pane = before.box("target"), before.box("pane")
        size = f"{was['width']}x{was['height']} -> {width}x{height}"
        walk.check(target["top"] >= pane["top"] - 1, f"resize {size}: the message is above the conversation: {target} {pane}")
        if after.found["fit"] == "message":
            walk.check(abs(target["bottom"] - pane["bottom"]) <= 1,
                       f"resize {size}: the conversation does not end at the message's foot: {target} {pane}")
        else:
            walk.check(target["height"] > pane["height"],
                       f"resize {size}: a message the room holds was left unfitted: {target} {pane}")
        walk.check(text["top"] >= pane["bottom"] - 1, f"resize {size}: the box is not under the conversation: {text} {pane}")
        offset, old_offset = target["top"] - pane["top"], old_target["top"] - old_pane["top"]
        if height >= opened["height"] and after.found["fit"] == "message":
            walk.check(abs(target["top"] - first) <= 1,
                       f"resize {size}: the message is at {target['top']:.1f}, not back where Reply put it ({first:.1f})")
        if height < was["height"]:
            walk.check(offset <= old_offset + 1, f"resize {size}: a shorter window moved the message down: {old_offset:.1f} -> {offset:.1f}")
        walk.check(after.number("overflow") <= 0, f"resize {size}: the page scrolls sideways")
        path.append(f"{width}x{height} top {target['top']:.1f} ({target['top'] - first:+.1f})")
        before = after
    walk.shot("resized")
    walk.press("#reply-cancel")
    walk.wait_for("() => !document.getElementById('screen-main').hidden", "resize: Cancel did not go back")
    walk.settled()
    walk.notes.append(f"resized on Reply, opened with the top at {first:.1f}: " + ", ".join(path))


def send_case(walk: Walk) -> None:
    """Send behaves as it did: the reply leaves with its reference, and the list is at its newest, untinted.
    The message answered is marked Done, on the row and on the server (`#256 reply-marks-read`)."""
    index = 31
    list_box = walk.measure(index).box("list")
    open_reply(walk, index, list_box["top"] + list_box["height"] * 0.4, "send")
    words = "on it, from the reply continuity check"
    walk.page.fill("#reply-text", words)
    sent = len(walk.api.replies)
    walk.press("#reply-send")
    walk.wait_for("() => !document.getElementById('screen-main').hidden", "send: Send did not go back to the list")
    walk.wait_for(f"() => document.querySelectorAll('#discord-log > li[data-id]').length === {COUNT + 1}",
                  "send: the sent reply never reached the list")
    walk.settled()
    walk.page.wait_for_timeout(200)
    with walk.api.lock:
        posted = list(walk.api.replies[sent:])
    walk.check(len(posted) == 1 and posted[0].get("text") == words and posted[0].get("reply_to") == message_id(index),
               f"send: what left the browser was {posted}")
    walk.wait_for(f"() => {{ const li = document.querySelector(\"#discord-log > li[data-id='{message_id(index)}']\");"
                  " return Boolean(li) && li.getAttribute('data-archived') === 'true'"
                  " && li.querySelector('.done-button').textContent === 'Unarchive'; }",
                  "send: the message answered is not drawn Done, with its button offering Unarchive")
    with walk.api.lock:
        done = set(walk.api.dismissed)
    walk.check(done == {message_id(index)}, f"send: the server holds {sorted(done)} as Done, not the message answered")
    after = walk.measure(index)
    walk.check(after.number("listTop") >= after.number("listMax") - 2,
               f"send: the list is at {after.number('listTop')}, not its newest ({after.number('listMax')})")
    walk.check(after.text("tint") == after.text("page") or after.text("tint") == "rgba(0, 0, 0, 0)",
               f"send: the tint outlived the reply screen: {after.text('tint')}")
    walk.notes.append("Send posted the reply to its message, marked that message Done, and left the list at its newest")


# The new-thread box and what is around it, measured by the engine. `#185 reply-new-thread`.
BRANCH_JS = """() => {
  const box = (node) => {
    if (!node || node.hidden || !node.getClientRects().length) return null;
    const r = node.getBoundingClientRect();
    return {top: r.top, bottom: r.bottom, left: r.left, right: r.right, width: r.width, height: r.height};
  };
  return {
    row: box(document.getElementById('reply-branch-row')),
    checked: document.getElementById('reply-branch').checked,
    note: document.getElementById('reply-destination').hidden ? null
      : document.getElementById('reply-destination').textContent,
    title: document.getElementById('topbar-title').textContent,
    width: window.innerWidth,
  };
}"""


# `#267 quote-reply`. Where in a row's words a selection from character `start` to `end` of its
# longest text node is drawn, and what the page then says: the selection's text, the row's Reply,
# and whether the row is folded and selected.
SELECTION_JS = """([id, start, end, select]) => {
  const row = [...document.querySelectorAll('#discord-log > li[data-id]')].find((li) => li.dataset.id === id);
  const body = row.querySelector('.body');
  const walker = document.createTreeWalker(body, NodeFilter.SHOW_TEXT);
  let text = null;
  for (let node = walker.nextNode(); node; node = walker.nextNode()) {
    if (!text || node.data.length > text.data.length) text = node;
  }
  const range = document.createRange();
  range.setStart(text, start);
  range.setEnd(text, end);
  if (select) getSelection().removeAllRanges(), getSelection().addRange(range);
  const rects = [...range.getClientRects()];
  const first = rects[0];
  return {left: first.left, right: first.right, top: first.top, bottom: first.bottom};
}"""

STATE_JS = """(id) => {
  const row = [...document.querySelectorAll('#discord-log > li[data-id]')].find((li) => li.dataset.id === id);
  const reply = row.querySelector('.reply-button');
  const box = document.getElementById('reply-text');
  const r = reply.getBoundingClientRect();
  const li = row.getBoundingClientRect();
  return {
    selected: String(getSelection()), label: reply.textContent, quote: reply.getAttribute('data-quote'),
    color: getComputedStyle(reply).color, collapsed: row.getAttribute('data-collapsed'),
    lit: row.getAttribute('data-selected'), screen: document.getElementById('screen-reply').hidden ? 'main' : 'reply',
    value: box.value, caret: box.selectionStart, focused: document.activeElement === box,
    replyRight: r.right, rowRight: li.right, overflow: document.scrollingElement.scrollWidth - window.innerWidth,
    draft: !row.querySelector('.draft-marker').hidden,
  };
}"""


def branch_state(walk: Walk) -> Measured:
    found: object = walk.page.evaluate(BRANCH_JS)
    walk.check(isinstance(found, dict), f"the page measured nothing: {found!r}")
    assert isinstance(found, dict)
    return Measured({str(key): value for key, value in found.items()})


def tick_branch(walk: Walk, case: str) -> None:
    walk.press("#reply-branch-row")
    walk.wait_for("() => document.getElementById('reply-branch').checked", f"{case}: a press did not tick the box")
    walk.settled()


def new_thread_walk(walk: Walk) -> None:
    """The new-thread box on Reply, on a channel whose messages each head an empty thread. `#185`."""
    page = walk.page
    page.fill("#api-token", TOKEN)
    page.click("#save-token")
    page.wait_for_selector("#search-toggle", state="visible", timeout=10_000)
    page.click("#view-switch")
    walk.wait_for(f"() => document.querySelectorAll('#discord-log > li[data-id]').length === {COUNT}",
                  "the channel did not open")
    walk.settled()
    # Low on the screen first, where the message ends at the foot of the room: what ticking the box
    # changes under it must not push the message's foot under the box.
    low = 34
    list_box = walk.measure(low).box("list")
    open_reply(walk, low, list_box["top"] + list_box["height"] * 0.86, "new-thread low")
    tick_branch(walk, "new-thread low")
    after = walk.measure(low)
    walk.check(after.found["fit"] == "message" and abs(after.box("pane")["bottom"] - after.box("target")["bottom"]) <= 1,
               f"new-thread low: ticked, the message's foot is not at the box: {after.box('target')} {after.box('pane')}")
    walk.check(after.box("send")["bottom"] <= after.number("height") + 1, "new-thread low: ticked, Send is below the window")
    walk.press("#reply-cancel")
    walk.wait_for("() => !document.getElementById('screen-main').hidden", "new-thread low: Cancel did not go back")
    walk.settled()

    index = 30
    list_box = walk.measure(index).box("list")
    opened = open_reply(walk, index, list_box["top"] + list_box["height"] * 0.4, "new-thread")
    branch = branch_state(walk)
    row, text, send = branch.box("row"), opened.box("text"), opened.box("send")
    walk.check(branch.found["checked"] is False, "new-thread: the box was ticked before anyone ticked it")
    walk.check(row["left"] >= 0 and row["right"] <= branch.number("width") + 0.5,
               f"new-thread: the box runs off the side of the window: {row}")
    walk.check(row["top"] >= text["bottom"] - 1 and row["bottom"] <= send["top"] + 1,
               f"new-thread: the box is not between the reply box and Send: {row} {text} {send}")
    walk.check(branch.text("note") == "Posts in the main channel as a reply to this message.",
               f"new-thread: unticked, the note says {branch.found['note']!r}")
    walk.shot("offered")
    tick_branch(walk, "new-thread")
    ticked = branch_state(walk)
    after = walk.measure(index)
    target, pane = after.box("target"), after.box("pane")
    walk.check(ticked.text("title").startswith("Reply · New thread"),
               f"new-thread: the title did not follow the box: {ticked.text('title')!r}")
    walk.check(ticked.text("note") == "Posts in a new thread on this message, as its first reply.",
               f"new-thread: ticked, the note says {ticked.found['note']!r}")
    walk.check(abs(target["top"] - opened.box("target")["top"]) <= 1,
               f"new-thread: ticking moved the message from {opened.box('target')['top']:.1f} to {target['top']:.1f}")
    walk.check(after.found["fit"] == "message" and abs(pane["bottom"] - target["bottom"]) <= 1,
               f"new-thread: ticked, the conversation does not end at the message's foot: {target} {pane}")
    walk.check(after.box("send")["bottom"] <= after.number("height") + 1,
               f"new-thread: ticked, Send is below the window: {after.box('send')}")
    walk.check(after.number("overflow") <= 0, "new-thread: the page scrolls sideways")
    walk.shot("ticked")
    page.fill("#reply-text", "the first answer in its thread, from the reply continuity check")
    sent = len(walk.api.replies)
    walk.press("#reply-send")
    walk.wait_for("() => !document.getElementById('screen-main').hidden", "new-thread: Send did not go back to the list")
    walk.page.wait_for_timeout(200)
    with walk.api.lock:
        posted = list(walk.api.replies[sent:])
    walk.check(len(posted) == 1 and posted[0].get("thread_id") == thread_of(message_id(index))
               and posted[0].get("reply_to") == message_id(index),
               f"new-thread: what left the browser was {posted}")
    walk.notes.append(f"the new-thread box {row['width']:.0f}x{row['height']:.0f} between the reply box and Send; "
                      f"ticked, the title and note followed, the message stayed, and Send posted into its thread")


def quote_text(selected: str) -> str:
    """The quote the page should write for `selected`: the rule Help states, restated for the check."""
    lines = [" ".join(line.split()) for line in selected.replace("\r\n", "\n").split("\n")]
    said = "\n".join(lines)
    while "\n\n\n" in said:
        said = said.replace("\n\n\n", "\n\n")
    said = said.strip()
    return "\n".join(f"> {line}" if line else ">" for line in said.split("\n")) + "\n\n"


def quote_case(walk: Walk) -> None:
    """Words of a message selected as a reader selects them make its Reply Quote reply; pressing it
    opens the reply with them quoted, the box focused with the caret under them, and Send posts it.

    At a desk the words are selected by dragging the mouse over them, which neither folds the row nor
    reads it, and selects it; r then quotes, and so does a click on Quote reply. On a phone, a
    selection is made as a long press leaves one (by the engine's own Selection API, which Playwright's
    touch cannot drag), and Quote reply is tapped: once as Chromium does it, keeping the selection
    through the tap, and once as a phone that clears the selection when the finger lands, before the
    click, which the page holds for the tap.
    """
    page = walk.page
    # Not 31, which `send_case` has answered and which counts its reply.
    index = 29
    row_id = message_id(index)
    list_box = walk.measure(index).box("list")
    walk.place(index, list_box["top"] + list_box["height"] * 0.4)

    def state() -> Json:
        found: object = page.evaluate(STATE_JS, row_id)
        assert isinstance(found, dict)
        return {str(key): value for key, value in found.items()}

    before = state()
    walk.check(before["label"] == "Reply" and before["quote"] is None, f"quote: Reply was not Reply to begin with: {before}")
    plain_color = before["color"]
    if walk.touch:
        page.evaluate(SELECTION_JS, [row_id, 12, 45, True])
    else:
        line: object = page.evaluate(SELECTION_JS, [row_id, 12, 45, False])
        assert isinstance(line, dict)
        y = (float(line["top"]) + float(line["bottom"])) / 2
        page.mouse.move(float(line["left"]) + 2, y)
        page.mouse.down()
        page.mouse.move(float(line["right"]) - 2, y, steps=8)
        page.mouse.up()
    walk.wait_for(f"() => document.querySelector(\"#discord-log > li[data-id='{row_id}'] .reply-button\")"
                  ".textContent === 'Quote reply'", "quote: the selection did not make Reply Quote reply")
    walk.settled()
    lit = state()
    selected = str(lit["selected"])
    walk.check(len(selected.strip()) > 10, f"quote: nothing was selected: {lit}")
    walk.check(lit["collapsed"] == before["collapsed"], f"quote: selecting the words folded the row: {before} -> {lit}")
    walk.check(lit["color"] != plain_color, f"quote: Quote reply is drawn as Reply is: {lit['color']}")
    walk.check(float(str(lit["overflow"])) <= 0 and float(str(lit["replyRight"])) <= float(str(lit["rowRight"])) + 1,
               f"quote: Quote reply does not fit its row: {lit}")
    if not walk.touch:
        walk.check(lit["lit"] == "true", f"quote: the drag's click did not select the row: {lit}")
    walk.shot("quote-label")
    expected = quote_text(selected)
    if not walk.touch:
        # r on the selected message, the selection inside it: the quote, the box focused.
        page.keyboard.press("r")
        walk.wait_for("() => !document.getElementById('screen-reply').hidden", "quote: r did not open the reply")
        walk.settled()
        keyed = state()
        walk.check(keyed["value"] == expected and keyed["focused"] is True and keyed["caret"] == len(expected),
                   f"quote: r did not quote {expected!r}: {keyed}")
        walk.press("#reply-cancel")
        walk.wait_for("() => !document.getElementById('screen-main').hidden", "quote: Cancel did not go back")
        walk.settled()
        cancelled = state()
        walk.check(cancelled["draft"] is False, f"quote: an untouched quote was kept as a draft: {cancelled}")
        walk.check(cancelled["label"] == "Reply", f"quote: Reply stayed Quote reply after its quote was taken: {cancelled}")
        triple_click_case(walk, list_box)
        walk.place(index, list_box["top"] + list_box["height"] * 0.4)
        # Selected again, and Quote reply clicked.
        line = page.evaluate(SELECTION_JS, [row_id, 12, 45, False])
        assert isinstance(line, dict)
        y = (float(line["top"]) + float(line["bottom"])) / 2
        page.mouse.move(float(line["left"]) + 2, y)
        page.mouse.down()
        page.mouse.move(float(line["right"]) - 2, y, steps=8)
        page.mouse.up()
        walk.wait_for(f"() => document.querySelector(\"#discord-log > li[data-id='{row_id}'] .reply-button\")"
                      ".textContent === 'Quote reply'", "quote: the second selection did not make Reply Quote reply")
        expected = quote_text(str(state()["selected"]))
    else:
        # Tapped as Chromium does it, the selection kept through the tap.
        walk.press(f"#discord-log > li[data-id='{row_id}'] .reply-button")
        walk.wait_for("() => !document.getElementById('screen-reply').hidden", "quote: the tap did not open the reply")
        walk.settled()
        tapped = state()
        walk.check(tapped["value"] == expected and tapped["focused"] is True and tapped["caret"] == len(expected),
                   f"quote: the tap did not quote {expected!r}: {tapped}")
        walk.press("#reply-cancel")
        walk.wait_for("() => !document.getElementById('screen-main').hidden", "quote: Cancel did not go back")
        walk.settled()
        # Over the blank line between two paragraphs, and a phone that clears it as the finger lands.
        two = message_id(PARAGRAPHS)
        walk.place(PARAGRAPHS, list_box["top"] + list_box["height"] * 0.4)
        page.evaluate("""(id) => {
          const row = [...document.querySelectorAll('#discord-log > li[data-id]')].find((li) => li.dataset.id === id);
          const parts = [...row.querySelectorAll('.body p')];
          const first = parts[0].firstChild, last = parts[parts.length - 1].firstChild;
          getSelection().setBaseAndExtent(first, 9, last, 22);
          document.addEventListener('pointerdown', () => getSelection().removeAllRanges(), {capture: true, once: true});
        }""", two)
        walk.wait_for(f"() => document.querySelector(\"#discord-log > li[data-id='{two}'] .reply-button\")"
                      ".textContent === 'Quote reply'", "quote: two paragraphs selected did not make Reply Quote reply")
        across: object = page.evaluate("() => String(getSelection())")
        walk.check("\n" in str(across), f"quote: the engine read two paragraphs as one line: {across!r}")
        expected = quote_text(str(across))
        walk.check("\n>\n" in expected, f"quote: the selection has no blank line to quote: {expected!r}")
        row_id = two
    walk.press(f"#discord-log > li[data-id='{row_id}'] .reply-button")
    walk.wait_for("() => !document.getElementById('screen-reply').hidden", "quote: Quote reply did not open the reply")
    walk.settled()
    opened = state()
    walk.check(opened["value"] == expected, f"quote: the box holds {opened['value']!r}, not {expected!r}")
    walk.check(opened["focused"] is True and opened["caret"] == len(expected),
               f"quote: the box is not focused with the caret under the quote: {opened}")
    walk.shot("quote-reply")
    page.keyboard.type("agreed")
    sent = len(walk.api.replies)
    walk.press("#reply-send")
    walk.wait_for("() => !document.getElementById('screen-main').hidden", "quote: Send did not go back to the list")
    walk.settled()
    page.wait_for_timeout(200)
    with walk.api.lock:
        posted = list(walk.api.replies[sent:])
    walk.check(len(posted) == 1 and posted[0].get("text") == f"{expected}agreed" and posted[0].get("reply_to") == row_id,
               f"quote: what left the browser was {posted}")
    quoted = len(expected.splitlines()) - 1
    walk.notes.append(f"Quote reply quoted {quoted} line{'' if quoted == 1 else 's'} and posted them to their message")


# Where the selection's first range ends: the row it ends in, and whether that is in the row's body.
RANGE_END_JS = """() => {
  const range = getSelection().rangeCount ? getSelection().getRangeAt(0) : null;
  if (!range) return null;
  let node = range.endContainer;
  const element = node.nodeType === 1 ? node : node.parentElement;
  const row = element ? element.closest('#discord-log > li[data-id]') : null;
  return {row: row ? row.dataset.id : null, inBody: !!(element && element.closest('.body'))};
}"""


def triple_click_case(walk: Walk, list_box: dict[str, float]) -> None:
    """A triple-click on a message of one paragraph, at a desk: Quote reply, and the paragraph quoted.

    Chromium selects the paragraph and the break after it, and ends the range at the very start of the
    next row, so the range's common ancestor is the list, as it is for a selection across two rows.
    Nothing of the next row is selected, and the page takes the range as the message's.
    """
    page = walk.page
    single = 27
    row_id = message_id(single)
    walk.place(single, list_box["top"] + list_box["height"] * 0.4)
    word: object = page.evaluate(SELECTION_JS, [row_id, 0, 7, False])
    assert isinstance(word, dict)
    page.mouse.click(float(word["left"]) + 3, (float(word["top"]) + float(word["bottom"])) / 2, click_count=3)
    walk.wait_for(f"() => document.querySelector(\"#discord-log > li[data-id='{row_id}'] .reply-button\")"
                  ".textContent === 'Quote reply'", "triple-click: the paragraph selected did not make Reply Quote reply")
    walk.settled()
    selected: object = page.evaluate("() => String(getSelection())")
    walk.check(str(selected).strip() == f"Message {single}: checked.", f"triple-click: the engine selected {selected!r}")
    ended: object = page.evaluate(RANGE_END_JS)
    walk.check(isinstance(ended, dict), "triple-click: nothing is selected")
    assert isinstance(ended, dict)
    next_row = page.evaluate(f"() => document.querySelector(\"#discord-log > li[data-id='{message_id(single + 1)}'] .reply-button\").textContent")
    walk.check(next_row == "Reply", f"triple-click: the next message's Reply reads {next_row!r}")
    expected = quote_text(str(selected))
    walk.press(f"#discord-log > li[data-id='{row_id}'] .reply-button")
    walk.wait_for("() => !document.getElementById('screen-reply').hidden", "triple-click: Quote reply did not open the reply")
    walk.settled()
    opened: object = page.evaluate(STATE_JS, row_id)
    assert isinstance(opened, dict)
    walk.check(opened["value"] == expected == f"> Message {single}: checked.\n\n",
               f"triple-click: the box holds {opened['value']!r}, not {expected!r}")
    walk.press("#reply-cancel")
    walk.wait_for("() => !document.getElementById('screen-main').hidden", "triple-click: Cancel did not go back")
    walk.settled()
    where = "the next row" if ended["row"] != row_id else "its own row, outside the words" if not ended["inBody"] else "its words"
    walk.notes.append(f"a triple-click's paragraph, its range ending in {where}, was offered and quoted")


# `#267 quote-reply`, at phone widths: each row's Reply made Quote reply by a selection in its words,
# and what that moved. The row's height, its words' top in it and Reply's width, before and after.
LAYOUT_JS = """async () => {
  const frame = () => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done)));
  const until = async (ok) => { for (let tries = 0; tries < 20 && !ok(); tries += 1) await frame(); return ok(); };
  const out = [];
  for (const row of document.querySelectorAll('#discord-log > li[data-id]')) {
    const body = row.querySelector('.body');
    const reply = row.querySelector('.reply-button');
    if (!body || !reply || !row.getClientRects().length) continue;
    const walker = document.createTreeWalker(body, NodeFilter.SHOW_TEXT);
    let text = walker.nextNode();
    while (text && !text.data.trim()) text = walker.nextNode();
    if (!text) continue;
    const at = () => {
      const box = row.getBoundingClientRect();
      return {height: box.height, bodyTop: body.getBoundingClientRect().top - box.top,
              width: reply.getBoundingClientRect().width};
    };
    const before = at();
    getSelection().setBaseAndExtent(text, 0, text, Math.min(text.data.length, 7));
    const lit = await until(() => reply.hasAttribute('data-quote'));
    const after = at();
    const author = row.querySelector('.msg-author');
    out.push({id: row.dataset.id, lit, label: reply.textContent, quote: reply.getAttribute('data-quote'),
              author: author && !author.hidden ? author.textContent : '', clipped: reply.scrollWidth > reply.clientWidth + 0.5,
              before, after});
    getSelection().removeAllRanges();
    await until(() => !reply.hasAttribute('data-quote'));
  }
  return out;
}"""

# Longer names than the walk's own, as bots and people have, for `quote_layout_walk`.
LONG_PEER = {"author": "integration-coordinator", "author_id": "1000000000000000009", "author_is_bot": False}
LONG_BOT = {"author": "release-watcher-on-call", "author_id": "1000000000000000010", "author_is_bot": True}
# The long-named messages answered twice each, so that their Reply counts: "Reply (2)".
ANSWERED_TWICE = (5, 6, 9, 10, 13)


def layout_messages() -> list[Json]:
    """The walk's channel with long author names, a person's and a bot's, and replies counted on them."""
    held = [message(index) for index in range(COUNT)]
    for index, kept in enumerate(held):
        if index % 4 == 1:
            kept.update(LONG_PEER)
        elif index % 4 == 2:
            kept.update(LONG_BOT)
    for target in ANSWERED_TWICE:
        for turn in range(2):
            answer = message(len(held))
            answer.update(content=f"Reply {turn + 1} to message {target}.", reply_to=message_id(target))
            held.append(answer)
    return held


def quote_layout_walk(walk: Walk) -> None:
    """Quote reply moves nothing on a phone, at 393 and 360 wide, on rows with long names and counts.

    Every row's words are selected in turn, as a long press leaves them, and its height and its
    words' top in it are measured before and after Reply becomes Quote reply: the same, to half a
    pixel. Where the header line has room, "Quote reply"; where it does not, "Quote" at Reply's width,
    its words inside the button. Both are seen at 360.
    """
    page = walk.page
    page.fill("#api-token", TOKEN)
    page.click("#save-token")
    page.wait_for_selector("#search-toggle", state="visible", timeout=10_000)
    page.click("#view-switch")
    walk.wait_for(f"() => document.querySelectorAll('#discord-log > li[data-id]').length >= {COUNT}",
                  "the channel did not open")
    walk.settled()
    long_peer = f"{LONG_PEER['author']}"
    for width, height in ((393, 852), (360, 780)):
        page.set_viewport_size({"width": width, "height": height})
        walk.settled()
        found: object = page.evaluate(LAYOUT_JS)
        walk.check(isinstance(found, list) and len(found) >= COUNT, f"layout {width}: rows measured: {found!r}")
        assert isinstance(found, list)
        rows = [row for row in found if isinstance(row, dict)]
        for row in rows:
            before, after = row["before"], row["after"]
            walk.check(row["lit"] is True and str(row["label"]).startswith("Quote") and row["quote"] in ("true", "short"),
                       f"layout {width}: a selection in {row['id']} did not make its Reply Quote reply: {row}")
            walk.check(abs(after["height"] - before["height"]) <= 0.5 and abs(after["bodyTop"] - before["bodyTop"]) <= 0.5,
                       f"layout {width}: Quote reply moved the words of {row['id']} ({row['author']!r}, {row['label']!r}): "
                       f"{before} -> {after}")
            walk.check(row["clipped"] is False, f"layout {width}: {row['label']!r} is cut off in its button: {row}")
            if row["quote"] == "short":
                walk.check(abs(after["width"] - before["width"]) <= 0.5,
                           f"layout {width}: Quote is not as wide as Reply was: {row}")
        counted_peer = [row for row in rows if row["author"] == long_peer and "(" in str(row["label"])]
        walk.check(bool(counted_peer), f"layout {width}: no row of {long_peer!r} with a count was measured")
        short = sum(1 for row in rows if row["quote"] == "short")
        full = sum(1 for row in rows if row["quote"] == "true")
        if width == 360:
            walk.check(short > 0 and full > 0, f"layout 360: not both labels were seen: {full} full, {short} short")
            target = next(row for row in counted_peer)
            page.evaluate("""(id) => { const row = [...document.querySelectorAll('#discord-log > li[data-id]')]
                .find((li) => li.dataset.id === id); row.scrollIntoView({block: 'center'});
                const walker = document.createTreeWalker(row.querySelector('.body'), NodeFilter.SHOW_TEXT);
                const text = walker.nextNode(); getSelection().setBaseAndExtent(text, 0, text, 7); }""", target["id"])
            walk.settled()
            walk.shot("quote-layout-360")
            page.evaluate("() => getSelection().removeAllRanges()")
        walk.notes.append(f"at {width} wide Quote reply moved no row of {len(rows)} ({full} read Quote reply, "
                          f"{short} Quote at Reply's width; {len(counted_peer)} of {long_peer} with a count)")


def light_walk(walk: Walk) -> None:
    """The light scheme: the tint is there too, and differs from the page; a picture of it."""
    page = walk.page
    page.fill("#api-token", TOKEN)
    page.click("#save-token")
    page.wait_for_selector("#search-toggle", state="visible", timeout=10_000)
    page.click("#view-switch")
    walk.wait_for(f"() => document.querySelectorAll('#discord-log > li[data-id]').length === {COUNT}",
                  "the channel did not open")
    walk.settled()
    list_box = walk.measure(30).box("list")
    reply_case(walk, 30, list_box["top"] + list_box["height"] * 0.4, "middle", 100)


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
        help="directory to write a PNG of the reply screen for each case into (default: none are kept)",
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
    # (label, width, height, a phone, the scheme)
    runs: tuple[tuple[str, int, int, bool, Scheme], ...] = (
        ("1280x800-dark", 1280, 800, False, "dark"),
        ("412x915-dark", 412, 915, True, "dark"),
        ("1280x800-light", 1280, 800, False, "light"),
        ("412x915-light", 412, 915, True, "light"),
        # `#185 reply-new-thread`, on a channel whose messages each head a thread nobody has answered.
        ("1280x800-new-thread", 1280, 800, False, "dark"),
        ("412x915-new-thread", 412, 915, True, "dark"),
        # `#267 quote-reply`, on long names and reply counts, at 393 and then 360 wide.
        ("393x852-quote-layout", 393, 852, True, "dark"),
    )
    report: list[str] = []
    with sync_playwright() as playwright:
        for label, width, height, phone, scheme in runs:
            api = (FakeApi(messages=layout_messages()) if label.endswith("quote-layout")
                   else FakeApi(empty_threads=label.endswith("new-thread")))
            server, thread, url = serve(api, web_root)
            try:
                with tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-reply-") as profile:
                    if phone:
                        context = playwright.chromium.launch_persistent_context(
                            profile, headless=True, executable_path=args.browser_executable,
                            viewport={"width": width, "height": height}, color_scheme=scheme,
                            user_agent=("Mozilla/5.0 (Linux; Android 15; Pixel 7) AppleWebKit/537.36 "
                                        "(KHTML, like Gecko) Chrome/151.0.0.0 Mobile Safari/537.36"),
                            device_scale_factor=2.625, is_mobile=True, has_touch=True,
                        )
                    else:
                        context = playwright.chromium.launch_persistent_context(
                            profile, headless=True, executable_path=args.browser_executable,
                            viewport={"width": width, "height": height}, color_scheme=scheme,
                        )
                    page = context.pages[0]
                    errors: list[str] = []
                    page.on("pageerror", lambda error: errors.append(str(error)))
                    page.goto(url, wait_until="load")
                    walk = Walk(page, api, phone, label, args.screenshots)
                    if label.endswith("quote-layout"):
                        quote_layout_walk(walk)
                    elif api.empty_threads:
                        new_thread_walk(walk)
                    elif scheme == "dark":
                        continuity_walk(walk, (100, 80, 130))
                        # Back at the reader's own size for the rest: a phone turned and back, a
                        # desk window made shorter, narrower and as it was.
                        page.evaluate("() => { const s = document.getElementById('msg-scale'); s.value = '100'; "
                                      "s.dispatchEvent(new Event('input', {bubbles: true})); }")
                        walk.settled()
                        turns = (((height, width), (width, height)) if phone else
                                 ((width, height - 220), (width, height), (720, height), (width, height)))
                        resize_case(walk, turns)
                        send_case(walk)
                        quote_case(walk)
                    else:
                        light_walk(walk)
                    check(not errors, f"{label}: the page threw: {errors}")
                    version = context.browser.version if context.browser else "Chromium"
                    context.close()
                    report.append(f"{version} at {label}: " + "; ".join(walk.notes))
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
