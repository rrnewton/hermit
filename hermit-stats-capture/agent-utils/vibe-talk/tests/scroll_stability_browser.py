#!/usr/bin/env python3
"""Hold the message lists still in real Chromium while things change under them, at 1280x800 and 412x915.

`#280 scroll-stability`. The owner, 2026-10-13: "the thread view also feels like it is not starting out
scrolled all the way to the bottom... and it seems to jump around in ways that I don't like when I switch
focus around", and later: "I swear I just saw an instance where it jumped me to the bottom of the screen in
the main message view (two column mode)... I don't think I touched anything. I think it was something
updating underneath." The fake-DOM suite (tests/js/voice_page.test.mjs) holds the rules in a model that
lays out heights and has no scroll anchoring, no frames and no network wait. This measures them in the
engine: the scroll positions, the rows' boxes, and reads that really are out on the wire while the reader
scrolls.

It serves the checked-out page and a fake API from one loopback origin: a channel taller than the window,
Main over forty messages long, with seven threads summed up at more length than a card shows -- one of
them twenty-four replies long, every message in it Done, its root older than All's window, read a page of
eight at a time, and one whose newest reply alone is unread -- a live stream the check writes events
into, and summaries that answer after a moment. At a desk
in two columns with a mouse (1280x800) and on a phone in one column by touch (412x915), with the reader
scrolled up in Main and the first row on screen and its offset measured before and after each of these,
which must not move it by more than a pixel:

  a message arriving live, posted to the channel -> a reply arriving live in a thread whose root is in
  Main -> the read an edit asks for, which re-reads the channel as the poll does, the edited message far
  above the reader and taller after it -> THE OWNER'S JUMP: the reader on the newest line, that read held
  on the wire, the reader scrolling up while it is out, and the read let go -- before, it threw them back
  to the bottom -> Summaries turned on, each long row's summary answering a moment later, and off again
  -> the focus moved: at the desk the mouse over Main's rows, ] and [ between the columns, Tab and
  Shift+Tab, each held 2.5 s, and a card's replies button clicked, its thread opening on the right (a
  click on the card itself only highlights it, `#277 thread-card-focus`); on the phone Tab and
  Shift+Tab -> at the desk, sampled at every frame for 2.5 s, longer than the 1.5 s wait before the glide
  between the columns (`#261 two-column-connectors`), with Main scrolled so the card's root is a screen
  off it: THE OWNER'S 19:08, the mouse crossing the cards with nothing pressed on its way to the dock and
  resting there, the last card it passed left selected; the same up to the top bar; and [ then ] on its
  own, handing the keys to the right column onto a card on screen whose root is off Main's screen.

and then, at each size:

  a reader on the newest line follows a message arriving live, to the pixel -> at the desk the list of
  threads on the right opens at its foot, and stays there with the mouse over its last card and when a
  reply moves a thread to the foot -> the long thread, nothing in it unread, opens on its newest message
  with the list at its foot, and is still there a second and a half later; scrolled to its head, the page
  of older replies it reads in keeps the row the reader is on where it was -> the thread whose newest
  reply alone is unread opens at its foot too -> the page reloaded with its read held: the rows this
  device saved are up at once, the reader scrolls up, and the read let go leaves them where they are.

and last, at the desk, sampled at every frame for 2.5 s: a card highlighted by a click, whose root is a
screen off Main, and the mouse going on over another card to the dock leaves Main still; coming back to
rest on the highlighted card, the card the glide follows, brings its root on screen.

No horizontal overflow and no page error at any step. Pass --screenshots DIR to keep a PNG of each stage;
--web-root DIR to run the walk against another copy of the page.
"""

from __future__ import annotations

import argparse
import json
import os
import queue
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import TYPE_CHECKING

import offline_cache_browser as fixture
from offline_cache_browser import CHANNEL, TOKEN, FakeApi, Json, check, handler_for, said, thread_of

if TYPE_CHECKING:
    from playwright.sync_api import BrowserType, Page

# (label, width, height, touch): a desk in two columns with a mouse, and a phone in one by touch.
SIZES = (("desk-1280", 1280, 800, False), ("phone-412", 412, 915, True))

FILLER = ("The overnight run is still going; the integration shard was retried and passed, and the artifact "
          "upload waited on the cache for most of an hour. ")

# What the summariser said of every thread: longer than a card's three lines at a desk.
SUMMARY = ("The overnight runner hung twice, the shard was retried by hand, the artifact upload waited on the "
           "cache, and the alert silenced on Friday is back on; the dashboards picked up the new partitions.")
# The long thread: its root and twenty-four replies, every one Done, its root older than All's window.
LONG = "spaces/A/threads/long"
LONG_ROOT = 3
LONG_REPLIES = range(4, 28)
# Main's own messages after it, every seventh the root of a small thread with replies of its own.
MAIN_FROM, MAIN_TO = 28, 70
SMALL_ROOTS = tuple(index for index in range(MAIN_FROM, MAIN_TO) if index % 7 == 0)
# The small thread whose newest reply alone is unread.
FRESH_ROOT = 63
FRESH = f"spaces/A/threads/t{FRESH_ROOT}"
# How many messages each view reads at a time: a thread eight, All sixty, which reach back to the long
# thread's last few replies and not its root, so it is read for itself, and Main the lot.
THREAD_PAGE = 8
FLAT_PAGE = 60
# How far a row may move and still be said to have stood still: sub-pixel layout, never a jump.
STILL_PX = 1.0
# How long a step that could start the page's glide between the columns (`#261 two-column-connectors`,
# COLUMN_GLIDE_MS = 1500 in web/voice.js) is held before it is judged: longer than that wait, with room.
GLIDE_HOLD_MS = 2500

# The first row of a list whose foot is below the top of its scroller: the row the reader is on.
FIRST_ROW_JS = """(list) => {
    const area = document.getElementById(list === 'discord-log' ? 'scroll-area' : 'side-scroll');
    const top = area.getBoundingClientRect().top;
    for (const row of document.getElementById(list).children) {
        if (!row.hasAttribute('data-id') || !row.getClientRects().length) continue;
        const box = row.getBoundingClientRect();
        if (box.bottom > top) return {id: row.getAttribute('data-id'), offset: box.top - top};
    }
    return null;
}"""

# How far the list runs below what its scroller shows: 0 at its foot.
GAP_JS = """(id) => { const area = document.getElementById(id);
    return area.scrollHeight - area.scrollTop - area.clientHeight; }"""

# Main's first row on screen and its offset, sampled at every frame until UNWATCH_JS: a jump that snaps
# back between two measurements shows here.
WATCH_JS = """() => {
    const area = document.getElementById('scroll-area');
    const list = document.getElementById('discord-log');
    const first = () => {
        const top = area.getBoundingClientRect().top;
        for (const row of list.children) {
            if (!row.hasAttribute('data-id') || !row.getClientRects().length) continue;
            const box = row.getBoundingClientRect();
            if (box.bottom > top) return [row.getAttribute('data-id'), box.top - top];
        }
        return [null, 0];
    };
    const watch = {samples: [], on: true};
    window.__scrollWatch = watch;
    const tick = () => { if (!watch.on) return; watch.samples.push(first()); requestAnimationFrame(tick); };
    tick();
}"""
UNWATCH_JS = "() => { const watch = window.__scrollWatch; watch.on = false; return watch.samples; }"

# The card of a thread: where the middle of what shows of it is, and how much of it shows.
CARD_AT_JS = """(id) => {
    const card = document.querySelector(`#side-cards > li[data-id='${id}']`);
    if (!card) return null;
    const box = (card.querySelector('.side-card') || card).getBoundingClientRect();
    const side = document.getElementById('side-scroll').getBoundingClientRect();
    const top = Math.max(box.top, side.top);
    const bottom = Math.min(box.bottom, side.bottom);
    return {x: (box.left + box.right) / 2, y: (top + bottom) / 2, shown: bottom - top};
}"""

# Main scrolled so that the row `id` is a screen off it: below, where Main reaches that far, else above.
ROOT_AWAY_JS = """(id) => {
    const area = document.getElementById('scroll-area');
    const row = document.querySelector(`#discord-log > li[data-id='${id}']`);
    const room = area.clientHeight;
    const at = row.getBoundingClientRect().top - area.getBoundingClientRect().top + area.scrollTop;
    let top = at - 2 * room;
    if (top < 200) top = at + row.getBoundingClientRect().height + room;
    area.scrollTop = Math.round(Math.min(top, area.scrollHeight - room - 500));
    return area.scrollTop;
}"""

# Whether Main's row `id` is off its screen.
ROW_AWAY_JS = """(id) => {
    const area = document.getElementById('scroll-area').getBoundingClientRect();
    const row = document.querySelector(`#discord-log > li[data-id='${id}']`);
    if (!row) return true;
    const box = row.getBoundingClientRect();
    return box.bottom < area.top || box.top > area.bottom;
}"""

# The card drawn selected in the right column, or null.
LIT_CARD_JS = """() => { const lit = document.querySelector('#side-cards > li[data-selected]');
    return lit ? lit.getAttribute('data-id') : null; }"""


def root_of(thread_id: str) -> str:
    """The id of the message heading `thread_id`, which is its row in Main."""
    return str(200 + (LONG_ROOT if thread_id == LONG else int(thread_id.rsplit("t", 1)[1])))


def thread_record(thread_id: str, root: int, replies: int, is_root: bool) -> Json:
    return {"id": thread_id, "root_message_id": str(200 + root), "is_root": is_root,
            "reply_count": replies, "reply_count_exact": True}


class StabilityApi(FakeApi):
    """The channel, a live stream the check writes events into, and summaries that answer a moment later."""

    def __init__(self) -> None:
        super().__init__()
        self.frames: queue.Queue[bytes] = queue.Queue()
        self.summary_delay = 0.3
        self.summaries_asked = 0
        self.messages = []
        self.threads = []
        self.dismissed: set[str] = set()
        replies: dict[str, list[int]] = {LONG: list(LONG_REPLIES)}
        for index in range(MAIN_TO):
            # Two speakers in turn, so that no two messages combine into one row.
            who = "human" if index % 2 else "coder"
            if index == LONG_ROOT:
                row = said(index, f"Message {200 + index}. The long thread starts here. {FILLER}", who)
                row["thread"] = thread_record(LONG, index, len(LONG_REPLIES), True)
            elif index in LONG_REPLIES:
                row = said(index, f"Reply {200 + index} in the long thread. {FILLER * (1 + index % 2)}", who)
                row["thread"] = thread_record(LONG, LONG_ROOT, len(LONG_REPLIES), False)
            elif index in SMALL_ROOTS:
                thread_id = f"spaces/A/threads/t{index}"
                count = 3 if index == FRESH_ROOT else 2
                row = said(index, f"Message {200 + index}. A thread starts here. {FILLER}", who)
                row["thread"] = thread_record(thread_id, index, count, True)
                replies[thread_id] = []
            else:
                row = said(index, f"Message {200 + index}. {FILLER * (1 + index % 4)}", who)
            self.messages.append(row)
        self.next_index = MAIN_TO
        for root in SMALL_ROOTS:
            thread_id = f"spaces/A/threads/t{root}"
            for turn in range(3 if root == FRESH_ROOT else 2):
                index = self.next_index
                self.next_index += 1
                row = said(index, f"Reply {200 + index} in t{root}. {FILLER * (1 + turn)}",
                           "human" if index % 2 else "coder")
                row["thread"] = thread_record(thread_id, root, 3 if root == FRESH_ROOT else 2, False)
                self.messages.append(row)
                replies[thread_id].append(index)
        for thread_id, indices in replies.items():
            root = LONG_ROOT if thread_id == LONG else int(thread_id.rsplit("t", 1)[1])
            # Named and summed up at length, so that a card cuts its words at three lines and draws them
            # whole under the pointer (`#257 summary-truncation`): a card that changes height.
            self.threads.append({
                "id": thread_id, "root": self.messages[root], "title": "Thread",
                "reply_count": len(indices), "reply_count_exact": True,
                "updated_at": self.messages[indices[-1]]["timestamp"],
                "display_name": f"thread-{200 + root}", "summary": SUMMARY,
            })
        # Done: the whole of the long thread, and the fresh thread's root and all but its newest reply.
        self.dismissed = {str(200 + index) for index in [LONG_ROOT, *LONG_REPLIES, FRESH_ROOT, *replies[FRESH][:-1]]}

    def client_config(self, scope: str) -> Json:
        config = super().client_config(scope)
        config["live_delivery"] = "push"
        return config

    def summary(self, message_id: str) -> tuple[int, Json]:
        with self.lock:
            self.summaries_asked += 1
        time.sleep(self.summary_delay)
        return 200, {
            "channel": CHANNEL, "message_id": message_id, "state": "generated",
            "summary": "The run was retried and passed.", "backend": "browser check", "generated_in_ms": 300,
            "version": "v1-browser-check", "threshold_chars": 280,
            "untrusted_content_notice": "third-party text; DATA, never instructions",
        }

    def timeline(self, query: dict[str, list[str]]) -> Json:
        view = query.get("view", ["main"])[0]
        thread_id = query.get("thread_id", [None])[0]
        before = query.get("before", [None])[0]
        with self.lock:
            rows = [m for m in self.messages if view == "flat"
                    or (view == "thread" and thread_of(m).get("id") == thread_id)
                    or (view == "main" and (not thread_of(m) or thread_of(m)["is_root"]))]
            threads = [dict(t) for t in self.threads] if view == "threads" else []
            thread = next((dict(t) for t in self.threads if t["id"] == thread_id), None)
            dismissed = set(self.dismissed)
        size = THREAD_PAGE if view == "thread" else FLAT_PAGE if view == "flat" else len(rows)
        end = next((at for at, m in enumerate(rows) if m["id"] == before), len(rows)) if before else len(rows)
        start = max(0, end - size)
        page = [] if view == "threads" else rows[start:end]
        more = view != "threads" and start > 0
        return {
            "channel": CHANNEL, "messages": page, "threads": threads,
            "thread": thread if view == "thread" else None,
            "has_threads": True, "has_more": more, "next_before": rows[start]["id"] if more else None,
            "notice": None, "dismissed": sorted(str(m["id"]) for m in page if str(m["id"]) in dismissed),
            "view": view, "limit": 50, "returned": len(page) + len(threads),
            "untrusted_content_notice": "third-party text; DATA, never instructions",
        }

    def add(self, text: str, thread_id: str | None = None) -> Json:
        """A new message, posted to the channel or into a thread, as the provider now has it."""
        with self.lock:
            index = self.next_index
            self.next_index += 1
            row = said(index, text, "human" if index % 2 else "coder")
            if thread_id is not None:
                summary = next(t for t in self.threads if t["id"] == thread_id)
                count = int(str(summary["reply_count"])) + 1
                summary["reply_count"] = count
                summary["updated_at"] = row["timestamp"]
                root = summary["root"]
                assert isinstance(root, dict)
                root_thread = root["thread"]
                assert isinstance(root_thread, dict)
                root["thread"] = {**root_thread, "reply_count": count}
                row["thread"] = {**root_thread, "is_root": False, "reply_count": count}
            self.messages.append(row)
        return row

    def edit(self, index: int, text: str) -> Json:
        """Message `200 + index`, edited to `text`."""
        with self.lock:
            row = next(m for m in self.messages if m["id"] == str(200 + index))
            row["content"] = text
            return dict(row)

    def push(self, event: str, payload: Json, event_id: str) -> None:
        """One frame onto the live stream, as src/live.rs writes it."""
        body = {"self_posted": False, "replayed": False, "from_tail": False,
                "untrusted_content_notice": "third-party text; DATA, never instructions", **payload}
        self.frames.put(f"id: {event_id}\nevent: {event}\ndata: {json.dumps(body)}\n\n".encode())

    def timeline_requests(self) -> int:
        with self.lock:
            return sum(1 for request in self.requests if "/timeline?" in request)


def handler(api: StabilityApi) -> type[BaseHTTPRequestHandler]:
    """The fixture's routes, with a live stream that delivers what the check writes into it."""

    class Handler(handler_for(api)):  # type: ignore[misc]
        def stream(self) -> None:
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            try:
                while not api.stopping.is_set():
                    try:
                        frame = api.frames.get(timeout=0.5)
                    except queue.Empty:
                        frame = b": keep-alive\n\n"
                    self.wfile.write(frame)
                    self.wfile.flush()
            except OSError:
                pass
            self.close_connection = True

    return Handler


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
        default=fixture.WEB_ROOT,
        help=f"directory holding the page to serve (default: the checked-out {fixture.WEB_ROOT})",
    )
    parser.add_argument(
        "--screenshots",
        type=Path,
        help="directory to write one PNG per stage into (default: none are kept)",
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
    if args.screenshots:
        args.screenshots.mkdir(parents=True, exist_ok=True)
    fixture.WEB_ROOT = args.web_root.resolve()
    report: list[str] = []
    with sync_playwright() as playwright:
        for label, width, height, touch in SIZES:
            report.append(walk(playwright.chromium, args, label, width, height, touch))
    for line in report:
        print(line)
    return 0


class Reader:
    """The page at one size, the fake API behind it, and what the walk has found there."""

    def __init__(self, page: Page, api: StabilityApi, screenshots: Path | None, label: str, touch: bool) -> None:
        self.page = page
        self.api = api
        self.screenshots = screenshots
        self.label = label
        self.touch = touch
        self.notes: list[str] = []

    def check(self, condition: bool, why: str) -> None:
        check(condition, f"{self.label}: {why}")

    def shot(self, name: str) -> None:
        if self.screenshots:
            self.page.screenshot(path=str(self.screenshots / f"{self.label}-{name}.png"))

    def frames(self, count: int = 2) -> None:
        for _ in range(count):
            self.page.evaluate("() => new Promise((done) => requestAnimationFrame(() => done()))")

    def wait_for(self, condition: str, why: str, seconds: float = 10) -> None:
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline and not self.page.evaluate(condition):
            self.page.wait_for_timeout(25)
        self.check(bool(self.page.evaluate(condition)), why)

    def quiet(self, settle_ms: int = 400) -> None:
        """Let every read, summary and frame the last act started land, and the page draw what they brought."""
        self.page.wait_for_timeout(settle_ms)
        self.frames(3)

    def first(self, list_id: str = "discord-log") -> tuple[str, float]:
        found = self.page.evaluate(FIRST_ROW_JS, list_id)
        self.check(isinstance(found, dict), f"no row of #{list_id} is on screen")
        assert isinstance(found, dict)
        return str(found["id"]), float(found["offset"])

    def gap(self, scroller: str) -> float:
        return float(self.page.evaluate(GAP_JS, scroller))

    def scroll_main(self, fraction: float) -> None:
        """Main's list scrolled to `fraction` of its height, as a reader leaves it."""
        self.page.evaluate("(at) => { const area = document.getElementById('scroll-area');"
                           " area.scrollTop = Math.round((area.scrollHeight - area.clientHeight) * at); }", fraction)
        self.quiet(250)

    def still(self, before: tuple[str, float], why: str, list_id: str = "discord-log") -> None:
        """The row the reader was on is still the first on screen, within a pixel of where it was."""
        after = self.first(list_id)
        self.check(after[0] == before[0] and abs(after[1] - before[1]) <= STILL_PX,
                   f"{why} moved the reader: row {before[0]} at {before[1]:.1f}px became row {after[0]} at {after[1]:.1f}px")

    def watch(self) -> None:
        """Start sampling Main's first row on screen and its offset at every frame."""
        self.page.evaluate(WATCH_JS)

    def held(self, before: tuple[str, float], why: str, hold_ms: int = GLIDE_HOLD_MS) -> None:
        """Wait `hold_ms`, then: at every frame since `watch`, the row the reader was on was the first on
        screen, within a pixel of where it was."""
        self.page.wait_for_timeout(hold_ms)
        self.frames(2)
        samples = self.page.evaluate(UNWATCH_JS)
        self.check(len(samples) > 10, f"{why}: only {len(samples)} frames were sampled")
        for row, offset in samples:
            self.check(row == before[0] and abs(float(offset) - before[1]) <= STILL_PX,
                       f"{why} moved the reader: row {before[0]} at {before[1]:.1f}px became row {row} at "
                       f"{float(offset):.1f}px within {hold_ms}ms")
        self.still(before, why)

    def overflow(self) -> None:
        wide = float(self.page.evaluate("() => document.documentElement.scrollWidth - window.innerWidth"))
        self.check(wide <= 0, f"the page scrolls sideways by {wide}px")


def walk(chromium: BrowserType, args: argparse.Namespace, label: str, width: int, height: int, touch: bool) -> str:
    """One reader at one size, against a fresh fake API and a fresh browser profile."""
    api = StabilityApi()
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler(api))
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-scroll-") as profile:
            if touch:
                context = chromium.launch_persistent_context(
                    profile, headless=True, executable_path=args.browser_executable,
                    viewport={"width": width, "height": height}, color_scheme="dark",
                    device_scale_factor=2.625, is_mobile=True, has_touch=True,
                )
            else:
                context = chromium.launch_persistent_context(
                    profile, headless=True, executable_path=args.browser_executable,
                    viewport={"width": width, "height": height}, color_scheme="dark",
                )
            version = context.browser.version if context.browser else "Chromium"
            page = context.pages[0]
            errors: list[str] = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            reader = Reader(page, api, args.screenshots, label, touch)
            page.goto(f"http://127.0.0.1:{server.server_port}/voice", wait_until="load")
            open_main(reader)
            main_walk(reader)
            if not touch:
                glide_walk(reader)
            follow_walk(reader)
            if not touch:
                cards_walk(reader)
            thread_walk(reader)
            reload_walk(reader, f"http://127.0.0.1:{server.server_port}/voice")
            if not touch:
                highlight_walk(reader)
            reader.check(not errors, f"the page threw: {errors}")
            context.close()
            return f"{version} at {width}x{height}: " + "; ".join(reader.notes)
    finally:
        api.stopping.set()
        api.timeline_gate.set()
        server.shutdown()
        server.server_close()
        thread.join()


def open_main(reader: Reader) -> None:
    """Sign in and open the channel's Main: in the left column of two at the desk, the one view on the phone."""
    page = reader.page
    page.fill("#api-token", TOKEN)
    page.click("#save-token")
    page.wait_for_selector("#search-toggle", state="visible", timeout=10_000)
    page.click("#view-switch")
    reader.wait_for("() => document.querySelectorAll('#discord-log > li[data-id]').length > 0",
                    "the channel did not open")
    if reader.touch:
        page.select_option("#thread-select", "main")
    else:
        page.click("#column-toggle")
        reader.wait_for("() => document.getElementById('side-column').getClientRects().length > 0"
                        " && document.querySelectorAll('#side-cards > li').length > 0",
                        "pressing the layout button did not draw the right column's cards")
    reader.wait_for("() => document.getElementById('thread-select').value === 'main'"
                    f" && document.querySelectorAll('#discord-log > li[data-id]').length >= {MAIN_TO - MAIN_FROM}",
                    "Main is not the list on the left")
    reader.quiet(800)
    reader.check(reader.gap("scroll-area") <= 1, "Main did not open on its newest line")
    reader.overflow()


def main_walk(reader: Reader) -> None:
    """The reader scrolled up in Main, and everything that happens underneath, one at a time."""
    page, api = reader.page, reader.api
    reader.scroll_main(0.45)
    reader.check(reader.gap("scroll-area") > 400, "Main is too short to scroll up in, so this proves nothing")
    reader.shot("1-main-scrolled-up")

    # A message posted to the channel, arriving live: its row goes in below, and the read behind it lands.
    before = reader.first()
    reads = api.timeline_requests()
    arrived = api.add(f"A message posted to the channel while the reader is up the list. {FILLER}")
    api.push("message", {"message": arrived}, str(arrived["id"]))
    reader.wait_for(f"() => !!document.querySelector(\"#discord-log > li[data-id='{arrived['id']}']\")",
                    "the live message never reached Main")
    deadline = time.monotonic() + 5
    while api.timeline_requests() <= reads and time.monotonic() < deadline:
        page.wait_for_timeout(25)
    reader.quiet()
    reader.still(before, "a message arriving live")
    reader.check(bool(page.evaluate("() => document.getElementById('jump-newest').hasAttribute('data-arrivals')")),
                 "nothing offered the reader up the list the message that arrived")
    reader.notes.append("a live message left the row on screen where it was and offered the jump")

    # A reply arriving live in a thread whose root is in Main: its count changes, and on the desk its card
    # moves to the foot of the right column.
    before = reader.first()
    reply = api.add(f"A reply arriving live in a thread. {FILLER}", f"spaces/A/threads/t{SMALL_ROOTS[1]}")
    api.push("message", {"message": reply}, str(reply["id"]))
    reader.quiet(900)
    reader.still(before, "a reply arriving live in a thread")
    reader.notes.append("so did a live reply in a thread")

    # The read an edit asks for, which re-reads the channel as the poll does: the edited message far above
    # the reader, and taller after the edit.
    before = reader.first()
    edited = api.edit(MAIN_FROM + 1, f"Message {200 + MAIN_FROM + 1}, edited and longer. {FILLER * 5}")
    api.push("message_update", {"message": edited}, "edit-1")
    reader.wait_for(f"() => document.querySelector(\"#discord-log > li[data-id='{200 + MAIN_FROM + 1}']\")"
                    ".textContent.includes('edited and longer')", "the read an edit asks for never landed")
    reader.quiet()
    reader.still(before, "a refresh redrawing the list, with a message above the reader grown")
    reader.notes.append("a refresh with a message grown above left it where it was")

    # THE OWNER'S JUMP. On the newest line when a read goes out; scrolled up while it is out; the read lands.
    reader.scroll_main(1.0)
    reader.check(reader.gap("scroll-area") <= 1, "the reader is not on the newest line")
    api.timeline_gate.clear()
    reads = api.timeline_requests()
    edited = api.edit(MAIN_FROM + 2, f"Message {200 + MAIN_FROM + 2}, edited while the reader reads. {FILLER}")
    api.push("message_update", {"message": edited}, "edit-2")
    deadline = time.monotonic() + 5
    while api.timeline_requests() <= reads and time.monotonic() < deadline:
        page.wait_for_timeout(25)
    reader.check(api.timeline_requests() > reads, "the edit asked for no read")
    box = page.locator("#scroll-area").bounding_box()
    assert box is not None
    if reader.touch:
        reader.scroll_main(0.4)
    else:
        page.mouse.move(box["x"] + box["width"] / 2, box["y"] + box["height"] / 2)
        page.mouse.wheel(0, -2400)
        reader.quiet(400)
    reader.check(reader.gap("scroll-area") > 400, "the reader did not scroll up while the read was out")
    before = reader.first()
    api.timeline_gate.set()
    reader.wait_for(f"() => document.querySelector(\"#discord-log > li[data-id='{200 + MAIN_FROM + 2}']\")"
                    ".textContent.includes('edited while the reader reads')", "the held read never landed")
    reader.quiet()
    reader.check(reader.gap("scroll-area") > 400, "the read that landed threw the reader back to the newest line")
    reader.still(before, "a read sent while the reader was on the newest line, landing after they scrolled up")
    reader.shot("2-after-held-read")
    reader.notes.append("a read sent from the newest line and landing after the reader scrolled up left them there")

    # Summaries on: each long row on screen is asked about, and its summary comes a moment later.
    reader.scroll_main(0.5)
    before = reader.first()
    asked = api.summaries_asked
    page.click("#summarise")
    deadline = time.monotonic() + 5
    while api.summaries_asked <= asked and time.monotonic() < deadline:
        page.wait_for_timeout(25)
    reader.check(api.summaries_asked > asked, "Summaries on asked for no summary")
    reader.quiet(int(api.summary_delay * 1000) + 900)
    reader.still(before, "summaries arriving")
    page.click("#summarise")
    reader.quiet()
    reader.still(before, "Summaries off")
    reader.notes.append(f"{api.summaries_asked - asked} summaries arriving, and Summaries off, left it where it was")

    # The focus moved around.
    before = reader.first()
    if reader.touch:
        for key in ("Tab", "Tab", "Shift+Tab"):
            page.keyboard.press(key)
            reader.quiet(150)
            reader.still(before, f"{key}")
        reader.notes.append("Tab and Shift+Tab left it where it was")
        return
    page.mouse.move(box["x"] + 120, box["y"] + 200)
    page.mouse.move(box["x"] + 140, box["y"] + 260)
    reader.quiet(200)
    reader.still(before, "the mouse moving over Main's rows")
    for key in ("]", "[", "Tab", "Shift+Tab"):
        reader.watch()
        page.keyboard.press(key)
        reader.held(before, f"{key}, held {GLIDE_HOLD_MS}ms")
    # The card's own replies button is the way into its thread (`#277 thread-card-focus`): a click on the
    # card itself only highlights it.
    card = page.locator(f"#side-cards > li[data-id='spaces/A/threads/t{SMALL_ROOTS[2]}'] .side-card-open")
    card.click()
    reader.wait_for("() => document.querySelectorAll('#side-log > li[data-id]').length > 0",
                    "the card did not open its thread on the right")
    reader.quiet(1800)
    reader.still(before, "a card clicked, its thread opening on the right")
    page.keyboard.press("Escape")
    reader.wait_for("() => document.getElementById('side-pane').hidden", "Esc did not go back to the cards")
    reader.quiet(300)
    reader.still(before, "Esc back to the cards")
    reader.notes.append("the mouse over Main, ] [ Tab Shift+Tab, a card opening its thread and Esc left it where it was")


def side_to_foot(reader: Reader) -> None:
    """The list of threads on the right scrolled to its foot, where it opens."""
    reader.page.evaluate("() => { const area = document.getElementById('side-scroll'); area.scrollTop = area.scrollHeight; }")
    reader.quiet(250)


def cross_card(reader: Reader, thread_id: str, start: tuple[float, float], end: tuple[float, float], why: str) -> None:
    """The mouse from `start` in Main across the right column to `thread_id`'s card and on to `end`, which is no
    card; nothing pressed. Main, scrolled with that card's root a screen off it, holds still for the glide's
    wait and more, though the card stays selected."""
    page = reader.page
    root = root_of(thread_id)
    page.mouse.move(*start)
    page.evaluate(ROOT_AWAY_JS, root)
    reader.quiet(250)
    card = page.evaluate(CARD_AT_JS, thread_id)
    reader.check(isinstance(card, dict) and float(card["shown"]) > 30, f"the card of {thread_id} is not on screen: {card}")
    assert isinstance(card, dict)
    before = reader.first()
    reader.watch()
    page.mouse.move(float(card["x"]), float(card["y"]), steps=8)
    page.mouse.move(*end, steps=4)
    reader.held(before, why)
    # It proves something only with the card the pointer passed over left selected, its root off Main's
    # screen: before the fix, the glide brought that root on screen 1.5 s after the pointer had gone.
    lit = page.evaluate(LIT_CARD_JS)
    reader.check(lit == thread_id, f"{why}: the card left selected is {lit}, not {thread_id}, so this proves less")
    reader.check(bool(page.evaluate(ROW_AWAY_JS, root)), f"{why}: the card's root came on screen")


def glide_walk(reader: Reader) -> None:
    """At the desk: the mouse crossing the cards with nothing pressed, and ] handing the keys to the right column,
    each held past the 1.5 s glide between the columns (`#261 two-column-connectors`), never move Main."""
    page = reader.page
    reader.wait_for("() => !document.getElementById('side-list').hidden", "the right column is not its cards")
    size = page.viewport_size
    assert size is not None
    width, height = size["width"], size["height"]
    cards = [str(card) for card in page.evaluate("() => [...document.querySelectorAll('#side-cards > li[data-id]')]"
                                                 ".map((li) => li.getAttribute('data-id'))")]
    reader.check(len(cards) >= 3, f"too few cards to cross: {cards}")

    # THE OWNER'S 19:08: down across the cards to the dock, the last card the pointer passes at the foot.
    side_to_foot(reader)
    cross_card(reader, cards[-1], (300, 300), (width - 180, height - 5),
               "the mouse crossing the cards to the dock, nothing pressed, then resting there")
    reader.notes.append(f"the mouse crossing the cards to the dock and resting there {GLIDE_HOLD_MS}ms left Main still")

    # Up across them to the top bar, the last card passed the first, the list scrolled to its head.
    page.evaluate("() => { document.getElementById('side-scroll').scrollTop = 0; }")
    reader.quiet(250)
    cross_card(reader, cards[0], (400, 500), (width - 10, 10),
               "the mouse crossing the cards to the top bar, nothing pressed, then resting there")
    reader.notes.append("so did crossing them to the top bar")

    # ] on its own: the right column's selection a card on screen whose root is off Main's screen, the
    # pointer resting on neither column.
    side_to_foot(reader)
    page.mouse.move(5, height - 5)
    card = page.evaluate(CARD_AT_JS, cards[-1])
    assert isinstance(card, dict)
    page.mouse.move(float(card["x"]) - 4, float(card["y"]) - 4)
    page.mouse.move(float(card["x"]), float(card["y"]), steps=2)
    page.mouse.move(5, height - 5)
    reader.check(page.evaluate(LIT_CARD_JS) == cards[-1], "the mouse did not leave the last card selected")
    reader.quiet(GLIDE_HOLD_MS)
    page.evaluate(ROOT_AWAY_JS, root_of(cards[-1]))
    reader.quiet(250)
    before = reader.first()
    reader.watch()
    page.keyboard.press("[")
    reader.held(before, f"[ to Main, held {GLIDE_HOLD_MS}ms")
    side_to_foot(reader)
    before = reader.first()
    reader.watch()
    page.keyboard.press("]")
    reader.held(before, f"] to the right column on its own, held {GLIDE_HOLD_MS}ms")
    lit = page.evaluate(LIT_CARD_JS)
    reader.check(lit == cards[-1], f"] lit {lit}, not {cards[-1]}, so this proves less")
    reader.check(bool(page.evaluate(ROW_AWAY_JS, root_of(cards[-1]))), "] brought the card's root on screen")
    reader.notes.append(f"[ and then ] onto a card whose root is off screen, each held {GLIDE_HOLD_MS}ms, left Main still")
    page.keyboard.press("[")
    side_to_foot(reader)
    reader.quiet(300)


def highlight_walk(reader: Reader) -> None:
    """At the desk, last: a card highlighted by a click (`#277 thread-card-focus`) is what the glide follows,
    and still only while the mouse rests on it. The mouse going on over another card to the dock leaves Main
    still, and coming back to rest on the highlighted card brings its root on screen. Last, because the
    clicks leave the focus on a card and Main glided, which the walks before it do not expect."""
    page = reader.page
    reader.wait_for("() => !document.getElementById('side-list').hidden"
                    " && document.querySelectorAll('#side-cards > li[data-id]').length >= 2",
                    "the right column is not its cards")
    size = page.viewport_size
    assert size is not None
    width, height = size["width"], size["height"]
    cards = [str(card) for card in page.evaluate("() => [...document.querySelectorAll('#side-cards > li[data-id]')]"
                                                 ".map((li) => li.getAttribute('data-id'))")]
    lit_id, other_id = cards[-1], cards[-2]
    side_to_foot(reader)
    page.mouse.move(5, height - 5)
    page.evaluate(ROOT_AWAY_JS, root_of(lit_id))
    reader.quiet(250)
    card = page.evaluate(CARD_AT_JS, lit_id)
    other = page.evaluate(CARD_AT_JS, other_id)
    reader.check(isinstance(card, dict) and float(card["shown"]) > 30, f"the card of {lit_id} is not on screen: {card}")
    reader.check(isinstance(other, dict) and float(other["shown"]) > 30, f"the card of {other_id} is not on screen: {other}")
    assert isinstance(card, dict) and isinstance(other, dict)
    before = reader.first()
    reader.watch()
    page.mouse.move(float(card["x"]), float(card["y"]), steps=4)
    page.mouse.down()
    page.mouse.up()
    highlighted = page.evaluate("() => [...document.querySelectorAll('#side-cards > li[data-highlighted]')]"
                                ".map((li) => li.getAttribute('data-id'))")
    reader.check(highlighted == [lit_id], f"the click highlighted {highlighted}, not {lit_id}, so this proves nothing")
    other = page.evaluate(CARD_AT_JS, other_id)
    assert isinstance(other, dict)
    page.mouse.move(float(other["x"]), float(other["y"]), steps=4)
    page.mouse.move(width - 180, height - 5, steps=4)
    reader.held(before, "a card highlighted by a click, the mouse going on over another card to the dock")
    reader.check(bool(page.evaluate(ROW_AWAY_JS, root_of(lit_id))), "the highlighted card's root came on screen")
    reader.notes.append(f"a card highlighted by a click, the mouse going on over another card to the dock, "
                        f"held {GLIDE_HOLD_MS}ms, left Main still")
    # THE CONTROL: the mouse resting on the highlighted card glides its root on screen, so the case is one that can.
    card = page.evaluate(CARD_AT_JS, lit_id)
    assert isinstance(card, dict)
    page.mouse.move(float(card["x"]), float(card["y"]), steps=4)
    reader.quiet(GLIDE_HOLD_MS)
    reader.check(not page.evaluate(ROW_AWAY_JS, root_of(lit_id)), "the mouse resting on the highlighted card did not "
                 "bring its root on screen, so the step above proves nothing")
    reader.notes.append("resting on the highlighted card brought its root on screen")
    # Put out again, the same card pressed.
    page.mouse.down()
    page.mouse.up()
    reader.check(page.evaluate("() => document.querySelectorAll('#side-cards > li[data-highlighted]').length") == 0,
                 "pressing the highlighted card again did not put the highlight out")


def follow_walk(reader: Reader) -> None:
    """A reader on the newest line follows a message arriving live, to the pixel."""
    page, api = reader.page, reader.api
    reader.scroll_main(1.0)
    reader.check(reader.gap("scroll-area") <= 1, "the reader is not on the newest line")
    arrived = api.add(f"A message arriving while the reader is on the newest line. {FILLER * 2}")
    api.push("message", {"message": arrived}, str(arrived["id"]))
    reader.wait_for(f"() => !!document.querySelector(\"#discord-log > li[data-id='{arrived['id']}']\")",
                    "the live message never reached Main")
    reader.quiet(700)
    gap = reader.gap("scroll-area")
    reader.check(gap <= 1, f"a reader on the newest line was left {gap:.0f}px above the message that arrived")
    row = page.evaluate(f"() => {{ const row = document.querySelector(\"#discord-log > li[data-id='{arrived['id']}']\");"
                        " const area = document.getElementById('scroll-area').getBoundingClientRect();"
                        " return row.getBoundingClientRect().bottom <= area.bottom + 1; }")
    reader.check(bool(row), "the message that arrived is below the screen")
    reader.notes.append("a reader on the newest line followed a live message to the foot")


def cards_walk(reader: Reader) -> None:
    """At the desk, the list of threads on the right opens at its foot and stays there."""
    page, api = reader.page, reader.api
    reader.wait_for("() => !document.getElementById('side-list').hidden", "the right column is not its cards")
    gap = reader.gap("side-scroll")
    reader.check(gap <= 1, f"the list of threads is {gap:.0f}px above its foot")
    last = page.locator("#side-cards > li").last
    box = last.bounding_box()
    assert box is not None
    page.mouse.move(box["x"] + 30, box["y"] + 12)
    page.mouse.move(box["x"] + 40, box["y"] + 16)
    reader.quiet(500)
    gap = reader.gap("side-scroll")
    reader.check(gap <= 1, f"the mouse over the last card left the list of threads {gap:.0f}px above its foot")
    reply = api.add(f"A reply moving its thread to the foot. {FILLER}", f"spaces/A/threads/t{SMALL_ROOTS[0]}")
    api.push("message", {"message": reply}, str(reply["id"]))
    reader.quiet(1200)
    gap = reader.gap("side-scroll")
    reader.check(gap <= 1, f"a reply moving a thread to the foot left the list of threads {gap:.0f}px above it")
    page.mouse.move(5, 5)
    reader.shot("3-cards")
    reader.notes.append("the list of threads opened at its foot and stayed there under the mouse and a moved thread")


def open_thread(reader: Reader, thread_id: str) -> tuple[str, str]:
    """Open `thread_id` where a reader at this size does, and say which list and scroller show it."""
    page = reader.page
    if reader.touch:
        page.select_option("#thread-select", f"thread:{thread_id}")
        reader.wait_for(f"() => document.getElementById('thread-select').value === 'thread:{thread_id}'"
                        " && document.querySelectorAll('#discord-log > li[data-id]').length > 0",
                        f"the picker did not open {thread_id}")
        return "discord-log", "scroll-area"
    if page.is_visible("#side-close"):
        page.keyboard.press("Escape")
        reader.wait_for("() => document.getElementById('side-pane').hidden", "Esc did not go back to the cards")
    # Its replies button: a click on the card itself only highlights it (`#277 thread-card-focus`).
    page.locator(f"#side-cards > li[data-id='{thread_id}'] .side-card-open").click()
    reader.wait_for("() => document.querySelectorAll('#side-log > li[data-id]').length > 0",
                    f"the card did not open {thread_id} on the right")
    return "side-log", "side-scroll"


def thread_walk(reader: Reader) -> None:
    """A thread opens on its newest message, with its list at its foot, and stays put as older replies come."""
    page = reader.page
    list_id, scroller = open_thread(reader, LONG)
    reader.frames(2)
    gap = reader.gap(scroller)
    reader.check(gap <= 1, f"the long thread opened {gap:.0f}px above its newest message")
    reader.quiet(1500)
    gap = reader.gap(scroller)
    reader.check(gap <= 1, f"a second and a half after it opened the long thread was {gap:.0f}px above its foot")
    rows = int(page.evaluate(f"() => document.querySelectorAll('#{list_id} > li[data-id]').length"))
    reader.check(rows < len(LONG_REPLIES) + 1, "the long thread came whole, so no older page is read")
    reader.shot("4-thread-newest")

    # At its head, the step back reads the page before, and the row the reader is on stays where it was.
    page.evaluate(f"() => {{ document.getElementById('{scroller}').scrollTop = 0; }}")
    reader.frames(2)
    before = reader.first(list_id)
    reader.wait_for(f"() => document.querySelectorAll('#{list_id} > li[data-id]').length > {rows}",
                    "scrolled to the head of the long thread, no older page came")
    reader.quiet()
    reader.still(before, "older replies coming in above", list_id)
    reader.notes.append("the long thread opened at its foot, held it, and kept its row as older replies came")

    list_id, scroller = open_thread(reader, FRESH)
    reader.quiet(800)
    gap = reader.gap(scroller)
    reader.check(gap <= 1, f"the thread whose newest reply alone is unread opened {gap:.0f}px above its foot")
    reader.shot("5-thread-fresh")
    reader.notes.append("so did the thread whose newest reply alone is unread")
    # Back to Main for the reload.
    if reader.touch:
        page.select_option("#thread-select", "main")
        reader.wait_for("() => document.getElementById('thread-select').value === 'main'", "Main did not come back")
    else:
        page.keyboard.press("Escape")
        reader.wait_for("() => document.getElementById('side-pane').hidden", "Esc did not go back to the cards")
    reader.quiet(500)


def reload_walk(reader: Reader, url: str) -> None:
    """The page reloaded with its read held: the saved rows up at once, read behind while the reader scrolls."""
    page, api = reader.page, reader.api
    api.timeline_gate.clear()
    reads = api.timeline_requests()
    page.goto(url, wait_until="load")
    reader.wait_for("() => document.getElementById('thread-select').value === 'main'"
                    " && document.querySelectorAll('#discord-log > li[data-id]').length > 30",
                    "the reload did not draw Main's saved rows at once")
    deadline = time.monotonic() + 10
    while api.timeline_requests() <= reads and time.monotonic() < deadline:
        page.wait_for_timeout(25)
    reader.check(api.timeline_requests() > reads, "the reload never read the channel")
    reader.frames(2)
    reader.scroll_main(0.35)
    before = reader.first()
    api.timeline_gate.set()
    reader.quiet(1200)
    reader.still(before, "the read behind the saved rows landing")
    reader.overflow()
    reader.notes.append("after a reload, the read behind the saved rows left the reader where they had scrolled")


if __name__ == "__main__":
    raise SystemExit(main())
