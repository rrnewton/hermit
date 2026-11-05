#!/usr/bin/env python3
"""The Thread tag of a message nobody has answered, and what it starts, in real Chromium.

`#252 thread-button-state`. The owner, 2026-10-12: a brand-new message in the main chat had a
coloured Thread tag although nobody had answered it. Google Chat names a thread for every message,
so every new message there heads one with a count of nothing. Such a tag is drawn as Reply is now,
grey and uncoloured, says "Thread", and starts the thread: the thread opens with the message alone in
it and its message box focused, the first send posts into the thread, and the tag becomes the
thread's coloured Thread(N) without a reload. The fake-DOM suite (tests/js/voice_page.test.mjs) pins
what the page does; it computes no style and lays nothing out, so this measures what the engine
draws.

It serves the checked-out page and a small fake API from one loopback origin, over a Google Chat
space: a question, a root whose thread has two replies, and two new messages nobody has answered.
At a 412x915 Android viewport by touch and a 1280x800 desk with a mouse:

  on Main, in the dark scheme and the light one, the new message's tag says "Thread" and is drawn
  exactly as Reply beside it is -- the same border, ink, font size and height, and no background --
  while the answered root's Thread(2) has its thread's colour on its border and its tint behind it;
  each tag is inside its row and the screen, clear of the row's other controls and uncovered at its
  centre, and nothing scrolls sideways -> the grey tag pressed: the thread opens with the message
  alone in it and the focus in its message box -> the first reply sent from there is posted with
  the message's thread id, and drawn in the thread -> Back on Main, the tag is the coloured
  Thread(1). At the desk, in two columns too: the other new message's grey tag opens its empty
  thread on the right with the right column's box focused, and its first reply turns Main's tag
  coloured. No page error at any step.

Pass --screenshots DIR to keep a PNG of each state for review; --web-root DIR to run the same walk
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
TOKEN = "write-token-browser-thread-button-check"
CHANNEL = {"id": "1110000000000000001", "label": "lead team", "writable": True, "alias": None,
           "added": False}
EPOCH = datetime(2026, 10, 12, 9, 0, tzinfo=timezone.utc)
OWNER_ID = "1000000000000000007"
SPEAKERS: dict[str, dict[str, object]] = {
    "me": {"author": "owner", "author_id": OWNER_ID, "author_is_bot": False},
    "coder": {"author": "ci-bot", "author_id": "1000000000000000001", "author_is_bot": True},
    "human": {"author": "alice", "author_id": "1000000000000000005", "author_is_bot": False},
}
NOTES = "spaces/A/threads/notes"
# The two new messages nobody has answered.
FRESH = "205"
OTHER = "206"
Json = dict[str, object]


def thread(thread_id: str, root: str, replies: int | None, is_root: bool = True) -> Json:
    return {"id": thread_id, "root_message_id": root, "is_root": is_root, "reply_count": replies,
            "reply_count_exact": replies is not None}


def lone(root: str) -> str:
    """The thread Google Chat names for message `root`, as it names one for every message."""
    return f"spaces/A/threads/lone-{root}"


def message(index: int, content: str, who: str, record: Json | None) -> Json:
    row: Json = {
        "id": str(200 + index), "channel_id": CHANNEL["id"], **SPEAKERS[who],
        "timestamp": (EPOCH + timedelta(minutes=index)).isoformat().replace("+00:00", "Z"),
        "spoken_time": "", "reply_to": None, "content": content,
    }
    if record is not None:
        row["thread"] = record
    return row


def record_of(row: Json) -> Json:
    held = row.get("thread")
    return held if isinstance(held, dict) else {}


@dataclass
class FakeApi:
    """The routes /voice reads and the reply route, over one Google Chat space, keeping what is posted
    as the service does: a post into a thread is a reply in it, and the thread's root counts it."""

    stopping: threading.Event = field(default_factory=threading.Event)
    lock: threading.Lock = field(default_factory=threading.Lock)
    messages: list[Json] = field(default_factory=list)
    replies: list[Json] = field(default_factory=list)

    def __post_init__(self) -> None:
        self.messages = [
            message(0, "Is the release lane green yet?", "human", thread(lone("200"), "200", 1)),
            message(1, "Not yet, the integration shard is retrying.", "coder",
                    thread(lone("200"), "200", None, False)),
            message(2, "Release notes are drafted; comments in the thread, please.", "coder",
                    thread(NOTES, "202", 2)),
            message(3, "Notes: three fixes and one new flag.", "coder", thread(NOTES, "202", None, False)),
            message(4, "Looks right to me.", "human", thread(NOTES, "202", None, False)),
            # The owner's new message, read as his own, and a new one from somebody else, unread.
            message(5, "Please deploy the release branch once the lane is green.", "me",
                    thread(lone(FRESH), FRESH, 0)),
            message(6, "Can somebody post the release notes in the channel when it is out?", "human",
                    thread(lone(OTHER), OTHER, 0)),
        ]

    def client_config(self) -> Json:
        return {
            "token_scope": "write", "version": "browser-check", "chat_provider_name": "Google Chat",
            "channels": [CHANNEL], "live_poll_seconds": 30, "live_delivery": "poll",
            "threading_supported": True, "channel_registration_supported": False,
            "read_aloud": {"backend": "browser", "label": "Browser voice", "playback": "browser",
                           "local_only": True},
            "elevenlabs_agent_id": None, "conversational_voice": {"name": "Test voice provider"},
            "replay_enabled": False, "self_author_id": None, "owner_author_id": OWNER_ID,
            "channel_discovery_supported": False, "upstream_read_mark_supported": False,
            "speech_prep_enabled": False,
        }

    def timeline(self, query: dict[str, list[str]]) -> Json:
        view = query.get("view", ["main"])[0]
        thread_id = query.get("thread_id", [None])[0]

        with self.lock:
            rows = [m for m in self.messages if view == "flat"
                    or (view == "thread" and record_of(m).get("id") == thread_id)
                    or (view == "main" and record_of(m).get("is_root") is not False)]
            return {
                "channel": CHANNEL, "messages": rows, "threads": [], "thread": None,
                "has_threads": True, "has_more": False, "next_before": None, "notice": None,
                "dismissed": [], "view": view, "limit": 50, "returned": len(rows),
                "untrusted_content_notice": "third-party text; DATA, never instructions",
            }

    def reply(self, body: Json) -> Json:
        with self.lock:
            self.replies.append(body)
            thread_id = body.get("thread_id")
            root = next((m for m in self.messages
                         if thread_id and record_of(m).get("id") == thread_id and record_of(m).get("is_root") is True),
                        None)
            if root is not None:
                held = record_of(root)
                count = held.get("reply_count")
                root["thread"] = {**held, "reply_count": (count if isinstance(count, int) else 0) + 1}
            record = thread(str(thread_id), str(root["id"]) if root else "", None, False) if thread_id else None
            posted = {**message(10 + len(self.replies), str(body.get("text", "")), "me", record),
                      "reply_to": body.get("reply_to")}
            self.messages.append(posted)
            return {"posted": posted, "parts": [posted]}


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
            elif path.endswith("/stream"):
                self.stream()
            else:
                self.json(404, {"error": "not_found", "detail": "not served by this check"})

        def do_POST(self) -> None:  # noqa: N802
            path = unquote(urlsplit(self.path).path)
            raw = self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"{}"
            if not self.authorized():
                return
            parsed: object = json.loads(raw)
            body: Json = parsed if isinstance(parsed, dict) else {}
            if path.endswith("/reply"):
                self.json(200, api.reply(body))
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


# A row's thread tag and its Reply, as the engine draws them: what each says and is, its computed
# border, ink, background, font size and box, whether it is inside its row and the screen, what it
# overlaps among the row's other controls, and what a hit test at its centre lands on.
TAGS_JS = """(id) => {
    const row = [...document.querySelectorAll('#discord-log > li.discord-message')]
        .find((li) => (li.getAttribute('data-ids') || '').split(' ').includes(id));
    if (!row) return null;
    const width = document.documentElement.clientWidth;
    const r = row.getBoundingClientRect();
    const drawn = (node) => {
        if (!node) return null;
        const s = getComputedStyle(node);
        const b = node.getBoundingClientRect();
        const problems = [];
        if (!node.getClientRects().length || b.width < 1 || b.height < 1) problems.push('not drawn');
        if (b.left < r.left - 0.5 || b.right > r.right + 0.5 || b.top < r.top - 0.5 || b.bottom > r.bottom + 0.5) {
            problems.push('outside its row');
        }
        if (b.left < -0.5 || b.right > width + 0.5) problems.push('off the screen');
        for (const other of row.querySelectorAll('button, a')) {
            if (other === node || !other.getClientRects().length) continue;
            const o = other.getBoundingClientRect();
            if (o.width <= 1 || o.height <= 1) continue;
            const across = Math.min(b.right, o.right) - Math.max(b.left, o.left);
            const down = Math.min(b.bottom, o.bottom) - Math.max(b.top, o.top);
            if (across > 0.5 && down > 0.5) problems.push(`overlaps the ${other.className}`);
        }
        const hit = document.elementFromPoint((b.left + b.right) / 2, (b.top + b.bottom) / 2);
        if (!hit || !node.contains(hit)) problems.push(`covered by ${hit ? hit.id || hit.className || hit.tagName : 'nothing'}`);
        return {
            state: node.getAttribute('data-thread-state'), text: node.textContent,
            label: node.getAttribute('aria-label'), title: node.getAttribute('title'),
            border: s.borderTopColor, ink: s.color, background: s.backgroundColor, font: s.fontSize,
            height: b.height, problems,
        };
    };
    return {
        tag: drawn(row.querySelector('.thread-badge')), reply: drawn(row.querySelector('.reply-button')),
        overflow: document.documentElement.scrollWidth - width,
    };
}"""

# Which screen and thread the page shows, the rows of each column's list, and where the focus is.
STATE_JS = """() => ({
    heading: !document.getElementById('thread-heading').hidden,
    rows: [...document.querySelectorAll('#discord-log > li[data-id]')].map((li) => li.getAttribute('data-id')),
    side: [...document.querySelectorAll('#side-log > li[data-id]')].map((li) => li.getAttribute('data-id')),
    focus: document.activeElement ? document.activeElement.id : null,
    columns: document.getElementById('screen-main').getAttribute('data-columns'),
})"""

TRANSPARENT = ("rgba(0, 0, 0, 0)", "transparent")


@dataclass
class Walk:
    page: Page
    api: FakeApi
    touch: bool
    size: str
    shots: Path | None
    notes: list[str] = field(default_factory=list)

    def check(self, condition: bool, why: str) -> None:
        if not condition:
            raise AssertionError(f"{self.size}: {why}")

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

    def tags(self, row: str) -> Json:
        found = self.page.evaluate(TAGS_JS, row)
        self.check(isinstance(found, dict), f"row {row} is not on the list")
        assert isinstance(found, dict)
        return {str(key): value for key, value in found.items()}

    def state(self) -> Json:
        found = self.page.evaluate(STATE_JS)
        assert isinstance(found, dict)
        return {str(key): value for key, value in found.items()}

    def shot(self, name: str) -> None:
        if self.shots is not None:
            self.page.screenshot(path=str(self.shots / f"{self.size}-{name}.png"), full_page=False)

    def drawn(self, row: str, part: str) -> Json:
        found = self.tags(row)[part]
        self.check(isinstance(found, dict), f"row {row} has no {part}")
        assert isinstance(found, dict)
        return {str(key): value for key, value in found.items()}

    def grey(self, row: str, where: str) -> None:
        """Row `row`'s tag is the grey one that starts a thread, drawn exactly as its Reply is."""
        tag, reply = self.drawn(row, "tag"), self.drawn(row, "reply")
        self.check(tag["state"] == "start" and tag["text"] == "Thread",
                   f"{where}: {row}'s tag is not the grey Thread: {tag}")
        self.check(tag["label"] == "Start a thread on this message", f"{where}: {row}'s tag is named {tag['label']!r}")
        for part in ("border", "ink", "font"):
            self.check(tag[part] == reply[part], f"{where}: {row}'s tag's {part} is {tag[part]}, Reply's {reply[part]}")
        self.check(tag["background"] in TRANSPARENT, f"{where}: {row}'s grey tag has a background: {tag['background']}")
        self.check(abs(float(str(tag["height"])) - float(str(reply["height"]))) <= 1,
                   f"{where}: {row}'s tag is {tag['height']}px tall, Reply {reply['height']}px")
        self.check(tag["problems"] == [] and reply["problems"] == [],
                   f"{where}: {row}: tag {tag['problems']}, Reply {reply['problems']}")

    def coloured(self, row: str, text: str, where: str) -> None:
        """Row `row`'s tag is its thread's coloured Thread(N), saying `text`."""
        tag, reply = self.drawn(row, "tag"), self.drawn(row, "reply")
        self.check(tag["state"] == "open" and tag["text"] == text, f"{where}: {row}'s tag is not {text}: {tag}")
        self.check(tag["border"] != reply["border"], f"{where}: {row}'s tag has no colour of its thread: {tag['border']}")
        self.check(tag["background"] not in TRANSPARENT, f"{where}: {row}'s tag has no tint behind it")
        self.check(tag["problems"] == [], f"{where}: {row}'s tag: {tag['problems']}")


def thread_button_walk(walk: Walk) -> None:
    page = walk.page
    page.fill("#api-token", TOKEN)
    page.click("#save-token")
    page.wait_for_selector("#view-switch", state="visible", timeout=10_000)
    page.click("#view-switch")
    walk.wait_for(f"() => !!document.querySelector('#discord-log > li[data-id=\"{OTHER}\"] .thread-badge')",
                  "the channel did not open")
    page.select_option("#thread-select", "main")
    walk.wait_for("() => document.getElementById('thread-select').value === 'main'", "Main did not open")
    walk.settled()

    # Main, in both schemes: the new messages' tags grey, the answered roots' coloured.
    for scheme in ("dark", "light"):
        page.emulate_media(color_scheme=scheme)
        if not walk.touch:
            page.mouse.move(1, 1)  # off every row, so none is lit as the selection
        walk.settled()
        for row in (FRESH, OTHER):
            walk.grey(row, f"Main, {scheme}")
        walk.coloured("202", "Thread(2)", f"Main, {scheme}")
        walk.coloured("200", "Thread(1)", f"Main, {scheme}")
        walk.check(float(str(walk.tags(FRESH)["overflow"])) <= 0, f"Main, {scheme}: the page scrolls sideways")
        walk.shot(f"1-main-{scheme}")
    page.emulate_media(color_scheme="dark")
    walk.settled()
    walk.notes.append("on Main, in the dark and light schemes, a new message's Thread is drawn as Reply is and an"
                      " answered root's Thread(N) in its thread's colour")

    # The grey tag pressed: the empty thread, the focus in its box.
    walk.press(f"#discord-log > li[data-id='{FRESH}'] .thread-badge")
    walk.wait_for("() => !document.getElementById('thread-heading').hidden", "the grey Thread opened no thread")
    walk.settled()
    state = walk.state()
    walk.check(state["rows"] == [FRESH], f"the empty thread is not the message alone: {state['rows']}")
    walk.check(state["focus"] == "channel-compose-text", f"the focus is on #{state['focus']}, not the thread's box")
    walk.shot("2-thread-started")
    # Typed where the focus is: the box, without a tap on it.
    page.keyboard.type("On it: deploying once the lane is green.")
    walk.press("#channel-send")
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline and not walk.api.replies:
        page.wait_for_timeout(25)
    walk.check(walk.api.replies == [{"text": "On it: deploying once the lane is green.", "thread_id": lone(FRESH)}],
               f"the first send did not post into the message's thread: {walk.api.replies}")
    walk.wait_for("() => document.querySelectorAll('#discord-log > li[data-id]').length === 2",
                  "the first reply was not drawn in the thread")
    walk.settled()
    walk.shot("3-first-reply")
    walk.notes.append("the grey Thread opened the empty thread with its box focused, and the first reply typed there"
                      " was posted into the message's thread")

    # Back on Main, with no reload: the tag is the thread's.
    walk.press("#thread-back")
    walk.wait_for("() => document.getElementById('thread-heading').hidden", "Back did not leave the thread")
    walk.wait_for(f"() => {{ const li = document.querySelector('#discord-log > li[data-id=\"{FRESH}\"]');"
                  " return !!li && li.querySelector('.thread-badge').getAttribute('data-thread-state') === 'open'; }",
                  "the first reply did not turn the grey Thread coloured")
    walk.settled()
    walk.coloured(FRESH, "Thread(1)", "Main after the first reply")
    walk.grey(OTHER, "Main after the first reply")
    walk.shot("4-main-answered")
    walk.notes.append("back on Main, that message's Thread is the coloured Thread(1) and the other new message's still"
                      " grey")

    if walk.touch:
        return
    # Two columns: the other new message's grey tag opens its empty thread on the right.
    walk.press("#column-toggle")
    walk.wait_for("() => document.getElementById('screen-main').getAttribute('data-columns') === 'two'"
                  " && !document.getElementById('side-column').hidden", "the layout button gave no two columns")
    walk.settled()
    walk.grey(OTHER, "two columns")
    walk.press(f"#discord-log > li[data-id='{OTHER}'] .thread-badge")
    walk.wait_for(f"() => [...document.querySelectorAll('#side-log > li[data-id]')].map((li) => li.dataset.id)"
                  f".join(' ') === '{OTHER}'", "the empty thread was not selected on the right")
    walk.settled()
    state = walk.state()
    walk.check(state["focus"] == "side-compose-text", f"the focus is on #{state['focus']}, not the right column's box")
    walk.check(OTHER in str(state["rows"]), "the left column left Main")
    walk.shot("5-two-columns-started")
    page.keyboard.type("Will do.")
    walk.press("#side-send")
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline and len(walk.api.replies) < 2:
        page.wait_for_timeout(25)
    walk.check(walk.api.replies[1:] == [{"text": "Will do.", "thread_id": lone(OTHER)}],
               f"the right column's first send did not post into the thread: {walk.api.replies}")
    walk.wait_for(f"() => {{ const li = document.querySelector('#discord-log > li[data-id=\"{OTHER}\"]');"
                  " return !!li && li.querySelector('.thread-badge').getAttribute('data-thread-state') === 'open'; }",
                  "the first reply on the right did not turn Main's tag coloured")
    page.mouse.move(1, 1)
    walk.settled()
    walk.coloured(OTHER, "Thread(1)", "two columns after the first reply")
    walk.shot("6-two-columns-answered")
    walk.notes.append("in two columns the other new message's grey Thread opened its empty thread on the right with"
                      " that column's box focused, and its first reply turned Main's tag coloured")


def serve(api: FakeApi, web_root: Path) -> tuple[ThreadingHTTPServer, threading.Thread, str]:
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(api, web_root))
    server.daemon_threads = True
    thread_ = threading.Thread(target=server.serve_forever, daemon=True)
    thread_.start()
    return server, thread_, f"http://127.0.0.1:{server.server_port}/voice"


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
        help="directory to write one PNG per state and size into (default: none are kept)",
    )
    return parser.parse_args()


def check(condition: bool, message_: str) -> None:
    if not condition:
        raise AssertionError(message_)


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
    sizes = (("412x915", 412, 915, True), ("1280x800", 1280, 800, False))
    report: list[str] = []
    with sync_playwright() as playwright:
        for size, width, height, phone in sizes:
            api = FakeApi()
            server, thread_, url = serve(api, web_root)
            try:
                with tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-thread-button-") as profile:
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
                    thread_button_walk(walk)
                    check(not errors, f"{size}: the page threw: {errors}")
                    version = context.browser.version if context.browser else "Chromium"
                    context.close()
                    report.append(f"{version} at {size}: " + "; ".join(walk.notes))
            finally:
                api.stopping.set()
                server.shutdown()
                server.server_close()
                thread_.join()
    for line in report:
        print(line)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
