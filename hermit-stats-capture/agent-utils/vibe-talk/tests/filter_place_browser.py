#!/usr/bin/env python3
"""Clear the Pinned filter over a chosen pin in real Chromium, at a 1280x800 desk and a 412x915 phone.

`#243 filter-keeps-selection`. The owner, 2026-10-12: "jump to my last pinned message: go to search,
filter for pinned, select the message. But then as soon as I clear the filter, the screen jumps and I
don't have the message I just found. The rule here is that when you adjust the filter, the highlighted
message MUST stay on the screen. Ideally it stays on the screen in the same Y location without a
"jump."" The fake-DOM suite (tests/js/voice_page.test.mjs) measures every kind of filter change in its
layout model; this does the owner's own sequence in a browser engine, which lays out the search bar's
room at the head of the list, the Pinned list and the channel's list for real.

It serves the checked-out page and the small fake API of tests/offline_cache_browser.py from one
loopback origin, over a channel of sixty messages read twenty at a time, with five pins, one of them on
a message two pages older than the channel's newest page. At each size — with a mouse on the desk and
a finger on the phone — and measuring each chosen row's top on the screen before and after:

  sign in and open the channel -> the search glass, then Pinned: the five pins, the old one drawn from
  what its pin kept -> choose a loaded pin from the middle of the list -> clear the filter (Pinned
  tapped again): the pin's row in the channel's list stands where it stood in the Pinned list, within
  a pixel, still there half a second later, and on the desk lit as the selection -> Pinned again, and
  the OLD pin chosen -> the filter cleared (Pinned on the phone, Escape on the desk): with the reads
  held, the head of the list says "Loading earlier messages…" on the screen; let go, the channel is
  walked back two pages and the old pin's row stands where it stood, within a pixel, and stays there
  -> Pinned again, and a pin that is the SECOND part of a row the channel combined (255, sent two
  seconds after 254 by the same account) chosen -> Pinned off: the pin's own text starts where it
  started in the Pinned list, within a pixel, the part above it above that -> the reader opens the
  long message just above that row, on screen whole, by a click or a tap on it: it stays where the
  reader saw it, within a pixel, and opens downwards (from review: what the filter change held was
  still every redraw's anchor, and pushed the message the reader opened up off the screen).

Pass --web-root DIR to run it against another copy of the page -- the page before `#243` fails at the
first clear -- and --screenshots DIR to keep a PNG of each stage.
"""

from __future__ import annotations

import argparse
import os
import tempfile
import threading
import time
from datetime import timedelta
from http.server import ThreadingHTTPServer
from pathlib import Path
from typing import TYPE_CHECKING

import offline_cache_browser as fixture
from offline_cache_browser import EPOCH, TOKEN, FakeApi, Json, check, handler_for, said

if TYPE_CHECKING:
    from playwright.sync_api import BrowserType, Page


# The two sizes the issue names: (label, width, height, touch). The desk is driven by a mouse and the
# keyboard, the phone by a finger.
SIZES = (("desk", 1280, 800, False), ("phone", 412, 915, True))

# The messages pinned, by index: 12 is two pages older than the newest page (40..59), the rest loaded.
PINNED = (12, 44, 47, 52, 55, 57)
# The loaded pin chosen first, and the old one chosen second, by id.
LOADED_PIN = "247"
OLD_PIN = "212"
# The message sent two seconds after the one before it, by the same account, so that the channel draws
# the two as one row: pinned, it is the second part of that row. By index, and its pin by id.
COMBINED = 55
COMBINED_PIN = "255"
# The long message just above that row, folded until the reader opens it: by index, and by id.
FOLDING = 53
FOLDING_ID = "253"
# How many rows one read of the channel answers with.
PAGE_SIZE = 20
# How far a row may stand from where it stood, in CSS px: the issue's "within 1px".
TOLERANCE_PX = 1.0

FILLER = "The overnight run is still going; the integration shard was retried and passed. "


class PlaceApi(FakeApi):
    """Sixty short messages, read twenty at a time by `before` as every real channel is read, with
    five pins kept by this server: one on a message the channel's newest page does not reach."""

    def __init__(self) -> None:
        super().__init__()
        # Under 280 characters each, so no row folds and a tap on one only selects it -- but one.
        self.messages = [said(i, f"Message {200 + i}. {FILLER * (1 + i % 3)}", "human" if i % 2 else "coder")
                         for i in range(60)]
        # The one: long enough to fold, and opened by the reader in the last stage.
        self.messages[FOLDING] = said(FOLDING, f"Message {200 + FOLDING}. {FILLER * 8}", "human")
        # By the account of the message before it, two seconds after it, and short enough that the two
        # together still do not fold: one row, its second part whole on the screen.
        before = "human" if (COMBINED - 1) % 2 else "coder"
        self.messages[COMBINED] = {
            **said(COMBINED, f"Message {200 + COMBINED}. {FILLER}", before),
            "timestamp": (EPOCH + timedelta(minutes=COMBINED - 1, seconds=2)).isoformat().replace("+00:00", "Z"),
        }
        self.threads = []
        self.serve_pins_revision = True
        for index in PINNED:
            row = self.messages[index]
            self.pins[str(row["id"])] = {
                "message_id": str(row["id"]), "author": row["author"], "author_id": row["author_id"],
                "author_is_bot": row["author_is_bot"], "content": str(row["content"]), "truncated": False,
                "timestamp": str(row["timestamp"]), "thread_id": None, "thread_root": False,
                "pinned_at_ms": 1_790_000_000_000 + index,
            }
        self.pins_revision = 1

    def timeline(self, query: dict[str, list[str]]) -> Json:
        answer = super().timeline(query)
        rows = answer["messages"]
        if answer["view"] not in ("main", "flat") or not isinstance(rows, list):
            return answer
        before = query.get("before", [None])[0]
        end = next((i for i, row in enumerate(rows) if str(row["id"]) == before), len(rows)) if before else len(rows)
        start = max(0, end - PAGE_SIZE)
        return {**answer, "messages": rows[start:end], "has_more": start > 0, "returned": end - start,
                "next_before": str(rows[start]["id"]) if start > 0 else None}


# The top of the row standing for message `id` in list `list` on the screen, or null when none does.
ROW_TOP_JS = """([list, id]) => {
    const row = [...document.querySelectorAll(`#${list} > li[data-ids]`)]
        .find((li) => li.getAttribute('data-ids').split(' ').includes(id));
    return row && row.getClientRects().length ? row.getBoundingClientRect().top : null;
}"""

# The top of the block message `id` is drawn in, inside the row of `list` holding it: the row's own text
# for a row of one message, and one block of several for a combined row. Null when none is on the screen.
BLOCK_TOP_JS = """([list, id]) => {
    const row = [...document.querySelectorAll(`#${list} > li[data-ids]`)]
        .find((li) => li.getAttribute('data-ids').split(' ').includes(id));
    if (!row || !row.getClientRects().length) return null;
    const body = row.querySelector(':scope > .body');
    const at = row.getAttribute('data-ids').split(' ').indexOf(id);
    const block = body && body.children.length === row.getAttribute('data-ids').split(' ').length
        ? body.children[at] : null;
    return block ? block.getBoundingClientRect().top : null;
}"""

# The row of `list` holding `id`: its top and bottom on the screen, the top of the list's box, its ids.
ROW_BOX_JS = """([list, id]) => {
    const row = [...document.querySelectorAll(`#${list} > li[data-ids]`)]
        .find((li) => li.getAttribute('data-ids').split(' ').includes(id));
    const box = row.getBoundingClientRect();
    const area = document.getElementById('scroll-area').getBoundingClientRect().top;
    return {top: box.top, bottom: box.bottom, area, ids: row.getAttribute('data-ids')};
}"""

# Scroll the list so the row of `list` holding `id` stands at `fraction` of the way down the list's box.
PARK_JS = """([list, id, fraction]) => {
    const area = document.getElementById('scroll-area');
    const row = [...document.querySelectorAll(`#${list} > li[data-ids]`)]
        .find((li) => li.getAttribute('data-ids').split(' ').includes(id));
    const box = area.getBoundingClientRect();
    area.scrollTop += row.getBoundingClientRect().top - (box.top + box.height * fraction);
}"""

# Where to press the row of `list` holding `id`: a point inside its box, left of its controls.
PRESS_AT_JS = """([list, id]) => {
    const row = [...document.querySelectorAll(`#${list} > li[data-ids]`)]
        .find((li) => li.getAttribute('data-ids').split(' ').includes(id));
    const box = row.getBoundingClientRect();
    return [box.left + Math.min(40, box.width / 4), box.top + box.height / 2];
}"""

# The step-back line at the head of the channel's list: its words, and whether it is on the screen.
OLDER_LINE_JS = """() => {
    const line = document.getElementById('load-older');
    const box = line.getBoundingClientRect(), area = document.getElementById('scroll-area').getBoundingClientRect();
    return {text: line.textContent.trim(), shown: line.getClientRects().length > 0 &&
        box.bottom > area.top && box.top < area.bottom};
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
    said_lines: list[str] = []
    with sync_playwright() as playwright:
        for label, width, height, touch in SIZES:
            version, moves = walk(playwright.chromium, args, label, width, height, touch)
            said_lines.append(f"{label} {width}x{height}: {moves}")
    print(f"{version}: search, Pinned, a pin chosen, the filter cleared -- the pin's row where it stood, "
          f"an old pin walked back to and put where it stood, a combined row's second part put back by its "
          f"own text, and a message the reader opened afterwards left where they saw it; " + "; ".join(said_lines))
    return 0


def walk(chromium: BrowserType, args: argparse.Namespace, label: str, width: int, height: int,
         touch: bool) -> tuple[str, str]:
    """The owner's sequence at one size, against a fresh fake API and a fresh browser profile."""
    api = PlaceApi()
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(api))
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-filter-place-") as profile:
            context = chromium.launch_persistent_context(
                profile, headless=True, executable_path=args.browser_executable,
                viewport={"width": width, "height": height},
                device_scale_factor=2.625 if touch else 1, is_mobile=touch, has_touch=touch,
            )
            version = context.browser.version if context.browser else "Chromium"
            page = context.pages[0]
            errors: list[str] = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            moves = sequence(page, api, server.server_port, args, label, touch)
            check(not errors, f"{label}: the page threw: {errors}")
            context.close()
            return version, moves
    finally:
        api.timeline_gate.set()
        api.stopping.set()
        server.shutdown()
        server.server_close()
        thread.join()


def sequence(page: Page, api: PlaceApi, port: int, args: argparse.Namespace, label: str, touch: bool) -> str:
    """Search, Pinned, a pin chosen, the filter cleared; then the same for the old pin."""

    def shot(name: str) -> None:
        if args.screenshots:
            page.screenshot(path=str(args.screenshots / f"{label}-filter-place-{name}.png"))

    def until(script: str, why: str, seconds: float = 10, arg: object = None) -> None:
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline and not page.evaluate(script, arg):
            page.wait_for_timeout(25)
        check(bool(page.evaluate(script, arg)), f"{label}: {why}")

    def settled() -> None:
        # Two frames, so what the page did has been laid out and its observers have answered.
        page.evaluate("() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done)))")

    def press(selector: str) -> None:
        box = page.locator(selector).bounding_box()
        check(box is not None, f"{label}: {selector} is not on the screen")
        if box is None:
            return
        x, y = box["x"] + box["width"] / 2, box["y"] + box["height"] / 2
        if touch:
            page.touchscreen.tap(x, y)
        else:
            page.mouse.click(x, y)

    def top(list_id: str, message: str) -> float | None:
        found = page.evaluate(ROW_TOP_JS, [list_id, message])
        return None if found is None else float(found)

    def press_row(list_id: str, message: str) -> None:
        """A click or a tap on the row of `list_id` holding `message`, left of its controls."""
        x, y = page.evaluate(PRESS_AT_JS, [list_id, message])
        if touch:
            page.touchscreen.tap(float(x), float(y))
        else:
            page.mouse.move(float(x), float(y))
            page.mouse.click(float(x), float(y))
        settled()

    def choose(message: str, fraction: float = 0.45) -> float:
        """Bring the pin to `fraction` of the way down the Pinned list, choose it, and say where its row stands."""
        page.evaluate(PARK_JS, ["pinned-log", message, fraction])
        settled()
        press_row("pinned-log", message)
        at = top("pinned-log", message)
        check(at is not None, f"{label}: pin {message} is not in the Pinned list")
        return float(at or 0)

    def back_with(message: str, why: str) -> None:
        """The Pinned list has gone, and the channel's list is back on the screen with `message` in it."""
        until("([list, id]) => { const row = [...document.querySelectorAll(`#${list} > li[data-ids]`)]"
              ".find((li) => li.getAttribute('data-ids').split(' ').includes(id));"
              " return Boolean(row && row.getClientRects().length) &&"
              " document.getElementById('pinned-log').hidden; }",
              f"{why}: the channel's list did not come back with {message} in it", arg=["discord-log", message])
        settled()

    def block_top(list_id: str, message: str) -> float:
        """The top of the block `message`'s own text is drawn in, in the row of `list_id` holding it."""
        found = page.evaluate(BLOCK_TOP_JS, [list_id, message])
        check(found is not None, f"{label}: #{list_id} draws no text for {message}")
        return float(found or 0)

    def row_box(list_id: str, message: str) -> tuple[float, float, float, str]:
        """The top and bottom of the row of `list_id` holding `message` on the screen, the list's top, its ids."""
        box = page.evaluate(ROW_BOX_JS, [list_id, message])
        return float(box["top"]), float(box["bottom"]), float(box["area"]), str(box["ids"])

    def kept(message: str, before: float, why: str) -> float:
        """The pin's row in the channel's list stands where it stood, and stays there."""
        back_with(message, why)
        after = top("discord-log", message)
        check(after is not None and abs(after - before) <= TOLERANCE_PX,
              f"{label}: {why}: {message} stood at {before:.1f}px and stands at {after}px")
        # Nothing else moves it afterwards: no observer, no refresh, no landing.
        page.wait_for_timeout(500)
        settled()
        later = top("discord-log", message)
        check(later is not None and after is not None and abs(later - after) <= TOLERANCE_PX,
              f"{label}: {why}: {message} moved from {after}px to {later}px after it was placed")
        return float(after or 0)

    def pinned_on() -> None:
        press("#pinned-filter")
        until("() => document.getElementById('pinned-filter').getAttribute('aria-pressed') === 'true'",
              "a tap on Pinned did not turn it on")
        until(f"() => document.querySelectorAll('#pinned-log > li[data-ids]').length === {len(PINNED)}",
              "the Pinned list does not show the five pins")
        settled()

    page.goto(f"http://127.0.0.1:{port}/voice", wait_until="load")
    page.fill("#api-token", TOKEN)
    page.click("#save-token")
    page.wait_for_selector("#search-toggle", state="visible", timeout=10_000)
    page.click("#view-switch")
    # Messages, not rows: two of them share one row.
    until("() => [...document.querySelectorAll('#discord-log > li[data-ids]')]"
          f".reduce((count, li) => count + li.getAttribute('data-ids').split(' ').length, 0) === {PAGE_SIZE}",
          "the channel did not open on its newest page")
    settled()

    # 1. Search, Pinned, a loaded pin chosen, Pinned tapped off.
    press("#search-toggle")
    until("() => !document.getElementById('pinned-filter').hidden", "the open search bar has no Pinned filter")
    pinned_on()
    shot("1-pinned")
    before = choose(LOADED_PIN)
    if not touch:
        check(page.evaluate("(id) => document.querySelector(`#pinned-log > li[data-id='${id}']`)"
                            ".getAttribute('data-selected')", LOADED_PIN) == "true",
              f"{label}: the click did not select pin {LOADED_PIN}")
    press("#pinned-filter")
    after = kept(LOADED_PIN, before, "Pinned cleared over a loaded pin")
    if not touch:
        check(page.evaluate("(id) => document.querySelector(`#discord-log > li[data-id='${id}']`)"
                            ".getAttribute('data-selected')", LOADED_PIN) == "true",
              f"{label}: pin {LOADED_PIN} is not the selection in the channel's list")
    shot("2-cleared")
    moves = f"{LOADED_PIN} at {before:.1f}px then {after:.1f}px"

    # 2. Pinned again, the old pin chosen, and the filter cleared with the reads held.
    pinned_on()
    old_before = choose(OLD_PIN)
    api.timeline_gate.clear()
    if touch:
        press("#pinned-filter")
    else:
        page.keyboard.press("Escape")
    until("() => document.getElementById('pinned-log').hidden", "clearing the filter left the Pinned list up")
    until("() => document.getElementById('load-older').textContent.trim() === 'Loading earlier messages…'",
          "the head of the list does not say it is loading earlier messages")
    settled()
    line = page.evaluate(OLDER_LINE_JS)
    check(bool(line["shown"]), f"{label}: the line saying earlier messages are loading is not on the screen")
    shot("3-walking")
    api.timeline_gate.set()
    old_after = kept(OLD_PIN, old_before, "the filter cleared over a pin two pages back")
    until("() => document.getElementById('load-older').textContent.trim() !== 'Loading earlier messages…'",
          "the line still says earlier messages are loading")
    shot("4-walked")
    moves += f"; {OLD_PIN} at {old_before:.1f}px then {old_after:.1f}px"

    # 3. Pinned again, the second part of a combined row chosen, and Pinned off: its own text stands
    # where it stood, and the part above it goes above that, not the pin below. Escape closed the search
    # on the desk: the glass opens it again.
    if page.evaluate("() => document.getElementById('pinned-filter').hidden"):
        press("#search-toggle")
        until("() => !document.getElementById('pinned-filter').hidden", "the glass did not open the search again")
    pinned_on()
    choose(COMBINED_PIN, 0.6)
    text_before = block_top("pinned-log", COMBINED_PIN)
    press("#pinned-filter")
    back_with(COMBINED_PIN, "Pinned off over the second part of a combined row")
    combined_top, _, _, ids = row_box("discord-log", COMBINED_PIN)
    check(ids == f"{200 + COMBINED - 1} {COMBINED_PIN}",
          f"{label}: {COMBINED_PIN} is not the second part of a combined row: {ids}")
    text_after = block_top("discord-log", COMBINED_PIN)
    check(abs(text_after - text_before) <= TOLERANCE_PX,
          f"{label}: the text of pin {COMBINED_PIN} started at {text_before:.1f}px and starts at {text_after:.1f}px")
    check(combined_top < text_before - TOLERANCE_PX,
          f"{label}: the part above pin {COMBINED_PIN} is not above where the pin stood")
    page.wait_for_timeout(500)
    settled()
    text_later = block_top("discord-log", COMBINED_PIN)
    check(abs(text_later - text_after) <= TOLERANCE_PX,
          f"{label}: pin {COMBINED_PIN}'s text moved from {text_after:.1f}px to {text_later:.1f}px after it was placed")
    shot("5-combined")
    moves += f"; {COMBINED_PIN}'s text at {text_before:.1f}px then {text_after:.1f}px"

    # 4. The reader opens the long message just above that row, on the screen whole: it opens downwards
    # and stays where the reader saw it.
    top_before, bottom_before, area_top, _ = row_box("discord-log", FOLDING_ID)
    check(top_before > area_top and bottom_before <= combined_top + TOLERANCE_PX,
          f"{label}: {FOLDING_ID} is not on the screen whole above the combined row: "
          f"{top_before:.1f}px to {bottom_before:.1f}px, the row below at {combined_top:.1f}px")
    press_row("discord-log", FOLDING_ID)
    top_after, bottom_after, _, _ = row_box("discord-log", FOLDING_ID)
    check(bottom_after - top_after > bottom_before - top_before + 20,
          f"{label}: a {'tap' if touch else 'click'} on {FOLDING_ID} did not open it")
    check(abs(top_after - top_before) <= TOLERANCE_PX,
          f"{label}: {FOLDING_ID}, opened by the reader, moved from {top_before:.1f}px to {top_after:.1f}px")
    shot("6-opened")
    return moves + f"; {FOLDING_ID} opened at {top_before:.1f}px then {top_after:.1f}px"

if __name__ == "__main__":
    raise SystemExit(main())
