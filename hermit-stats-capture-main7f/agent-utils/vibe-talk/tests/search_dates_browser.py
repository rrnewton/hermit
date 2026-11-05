#!/usr/bin/env python3
"""Search a week of a channel in real Chromium, at a 1280x800 desk and a 412x915 phone.

`#249 search-highlight`, `#250 date-separators` and `#251 author-filter`. The owner, 2026-10-12: a
search should "highlight where the word hits within the filtered messages that have the word", show the
messages it leaves out as gaps that "say "N unmatched messages"", the days should be ruled off — "----
Oct 3, Mon ----" — and collapse into one range when a filter skips them, "Oct 3 - 5", and the search bar
should filter to "prompts vs agent responses (me vs them)", which with no term typed is "a full view of
all my prompts". The fake-DOM suite (tests/js/voice_page.test.mjs) pins every rule of that in its
layout model; this lays the lines out for real, in the margins the stylesheet gives them.

It serves the checked-out page and the small fake API of tests/bookmark_browser.py from one loopback
origin, over a channel of thirty messages across eight days in New York's zone, read on Monday 12
October 2026 at noon there (the browser's clock is fixed), with the owner's prompts and the agent's
answers, the bookmark on the first message of one day, one long answer with the word past its fold, and
one link to it by its address alone. At each size — a mouse on the desk, a finger on the phone:

  sign in and open the channel -> a rule above the first message of each day, eight of them, in the
  owner's words ("Oct 9, Fri", "Yesterday, Oct 11, Sun", "Today, Oct 12, Mon"), each between the row
  above and its own, touching neither; the bookmark's day: its rule above the bookmark's line, the line
  above the message -> the search glass, "engine": four messages, each hit lit where it is once the
  typing pauses, the link found by its address outlined, the long answer's hit said under its fold; a
  line in each gap, between the rows and touching neither, the days it reaches on it ("· Oct 7 – 8"),
  on one line or two broken between its count and its days and nowhere else, inside its room; and one
  under the last match -> the text cleared, the author filter a whole target; on the narrow phone,
  where it hangs under the bar, the list starting below it, so that scrolled no row passes under it
  -> an owner's prompt mid-screen chosen, the author filter pressed: only the owner's prompts, "N of
  30 loaded", the prompt where it stood within a pixel; pressed twice more, back to everyone's, the
  prompt where it stood again.

Pass --web-root DIR to run it against another copy of the page, and --screenshots DIR to keep a PNG of
each stage.
"""

from __future__ import annotations

import argparse
import os
import tempfile
import time
from datetime import datetime, timedelta
from pathlib import Path
from typing import TYPE_CHECKING
from zoneinfo import ZoneInfo

import bookmark_browser as fixture
from bookmark_browser import AGENT, CHANNEL, TOKEN, FakeApi, Json, check, serve

if TYPE_CHECKING:
    from playwright.sync_api import BrowserContext, Page, Playwright

# The two sizes the issues name, and the narrow phone where the author filter hangs under the bar:
# (label, width, height, touch).
SIZES = (("desk", 1280, 800, False), ("phone", 412, 915, True), ("phone-360", 360, 800, True))
# Up to this width the author filter hangs under the bar's left end rather than standing on its line.
HANGS_UNDER_PX = 400
# The reader's zone, and the moment the browser's clock is fixed at: Monday 12 October 2026, noon there.
ZONE = "America/New_York"
NOW = datetime(2026, 10, 12, 12, 0, tzinfo=ZoneInfo(ZONE))
# The owner's account, as client-config names it, so the page draws these as his.
OWNER = {"author": "owner", "author_id": "1000000000000000007", "author_is_bot": False}
# How far a row may stand from where it stood, in CSS px: the issue's "within 1px".
TOLERANCE_PX = 1.0

FILLER = ("The integration shard was retried twice and passed; the artifact upload waited on the cache "
          "to warm and then went through. ")

# The channel: (days before today, hour, minute, who, words). "engine" is in four of them.
SCRIPT: tuple[tuple[int, int, int, str, str], ...] = (
    (7, 9, 0, "owner", "Morning. What is left on the release lane?"),
    (7, 9, 4, "agent", "Two shards and the packaging job. " + FILLER),
    (7, 16, 30, "owner", "Ship the packaging job first."),
    (6, 8, 15, "agent", "The engine rebuild finished overnight; nothing else moved."),
    (6, 10, 0, "owner", "Good. Hold the shards until the cache is warm."),
    (6, 14, 45, "agent", "Holding. " + FILLER),
    (5, 9, 30, "owner", "Any change?"),
    (5, 9, 32, "agent", "None yet. " + FILLER),
    (4, 11, 0, "owner", "Restart the engine shard and tell me when it is green."),
    (4, 11, 6, "agent", "Restarted; it is queued behind two other jobs."),
    (4, 15, 20, "agent", "Green now. " + FILLER),
    (3, 10, 10, "owner", "Start the canary."),
    (3, 10, 12, "agent", "Canary started at five per cent."),
    (3, 18, 40, "agent", "Canary at twenty per cent, no errors. " + FILLER),
    (2, 9, 5, "owner", "Hold the canary at twenty over the weekend."),
    (2, 9, 7, "agent", "Held at twenty."),
    (1, 8, 0, "agent", FILLER * 3 + "One thing to watch: the engine logs show a slow start on the canary hosts."),
    (1, 8, 20, "owner", "How slow?"),
    (1, 8, 22, "agent", "Under a second, inside the budget."),
    (1, 21, 5, "owner", "Fine. Resume tomorrow."),
    (0, 8, 0, "agent", "Resumed the canary; forty per cent."),
    (0, 8, 30, "owner", "Post the runbook link."),
    (0, 8, 31, "agent", "RUNBOOK"),
    (0, 9, 15, "owner", "Thanks. What else is open?"),
    (0, 9, 16, "agent", "The docs change and one flaky test. " + FILLER),
    (0, 10, 0, "owner", "Take the flaky test."),
    (0, 10, 5, "agent", "Taken; it fails one run in twenty. " + FILLER),
    (0, 11, 0, "owner", "Ping me when it is fixed."),
    (0, 11, 2, "agent", "Will do."),
    (0, 11, 30, "owner", "Lunch."),
)
# The link found by its address alone: drawn by the server with a name that does not hold the word.
RUNBOOK = "https://docs.example.com/engine-runbook"
RUNBOOK_HTML = (f'<p>Here: <a href="{RUNBOOK}" target="_blank" rel="noopener noreferrer nofollow">the release '
                "runbook</a></p>\n")
# The message the bookmark is on: the first of four days ago.
BOOKMARKED = 8
# The owner's prompt chosen before the author filter, by index.
CHOSEN = 14


def message_id(index: int) -> str:
    return f"77000000000000000{index:02d}"


class WeekApi(FakeApi):
    """The fake API of tests/bookmark_browser.py over the week above, with the owner configured."""

    def __post_init__(self) -> None:
        for index, (ago, hour, minute, who, words) in enumerate(SCRIPT):
            sent = (NOW - timedelta(days=ago)).replace(hour=hour, minute=minute)
            row: Json = {
                "id": message_id(index), "channel_id": CHANNEL["id"],
                "timestamp": sent.astimezone(ZoneInfo("UTC")).isoformat().replace("+00:00", "Z"),
                "spoken_time": "", "reply_to": None, **(OWNER if who == "owner" else AGENT),
                "content": f"Here: [the release runbook]({RUNBOOK})" if words == "RUNBOOK" else words,
            }
            if words == "RUNBOOK":
                row["content_html"] = RUNBOOK_HTML
            self.messages.append(row)
        marked = self.messages[BOOKMARKED]
        self.bookmark = {"revision": 1, "anchor": {"message_id": marked["id"], "place": "above",
                                                   "timestamp": marked["timestamp"]}}

    def client_config(self) -> Json:
        return {**super().client_config(), "owner_author_id": OWNER["author_id"]}


# Each shown row of the channel's list, top to bottom: its id, its box, its lines and theirs, the hits
# marked in it and the line its fold carries.
ROWS_JS = """() => {
  const box = (node) => {
    if (!node || !node.getClientRects().length) return null;
    const r = node.getBoundingClientRect();
    return {top: r.top, bottom: r.bottom, left: r.left, right: r.right};
  };
  const list = box(document.getElementById('discord-log'));
  // A line's words as the reader reads them, the no-break spaces and word joiners holding each side of
  // its "·" together read as nothing more; and how they are laid out: the distinct lines its text is
  // on, and whether the count and the days are each on one line.
  const words = (seam) => seam.textContent.replace(/\u2060/g, '').replace(/\u00a0/g, ' ');
  const layout = (seam) => {
    const text = seam.firstChild;
    if (!text || text.nodeType !== 3) return null;
    const lines = (from, to) => {
      const range = document.createRange();
      range.setStart(text, from);
      range.setEnd(text, to);
      const rects = [...range.getClientRects()].filter((r) => r.width > 0);
      return {count: new Set(rects.map((r) => Math.round(r.top))).size,
        top: Math.min(...rects.map((r) => r.top)), bottom: Math.max(...rects.map((r) => r.bottom))};
    };
    const all = lines(0, text.length);
    const dot = text.data.indexOf('·');
    return {lines: all.count, top: all.top, bottom: all.bottom,
      count: dot < 0 ? null : lines(0, dot + 1).count, days: dot < 0 ? null : lines(dot + 2, text.length).count};
  };
  return [...document.querySelectorAll('#discord-log > li[data-id]')]
    .filter((li) => li.getClientRects().length)
    .map((li) => {
      const line = (below) => [...li.children].find((kid) => kid.classList.contains('row-seam') &&
        kid.classList.contains('row-seam-below') === below) || null;
      const above = line(false), under = line(true);
      const excerpt = li.querySelector(':scope > .search-excerpt');
      return {
        id: li.dataset.id, who: li.dataset.who, box: box(li), seam: li.getAttribute('data-seam'), list,
        above: above ? {text: words(above), box: box(above), layout: layout(above),
          style: getComputedStyle(above, '::before').borderTopStyle} : null,
        below: under ? {text: words(under), box: box(under), layout: layout(under)} : null,
        bookmark: box(li.querySelector(':scope > .bookmark-line')),
        marks: [...li.querySelectorAll('mark.search-hit')].filter((m) => !excerpt || !excerpt.contains(m))
          .map((m) => ({text: m.textContent, box: box(m), background: getComputedStyle(m).backgroundColor})),
        outlined: [...li.querySelectorAll('a.search-hit-link')].map((a) => getComputedStyle(a).outlineStyle),
        excerpt: excerpt && excerpt.getClientRects().length ? {text: excerpt.textContent,
          marks: excerpt.querySelectorAll('mark.search-hit').length} : null,
      };
    });
}"""

# Scroll the list so the row holding `id` stands at `fraction` of the way down the list's box.
PARK_JS = """([id, fraction]) => {
    const area = document.getElementById('scroll-area');
    const row = document.querySelector(`#discord-log > li[data-id='${id}']`);
    const box = area.getBoundingClientRect();
    area.scrollTop += row.getBoundingClientRect().top - (box.top + box.height * fraction);
}"""

# Where to press the row holding `id`, left of its controls; and its top.
PRESS_AT_JS = """(id) => {
    const r = document.querySelector(`#discord-log > li[data-id='${id}']`).getBoundingClientRect();
    return [r.left + Math.min(40, r.width / 4), r.top + Math.min(18, r.height / 2)];
}"""
# Scroll the list half way down, and measure what of each row is drawn — its box within the list's —
# against the author filter's: on a narrow phone the filter hangs under the bar, over the list.
SCROLLED_JS = """() => {
    const area = document.getElementById('scroll-area');
    area.scrollTop = Math.round((area.scrollHeight - area.clientHeight) / 2);
    const clip = area.getBoundingClientRect();
    const at = document.getElementById('author-filter').getBoundingClientRect();
    const drawn = [...document.querySelectorAll('#discord-log > li[data-id]')].map((li) => {
        const r = li.getBoundingClientRect();
        const top = Math.max(r.top, clip.top), bottom = Math.min(r.bottom, clip.bottom);
        return bottom > top ? {id: li.dataset.id, top, bottom, left: r.left, right: r.right} : null;
    }).filter(Boolean);
    return {list: {top: clip.top, bottom: clip.bottom}, scrolled: area.scrollTop,
        author: {top: at.top, bottom: at.bottom, left: at.left, right: at.right}, drawn};
}"""

# The open bar, its field, the author filter and the first row shown, as boxes.
BAR_JS = """() => {
    const box = (node) => { const r = node.getBoundingClientRect();
        return {top: r.top, bottom: r.bottom, left: r.left, right: r.right, width: r.width, height: r.height}; };
    const first = [...document.querySelectorAll('#discord-log > li[data-id]')].find((li) => li.getClientRects().length);
    const author = document.getElementById('author-filter');
    const at = box(author);
    const hit = document.elementFromPoint((at.left + at.right) / 2, (at.top + at.bottom) / 2);
    return {bar: box(document.getElementById('search-float')), field: box(document.getElementById('search-field')),
        author: at, links: box(document.getElementById('links-filter')), first: first ? box(first) : null,
        pressable: Boolean(hit && author.contains(hit)), width: window.innerWidth};
}"""
TOP_JS = """(id) => {
    const row = document.querySelector(`#discord-log > li[data-id='${id}']`);
    return row && row.getClientRects().length ? row.getBoundingClientRect().top : null;
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


class Walk:
    """One reader at one size."""

    def __init__(self, page: Page, label: str, touch: bool, shots: Path | None) -> None:
        self.page = page
        self.label = label
        self.touch = touch
        self.shots = shots
        self.notes: list[str] = []

    def check(self, condition: bool, why: str) -> None:
        check(condition, f"{self.label}: {why}")

    def shot(self, name: str) -> None:
        if self.shots is not None:
            self.page.screenshot(path=str(self.shots / f"{self.label}-search-dates-{name}.png"))

    def settled(self) -> None:
        # Two frames, so what the page did has been laid out and its observers have answered.
        self.page.evaluate("() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done)))")

    def until(self, script: str, why: str, seconds: float = 10) -> None:
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline and not self.page.evaluate(script):
            self.page.wait_for_timeout(25)
        self.check(bool(self.page.evaluate(script)), why)

    def press(self, selector: str) -> None:
        box = self.page.locator(selector).bounding_box()
        self.check(box is not None, f"{selector} is not on the screen")
        if box is None:
            return
        x, y = box["x"] + box["width"] / 2, box["y"] + box["height"] / 2
        if self.touch:
            self.page.touchscreen.tap(x, y)
        else:
            self.page.mouse.click(x, y)
        self.settled()

    def press_row(self, message: str) -> None:
        x, y = self.page.evaluate(PRESS_AT_JS, message)
        if self.touch:
            self.page.touchscreen.tap(float(x), float(y))
        else:
            self.page.mouse.move(float(x), float(y))
            self.page.mouse.click(float(x), float(y))
        self.settled()

    def top(self, message: str) -> float:
        found = self.page.evaluate(TOP_JS, message)
        self.check(found is not None, f"{message} is not on the screen")
        return float(found or 0)

    def rows(self) -> list[Json]:
        found = self.page.evaluate(ROWS_JS)
        return [row for row in found if isinstance(row, dict)] if isinstance(found, list) else []


def number(box: object, edge: str) -> float:
    """One edge of a box `ROWS_JS` measured."""
    if not isinstance(box, dict):
        return 0.0
    value = box.get(edge)
    return float(value) if isinstance(value, (int, float)) else 0.0


def part(row: Json, key: str) -> Json | None:
    """One part of what `ROWS_JS` measured of a row, or None where it has none."""
    found = row.get(key)
    return {str(name): value for name, value in found.items()} if isinstance(found, dict) else None


def parts(row: Json, key: str) -> list[Json]:
    """A list of parts of what `ROWS_JS` measured of a row."""
    found = row.get(key)
    if not isinstance(found, list):
        return []
    return [{str(name): value for name, value in one.items()} for one in found if isinstance(one, dict)]


def said(row: Json, key: str) -> str:
    """The words of a row's line or of its fold's, or "" where it has none."""
    found = part(row, key)
    return str(found.get("text", "")) if found is not None else ""


def lines_between(walk: Walk, rows: list[Json], why: str) -> int:
    """Every line drawn above a row stands between the row before and its own, touching neither; and
    one drawn below the last stands under it. Answers how many there were."""
    count = 0
    for at, row in enumerate(rows):
        for kind in ("above", "below"):
            drawn = part(row, kind)
            if drawn is None:
                continue
            # As wide as the list, whatever side of it the row is drawn on.
            for edge in ("left", "right"):
                walk.check(abs(number(drawn.get("box"), edge) - number(row.get("list"), edge)) <= 1.5,
                           f"{why}: the line {kind} {row['id']} ({row['who']}) is not as wide as the list: its {edge} "
                           f"edge is at {number(drawn.get('box'), edge):.1f}px, the list's at {number(row.get('list'), edge):.1f}px")
        above = part(row, "above")
        if above is not None:
            count += 1
            line = above.get("box")
            walk.check(number(line, "bottom") <= number(row.get("box"), "top") + TOLERANCE_PX,
                       f"{why}: the line above {row['id']} overlaps its row")
            bookmark = row.get("bookmark")
            if bookmark is not None:
                walk.check(number(line, "bottom") <= number(bookmark, "top") + TOLERANCE_PX,
                           f"{why}: the line above {row['id']} is not above its bookmark")
            if at > 0:
                before = rows[at - 1]
                under = part(before, "below")
                floor = number(under.get("box"), "bottom") if under is not None else number(before.get("box"), "bottom")
                walk.check(number(line, "top") >= floor - TOLERANCE_PX,
                           f"{why}: the line above {row['id']} overlaps {before['id']} above it")
        below = part(row, "below")
        if below is not None:
            count += 1
            walk.check(number(below.get("box"), "top") >= number(row.get("box"), "bottom") - TOLERANCE_PX,
                       f"{why}: the line below {row['id']} overlaps its row")
    return count


def gap_lines_fit(walk: Walk, rows: list[Json]) -> int:
    """Every gap's line that says its days is on one line, or on two broken between its count and its
    days — "… 5 unmatched messages ·" over "Today, Oct 12, Mon …" — and never inside either, nor with
    a lone "…" on a line of its own; and its words stay inside the room the row keeps for it. Answers
    how many were on two."""
    wrapped = 0
    for row in rows:
        for kind in ("above", "below"):
            drawn = part(row, kind)
            if drawn is None or " · " not in str(drawn.get("text", "")):
                continue
            layout = drawn.get("layout")
            walk.check(isinstance(layout, dict), f"the line {kind} {row['id']} is not one run of text")
            if not isinstance(layout, dict):
                continue
            lines = number(layout, "lines")
            walk.check(lines in (1, 2), f"the line {kind} {row['id']} ({drawn.get('text')!r}) is on {lines:.0f} lines")
            walk.check(number(layout, "count") == 1 and number(layout, "days") == 1,
                       f"the line {kind} {row['id']} ({drawn.get('text')!r}) breaks inside its count or its days")
            box = drawn.get("box")
            walk.check(number(layout, "top") >= number(box, "top") - 0.5 and number(layout, "bottom") <= number(box, "bottom") + 0.5,
                       f"the words of the line {kind} {row['id']} run out of its room: {number(layout, 'top'):.1f}-"
                       f"{number(layout, 'bottom'):.1f}px in {number(box, 'top'):.1f}-{number(box, 'bottom'):.1f}px")
            wrapped += 1 if lines == 2 else 0
    return wrapped


def bar_geometry(walk: Walk) -> None:
    """The author filter is a whole 44px target a tap reaches, on the screen: on the bar's line left of
    Links where the bar has the room, and hanging under the bar's left end on a narrow phone, so the
    field keeps 140px either way. Hanging, it covers no row: not the first at the list's head, and not
    any row of a list scrolled half way down, which passes under the bar but never under the filter."""
    found = walk.page.evaluate(BAR_JS)
    measured: Json = {str(name): value for name, value in found.items()} if isinstance(found, dict) else {}
    author, bar, field = measured.get("author"), measured.get("bar"), measured.get("field")
    walk.check(number(author, "width") >= 43.5 and number(author, "height") >= 43.5,
               f"the author filter's target is {number(author, 'width'):.1f}x{number(author, 'height'):.1f}px")
    walk.check(bool(measured.get("pressable")), "a tap at the author filter's centre lands on something else")
    walk.check(number(author, "left") >= 0 and number(author, "right") <= number(measured, "width") + 0.5,
               "the author filter is off the screen")
    walk.check(number(field, "width") >= 140, f"the field is {number(field, 'width'):.0f}px wide beside the filters")
    if number(measured, "width") <= HANGS_UNDER_PX:
        walk.check(number(author, "top") >= number(bar, "bottom") - 0.5, "on a narrow phone the author filter is on the bar's line")
        first = measured.get("first")
        walk.check(first is None or number(first, "top") >= number(author, "bottom") - 0.5,
                   "the author filter hangs over the first row")
        found = walk.page.evaluate(SCROLLED_JS)
        walk.settled()
        scrolled: Json = {str(name): value for name, value in found.items()} if isinstance(found, dict) else {}
        hanging = scrolled.get("author")
        walk.check(number(scrolled, "scrolled") > 0, "the list did not scroll, so the scrolled case was not looked at")
        walk.check(number(scrolled.get("list"), "top") >= number(hanging, "bottom") - 0.5,
                   f"scrolled, the list is drawn from {number(scrolled.get('list'), 'top'):.1f}px, above the author "
                   f"filter's foot at {number(hanging, 'bottom'):.1f}px")
        drawn = scrolled.get("drawn")
        covered = [row.get("id") for row in (drawn if isinstance(drawn, list) else []) if isinstance(row, dict) and
                   number(row, "top") < number(hanging, "bottom") - 0.5 and number(row, "bottom") > number(hanging, "top") + 0.5 and
                   number(row, "left") < number(hanging, "right") - 0.5 and number(row, "right") > number(hanging, "left") + 0.5]
        walk.check(not covered, f"scrolled, the author filter covers part of {covered}")
        walk.page.evaluate("() => { document.getElementById('scroll-area').scrollTop = 0; }")
        walk.settled()
    else:
        walk.check(abs(number(author, "right") - number(measured.get("links"), "left")) <= 0.5 and
                   number(author, "top") >= number(bar, "top") - 0.5 and number(author, "bottom") <= number(bar, "bottom") + 0.5,
                   "the author filter is not on the bar's line, right beside Links")


def walk_through(walk: Walk) -> None:
    page = walk.page
    page.fill("#api-token", TOKEN)
    page.click("#save-token")
    page.wait_for_selector("#search-toggle", state="visible", timeout=10_000)
    page.click("#view-switch")
    walk.until(f"() => document.querySelectorAll('#discord-log > li[data-id]').length === {len(SCRIPT)}",
               "the channel did not open with the week in it")
    walk.settled()

    # 1. The days, each ruled off above its first message, and the bookmark's day.
    rows = walk.rows()
    days = [(str(row["id"]), said(row, "above")) for row in rows if row.get("seam") == "day"]
    walk.check([words for _, words in days] == ["Oct 5, Mon", "Oct 6, Tue", "Oct 7, Wed", "Oct 8, Thu", "Oct 9, Fri",
                                                 "Oct 10, Sat", "Yesterday, Oct 11, Sun", "Today, Oct 12, Mon"],
               f"the days are not ruled off as the owner asked: {days}")
    walk.check(all((part(row, "above") or {}).get("style") == "solid" for row in rows if row.get("seam") == "day"),
               "a day's rule is not drawn solid")
    page.evaluate(PARK_JS, [message_id(BOOKMARKED), 0.4])
    walk.settled()
    rows = walk.rows()
    marked = next((row for row in rows if row["id"] == message_id(BOOKMARKED)), None)
    walk.check(marked is not None and marked.get("seam") == "day" and marked.get("bookmark") is not None,
               "the bookmarked message does not carry both its day's rule and the bookmark")
    lines_between(walk, rows, "the days")
    walk.shot("1-days-bookmark")
    walk.notes.append(f"{len(days)} day rules")

    # 2. A search: its hits lit where they are, and its gaps said with the days they reach.
    walk.press("#search-toggle")
    page.fill("#search-field", "engine")
    walk.until("() => document.querySelectorAll('#discord-log > li[data-id]:not(.search-hidden)').length === 4",
               "the search did not leave the four messages holding the word")
    # The hits are marked once the typing pauses.
    walk.until("() => document.querySelectorAll('#discord-log mark.search-hit').length >= 3",
               "the hits were not marked once the typing paused")
    walk.settled()
    page.evaluate("() => { document.getElementById('scroll-area').scrollTop = 0; }")
    walk.settled()
    rows = walk.rows()
    hits = [mark for row in rows for mark in parts(row, "marks")]
    walk.check(len(hits) >= 3 and all(number(mark.get("box"), "right") > number(mark.get("box"), "left")
                                      for mark in hits),
               f"the hits are not marked where they are: {hits}")
    walk.check(len({mark["background"] for mark in hits}) == 1 and hits[0]["background"] not in ("", "rgba(0, 0, 0, 0)"),
               "a hit is not drawn in the highlighter's colour")
    walk.check(any(row.get("outlined") == ["solid"] for row in rows), "the link found by its address alone is not outlined")
    folded = next((part(row, "excerpt") for row in rows if part(row, "excerpt") is not None), None)
    walk.check(folded is not None and "engine" in str(folded.get("text")) and number(folded, "marks") >= 1,
               "the long answer's hit past its fold is not said under it")
    gaps = [said(row, "above") for row in rows if row.get("seam") == "gap"]
    walk.check(any(" · " in gap for gap in gaps), f"no gap's line says the days it reaches: {gaps}")
    wrapped = gap_lines_fit(walk, rows)
    walk.check(part(rows[-1], "below") is not None, "the messages after the last match are not said under it")
    count = lines_between(walk, rows, "the search")
    walk.shot("2-search-gaps-days")
    walk.notes.append(f"{len(hits)} hits, {count} lines, {wrapped} on two: " + " / ".join(gaps))

    # 3. Only the owner's prompts, with nothing typed, the prompt chosen kept where it stood.
    page.fill("#search-field", "")
    walk.until(f"() => document.querySelectorAll('#discord-log > li[data-id]:not(.search-hidden)').length === {len(SCRIPT)}",
               "clearing the text did not bring the week back")
    walk.settled()
    page.evaluate("() => { document.getElementById('scroll-area').scrollTop = 0; }")
    walk.settled()
    bar_geometry(walk)
    chosen = message_id(CHOSEN)
    page.evaluate(PARK_JS, [chosen, 0.45])
    walk.settled()
    walk.press_row(chosen)
    before = walk.top(chosen)
    walk.press("#author-filter")
    walk.until("() => document.getElementById('author-filter').dataset.author === 'mine'",
               "a press on the author filter did not show only the owner's messages")
    walk.settled()
    rows = walk.rows()
    mine = sum(1 for _, _, _, who, _ in SCRIPT if who == "owner")
    walk.check(len(rows) == mine and all(row["who"] == "me" for row in rows),
               f"Mine does not show exactly the owner's {mine} prompts")
    count_text = str(page.evaluate("() => document.getElementById('search-count').textContent"))
    walk.check(count_text == f"{mine} of {len(SCRIPT)} loaded", f"the count says {count_text!r}")
    after = walk.top(chosen)
    walk.check(abs(after - before) <= TOLERANCE_PX, f"Mine moved the chosen prompt from {before:.1f}px to {after:.1f}px")
    lines_between(walk, rows, "Mine")
    walk.shot("3-mine-no-term")
    for _ in range(2):
        walk.press("#author-filter")
    walk.until("() => document.getElementById('author-filter').dataset.author === 'everyone'",
               "two more presses did not come back to everyone's messages")
    walk.settled()
    back = walk.top(chosen)
    walk.check(abs(back - before) <= TOLERANCE_PX, f"back to everyone's, the prompt stands at {back:.1f}px, not {before:.1f}px")
    walk.notes.append(f"Mine {count_text}, the prompt at {before:.1f}px, {after:.1f}px and {back:.1f}px")


def launch(playwright: Playwright, args: argparse.Namespace, profile: str, width: int, height: int,
           touch: bool) -> BrowserContext:
    if touch:
        return playwright.chromium.launch_persistent_context(
            profile, headless=True, executable_path=args.browser_executable, timezone_id=ZONE,
            viewport={"width": width, "height": height}, color_scheme="dark",
            device_scale_factor=2.625, is_mobile=True, has_touch=True,
        )
    return playwright.chromium.launch_persistent_context(
        profile, headless=True, executable_path=args.browser_executable, timezone_id=ZONE,
        viewport={"width": width, "height": height}, color_scheme="dark",
    )


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
    report: list[str] = []
    with sync_playwright() as playwright:
        for label, width, height, touch in SIZES:
            api = WeekApi()
            server, thread, url = serve(api, web_root)
            try:
                with tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-search-dates-") as profile:
                    context = launch(playwright, args, profile, width, height, touch)
                    # The clock is fixed, so Today is the same day whenever this runs; timers still run.
                    context.clock.set_fixed_time(NOW)
                    page = context.pages[0]
                    errors: list[str] = []
                    page.on("pageerror", lambda error: errors.append(str(error)))
                    page.goto(url, wait_until="load")
                    walk = Walk(page, label, touch, args.screenshots)
                    walk_through(walk)
                    check(not errors, f"{label}: the page threw: {errors}")
                    version = context.browser.version if context.browser else "Chromium"
                    context.close()
                    report.append(f"{version} {label} {width}x{height}: " + "; ".join(walk.notes))
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
