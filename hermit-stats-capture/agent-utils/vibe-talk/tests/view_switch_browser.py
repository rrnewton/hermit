#!/usr/bin/env python3
"""Switch a channel between All and Main in real Chromium, with every timeline request counted.

`#220 view-switch-instant`. The owner, 2026-10-08: "ALL view has loaded all the messages so switching
to MAIN channel only should basically just hide thread replies. But instead I saw it flash to a blank
screen and do the message fetching/loading animation. Let's fix that. I don't want to fetch already
cached data and I don't want to WAIT in the UI if I don't have to." The fake-DOM suite
(tests/js/voice_page.test.mjs) covers each branch of a switch; this covers what only a browser engine
can show: what is painted, frame by frame, while the page switches and while a read is on the wire.

It serves the checked-out page and the small fake API of tests/offline_cache_browser.py from one
loopback origin, at a phone's size, over a channel longer than the screen whose thread replies sit
between its own messages, and intercepts every timeline request the browser sends. Then:

  sign in and open the channel, in All, read once -> from the middle of the channel, Main, All, Main
  and All again, picked in the bar's real thread picker: no request at all, the list never empty and
  the loading line never shown -- watched on every change to either and on every frame -- and the
  message at the top of the screen where it was -> reload with the saved rows removed, so the page
  reopens in Main, the view last chosen, and reads only Main (the read of All it asks for behind
  Main, for its roots' reply counts, `#222 unread-replies`, is failed here) -> pick All, which has
  never been read:
  All is up at once from the rows the page holds, its one read held on the server meanwhile with the
  freshness pill saying it is refreshing, and merged when it lands, the reader's message still where
  it was, nothing read twice, and again no empty list and no loading line -> eight rows a read from
  here, paged by `before`, as every real channel is: reopened in All, Main is drawn from All's newest
  page with no read and keeps its way back, its first step back being Main's own newest page read
  behind the rows -> reopened in Main and walked back to its start, All, never read, is drawn from
  Main's rows at once and walked back to the message near the top of the screen before its read is
  drawn: All's newest page alone is never painted, the message stays where it was, and again no empty
  list and no loading line.

Pass --web-root DIR to run it against another copy of the page -- the page before `#220` fails at the
first switch -- and --screenshots DIR to keep a PNG of each stage.
"""

from __future__ import annotations

import argparse
import json
import os
import tempfile
import threading
import time
from http.server import ThreadingHTTPServer
from pathlib import Path
from typing import TYPE_CHECKING
from urllib.parse import parse_qs, urlsplit

import offline_cache_browser as fixture
from offline_cache_browser import CACHE_KEY, TOKEN, FakeApi, Json, check, handler_for, said, thread_of

if TYPE_CHECKING:
    from playwright.sync_api import BrowserType, Route


# A phone, as tests/offline_cache_browser.py's first profile: (label, width, height).
PHONE = ("phone", 412, 915)

# The message posted to the channel itself that the walk parks near the top of the screen.
ANCHOR = "212"


class SwitchApi(FakeApi):
    """A channel longer than a phone screen whose two threads' replies sit between the messages
    posted to the channel itself, so All and Main differ row for row."""

    def __init__(self) -> None:
        super().__init__()
        first: Json = {"id": "spaces/A/threads/first", "root_message_id": "201", "is_root": True,
                       "reply_count": 5, "reply_count_exact": True}
        second: Json = {"id": "spaces/A/threads/second", "root_message_id": "214", "is_root": True,
                        "reply_count": 4, "reply_count_exact": True}
        filler = "The overnight run is still going; the integration shard was retried and passed. "
        self.messages = []
        for index in range(30):
            thread: Json | None = None
            if index == 1:
                thread = first
            elif index == 14:
                thread = second
            elif index in (3, 5, 7, 9, 11):
                thread = {**first, "is_root": False}
            elif index in (16, 18, 20, 22):
                thread = {**second, "is_root": False}
            row = said(index, f"Message {200 + index}. {filler * (1 + index % 3)}", "human" if index % 2 else "coder")
            if thread is not None:
                row["thread"] = thread
            self.messages.append(row)
        self.threads = [{
            "id": record["id"], "root": self.messages[root], "title": f"Thread from {200 + root}",
            "reply_count": record["reply_count"], "reply_count_exact": True,
            "updated_at": self.messages[last]["timestamp"],
        } for record, root, last in ((first, 1, 11), (second, 14, 22))]

    def ids(self, view: str) -> list[str]:
        """The rows `view` shows, in order: every message in All, and in Main all but the replies."""
        return [str(m["id"]) for m in self.messages
                if view == "flat" or not thread_of(m) or thread_of(m)["is_root"]]

    # How many rows a read of Main or All answers with -- the newest that many, or the newest before
    # `before`, the id the previous page handed back, as a real channel's reads page -- or None for
    # every row at once.
    page_size: int | None = None

    def timeline(self, query: dict[str, list[str]]) -> Json:
        answer = super().timeline(query)
        rows = answer["messages"]
        if self.page_size is None or answer["view"] not in ("main", "flat") or not isinstance(rows, list):
            return answer
        before = query.get("before", [None])[0]
        end = next((i for i, row in enumerate(rows) if str(row["id"]) == before), len(rows)) if before else len(rows)
        start = max(0, end - self.page_size)
        return {**answer, "messages": rows[start:end], "has_more": start > 0, "returned": end - start,
                "next_before": str(rows[start]["id"]) if start > 0 else None}


# Watch the channel list from now on, in the page: on every change to it or to the loading line, and
# on every frame, count an empty list and a shown loading line. A list emptied and refilled inside
# one task is never painted, and never seen here; one left empty across a read is.
WATCH_JS = """() => {
    const log = document.getElementById('discord-log'), line = document.getElementById('channel-loading');
    const seen = window.__switchSeen = {emptied: 0, loading: 0, firsts: []};
    const look = () => {
        if (!log.querySelector('li[data-id]')) seen.emptied += 1;
        if (!line.hidden) seen.loading += 1;
        // The first row whenever it changes: a page drawn and then walked back is two of them.
        const first = log.querySelector('li[data-id]');
        const id = first ? first.getAttribute('data-id') : null;
        if (seen.firsts[seen.firsts.length - 1] !== id) seen.firsts.push(id);
    };
    new MutationObserver(look).observe(log, {childList: true});
    new MutationObserver(look).observe(line, {attributes: true, attributeFilter: ['hidden']});
    const frame = () => { look(); requestAnimationFrame(frame); };
    requestAnimationFrame(frame);
}"""

# Scroll the list so channel row `id` stands 20px below the top of its box: the row above it is then
# plainly on the screen too, so which message is at the top is never a sub-pixel question.
PARK_JS = """(id) => {
    const area = document.getElementById('scroll-area');
    const row = document.querySelector(`#discord-log > li[data-id="${id}"]`);
    area.scrollTop += row.getBoundingClientRect().top - area.getBoundingClientRect().top - 20;
}"""

# The channel rows on the screen, top to bottom, each with its top against the list's box.
VISIBLE_JS = """() => {
    const box = document.getElementById('scroll-area').getBoundingClientRect();
    return [...document.querySelectorAll('#discord-log > li[data-id]')]
        .map((row) => ({id: row.getAttribute('data-id'), r: row.getBoundingClientRect()}))
        .filter(({r}) => r.bottom > box.top && r.top < box.bottom)
        .map(({id, r}) => ({id, top: r.top - box.top}));
}"""


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
    # The fake API's handler serves the page from this module attribute.
    fixture.WEB_ROOT = args.web_root.resolve()
    label, width, height = PHONE
    with sync_playwright() as playwright:
        version = walk(playwright.chromium, args, label, width, height)
    print(f"{version} {label} at {width}x{height}: All to Main and back, twice, in the bar's real picker"
          " with every timeline request intercepted -- no read, the list never empty, no loading line,"
          " the message at the top of the screen kept; reopened in Main with no saved rows, All drawn"
          " from the held rows at once, read once behind them, and merged with the reader kept; paged,"
          " Main drawn from All's newest page keeps its way back through its own read, and All never"
          " read is walked back to the reader's message before it is drawn")
    return 0


def walk(chromium: BrowserType, args: argparse.Namespace, label: str, width: int, height: int) -> str:
    """One reader, start to finish, against a fresh fake API and a fresh browser profile."""
    api = SwitchApi()
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(api))
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-switch-") as profile:
            context = chromium.launch_persistent_context(
                profile, headless=True, executable_path=args.browser_executable,
                viewport={"width": width, "height": height}, device_scale_factor=2.625,
                is_mobile=True, has_touch=True,
            )
            version = context.browser.version if context.browser else "Chromium"
            page = context.pages[0]
            errors: list[str] = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            # Every read of a view of the channel, as the browser sent it. The thread list a touch on
            # the picker refreshes is not one: it reads summaries for the picker, not rows for the list.
            reads: list[str] = []
            # ...and each one's `before`, "" for a newest page.
            cursors: list[str] = []
            # `#222 unread-replies`: a read of Main whose roots report replies the page does not hold
            # reads All behind it, for the counts. While this is set, the next newest read of All is
            # failed, as a network would fail it, and kept here rather than in `reads`: the page then
            # has still never read All, which is what the walk's second stage is about.
            fail_counts_read = threading.Event()
            counts_reads: list[str] = []

            def counted(route: Route) -> None:
                query = parse_qs(urlsplit(route.request.url).query)
                view = query.get("view", [""])[0]
                if view == "flat" and "before" not in query and fail_counts_read.is_set():
                    fail_counts_read.clear()
                    counts_reads.append(view)
                    route.abort()
                    return
                if view != "threads":
                    reads.append(view)
                    cursors.append(query.get("before", [""])[0])
                route.continue_()

            page.route(lambda address: "/timeline" in address, counted)

            def shot(name: str) -> None:
                if args.screenshots:
                    page.screenshot(path=str(args.screenshots / f"{label}-switch-{name}.png"))

            def rows() -> list[str]:
                return [str(row) for row in page.eval_on_selector_all(
                    "#discord-log > li[data-id]", "items => items.map(i => i.getAttribute('data-id'))")]

            def wait_rows(expected: list[str], why: str, seconds: float = 10) -> None:
                deadline = time.monotonic() + seconds
                while time.monotonic() < deadline and rows() != expected:
                    page.wait_for_timeout(25)
                check(rows() == expected, f"{label}: {why}: showing {rows()}")

            def settled() -> None:
                page.evaluate("() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done)))")

            def seen() -> dict[str, int]:
                found = page.evaluate("() => window.__switchSeen")
                return {"emptied": int(found["emptied"]), "loading": int(found["loading"])}

            def visible() -> list[tuple[str, float]]:
                return [(str(row["id"]), float(row["top"])) for row in page.evaluate(VISIBLE_JS)]

            def kept(before: list[tuple[str, float]], why: str) -> None:
                """The first message on the screen before that this list shows is where it was."""
                now = dict(visible())
                shared = next(((id, top) for id, top in before if id in now), None)
                check(shared is not None, f"{label}: {why}: none of {before} is on the screen now: {now}")
                if shared is not None:
                    check(abs(now[shared[0]] - shared[1]) <= 1.5,
                          f"{label}: {why}: {shared[0]} moved from {shared[1]:.1f}px to {now[shared[0]]:.1f}px")

            def pick(value: str) -> None:
                page.select_option("#thread-select", value)

            def pill() -> str:
                return str(page.evaluate("() => document.getElementById('channel-freshness').textContent"))

            page.goto(f"http://127.0.0.1:{server.server_port}/voice", wait_until="load")
            page.fill("#api-token", TOKEN)
            page.click("#save-token")
            page.wait_for_selector("#search-toggle", state="visible", timeout=10_000)
            page.click("#view-switch")
            wait_rows(api.ids("flat"), "the channel did not open in All")
            settled()
            check(reads == ["flat"], f"{label}: opening the channel read {reads}")

            # 1. All, read: Main and back, twice, from the middle of the channel.
            page.evaluate(WATCH_JS)
            page.evaluate(PARK_JS, ANCHOR)
            settled()
            for view in ("main", "flat", "main", "flat"):
                before = visible()
                pick(view)
                wait_rows(api.ids(view), f"{view} was not drawn at once", seconds=2)
                settled()
                kept(before, f"switching to {view}")
            check(reads == ["flat"], f"{label}: switching between Main and All read {reads[1:]}")
            check(seen() == {"emptied": 0, "loading": 0}, f"{label}: switching between Main and All showed {seen()}")
            shot("1-local")

            # 2. Reopened in Main with no saved rows, so All has never been read -- the read of All
            # Main asks for behind it, for the counts its roots report, fails: picked, All is drawn
            # from what the page holds at once, read once behind those rows, and merged under the reader.
            pick("main")
            wait_rows(api.ids("main"), "Main was not drawn before the reload")
            page.evaluate(f"() => localStorage.removeItem({json.dumps(CACHE_KEY)})")
            reads.clear()
            fail_counts_read.set()
            page.reload(wait_until="load")
            wait_rows(api.ids("main"), "the reload did not reopen on Main")
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline and not counts_reads:
                page.wait_for_timeout(25)
            check(counts_reads == ["flat"], f"{label}: reopening on Main did not ask for All behind it, once")
            check(reads == ["main"], f"{label}: reopening on Main read {reads}")
            page.evaluate(WATCH_JS)
            page.evaluate(PARK_JS, ANCHOR)
            settled()
            before = visible()
            api.timeline_gate.clear()
            pick("flat")
            check(rows() == api.ids("main"), f"{label}: All did not draw the rows the page held at once: {rows()}")
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline and len(reads) < 2:
                page.wait_for_timeout(25)
            settled()
            check(reads == ["main", "flat"], f"{label}: All was not read once, behind its rows: {reads}")
            check("refreshing" in pill(), f"{label}: the read behind the rows was not said: {pill()!r}")
            kept(before, "drawing All from the held rows")
            shot("2-held-rows")
            api.timeline_gate.set()
            wait_rows(api.ids("flat"), "All's read was not merged in")
            settled()
            kept(before, "merging All's read")
            check(reads == ["main", "flat"], f"{label}: All was read more than once: {reads}")
            check(seen() == {"emptied": 0, "loading": 0}, f"{label}: All, read behind its rows, showed {seen()}")
            shot("3-merged")

            # 3. Eight rows a read from here, as a channel longer than one read answers. Reopened in
            # All: Main is drawn from All's newest page with no read, and keeps its way back -- the
            # first step is Main's own newest page, read behind the rows, the reader kept.
            api.page_size = 8
            pick("flat")
            page.evaluate(f"() => localStorage.removeItem({json.dumps(CACHE_KEY)})")
            reads.clear()
            page.reload(wait_until="load")
            newest_all = api.ids("flat")[-8:]
            wait_rows(newest_all, "the reload did not reopen on All's newest page")
            check(reads == ["flat"], f"{label}: reopening on All read {reads}")
            page.evaluate(WATCH_JS)
            api.timeline_gate.clear()
            pick("main")
            derived = [id for id in api.ids("main") if int(id) >= int(newest_all[0])]
            check(rows() == derived, f"{label}: Main was not drawn from All's newest page at once: {rows()}")
            check(page.is_visible("#load-older"), f"{label}: Main drawn from All lost the way to older messages")
            page.evaluate("() => { document.getElementById('scroll-area').scrollTop = 0; }")
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline and len(reads) < 2:
                page.wait_for_timeout(25)
            settled()
            check(reads == ["flat", "main"] and cursors[-1] == "",
                  f"{label}: Main's first step back was not its own newest page, once: {reads} {cursors}")
            before = visible()
            api.timeline_gate.set()
            wait_rows(api.ids("main")[-8:], "Main's own newest page was not merged in")
            settled()
            kept(before, "Main's first step back")
            check(page.is_visible("#load-older"), f"{label}: older history was not offered after Main's read")
            check(seen()["emptied"] == 0 and seen()["loading"] == 0, f"{label}: Main's step back showed {seen()}")
            shot("4-paged-main")

            # Reopened in Main and walked back to its start, a message near the top of the screen: All,
            # never read, is drawn from Main's rows at once and walked back to that message before its
            # read is drawn -- the list changes once, under the reader.
            page.evaluate(f"() => localStorage.removeItem({json.dumps(CACHE_KEY)})")
            reads.clear()
            cursors.clear()
            page.reload(wait_until="load")
            wait_rows(api.ids("main")[-8:], "the reload did not reopen on Main's newest page")
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline and rows() != api.ids("main"):
                if page.is_visible("#load-older") and page.is_enabled("#load-older"):
                    page.click("#load-older")
                page.wait_for_timeout(50)
            check(rows() == api.ids("main"), f"{label}: Main was not walked back to its start: {rows()}")
            check(reads == ["main"] * 3, f"{label}: walking Main back read {reads}")
            page.evaluate(WATCH_JS)
            page.evaluate(PARK_JS, "204")
            settled()
            before = visible()
            pick("flat")
            check(rows() == api.ids("main"), f"{label}: All was not drawn from the rows the page held at once: {rows()}")
            wait_rows(api.ids("flat"), "All was not walked back to the reader's message")
            settled()
            kept(before, "walking All back to the reader's message")
            check(reads[3:] == ["flat"] * 4 and cursors[3:] == ["", "222", "214", "206"],
                  f"{label}: All was not read back to the reader's message, and no further: {reads[3:]} {cursors[3:]}")
            firsts = page.evaluate("() => window.__switchSeen.firsts")
            check(newest_all[0] not in firsts, f"{label}: All's newest page was painted before the walk: {firsts}")
            check(seen()["emptied"] == 0 and seen()["loading"] == 0, f"{label}: All, walked back, showed {seen()}")
            shot("5-walked-back")
            check(not errors, f"{label}: the page threw: {errors}")
            context.close()
            return version
    finally:
        api.timeline_gate.set()
        api.stopping.set()
        server.shutdown()
        server.server_close()
        thread.join()


if __name__ == "__main__":
    raise SystemExit(main())
