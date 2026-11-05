#!/usr/bin/env python3
"""Highlight one thread card and see every other one go grey, in real Chromium at 1280x800 and 412x915.

`#277 thread-card-focus`. The owner, 2026-10-13: "It is VERY busy with all the colored lines... Let's
make it so when we highlight a thread card all the other lines grey out and we can see what we've
highlighted. I also don't like the cards expanding. Let's leave the thread cards contracted. Let's
CLICK to put a persistent highlight and expand on one card (NOT enter the thread). To enter the thread
you can click the 'N reply M unread' bit let's say or press '->' to go into the thread." The fake-DOM
suite (tests/js/voice_page.test.mjs) pins what the page does; it draws nothing, so it cannot say
whether a line on the screen is grey. This does, from the colour Chromium computes for each line and
from the pixels a screenshot holds where it is drawn.

It serves the checked-out page and the fake API of tests/two_column_browser.py — a channel taller than
the window with three threads, one opened by a long question — from one loopback origin.

At 1280x800, with a mouse and the keys, in two columns over the cards:

  Main scrolled until the long question's root and another root are on screen, both linked to their
  cards -> every card three lines and one height, every card's edge and every link in its thread's
  colour, by the computed colour and by the pixels drawn -> the mouse over the long card: selected,
  still three lines, no card a pixel taller -> a click on it: highlighted and shown whole, its thread
  not entered, every other card its old height; every other card's edge the page's grey, by its
  computed colour and its pixels, the highlighted card's still its thread's colour; every other link
  marked grey, drawn in the grey and fainter, the highlighted card's link heavy, opaque and in its
  colour, by computed colour and by the pixels of each link's run through the gap -> the mouse over
  another card: the highlight and the grey stay -> the highlighted card's replies button: its thread
  on the right -> <- back out to the cards, the keys on that card, still highlighted -> Esc: the
  highlight out, every edge and link in its colour again -> j to the next card and ->: its thread
  -> Esc: the cards again.

At 412x915, with a finger, in one column over the Threads list:

  every card its thread's name and its replies button, its edge in its thread's colour, what the
  thread is about not drawn -> a tap on the name of a card whose first message says more than its
  name: highlighted, that message drawn under the name, the list still on screen; every other card's edge the page's grey, by its computed
  colour and its pixels, the highlighted card's still its colour -> a tap on its replies button: its
  thread -> Back: the Threads list, the card still highlighted.

No horizontal overflow and no page error at any step. Pass --screenshots DIR to keep a PNG of each
state for review; --web-root DIR to run the same walk against another copy of the page.
"""

from __future__ import annotations

import argparse
import base64
import os
import tempfile
import threading
from http.server import ThreadingHTTPServer
from pathlib import Path
from typing import TYPE_CHECKING

import offline_cache_browser as fixture
from offline_cache_browser import TOKEN, Json, check, handler_for
from two_column_browser import CANARY, REPLICA, THREAD_ROWS, ColumnsApi, Desk

if TYPE_CHECKING:
    from playwright.sync_api import BrowserType

# (label, width, height, phone): two columns at a desk, one column on a phone.
SIZES = (("desk-1280", 1280, 800, False), ("phone-412", 412, 915, True))

# A line drawn in a thread's colour, `hsl(hue 55% 50%)`, is about 140 apart between its strongest and
# weakest channel; the page's grey is under 20. Between the two, these say which a line is.
COLOURED = 90
GREY = 30
# A pixel blends what is drawn with what is under it, a link's fainter for its opacity.
COLOURED_PIXEL = 50

# Each card of a list, each link between the columns, the page's grey, and where the reader is, as
# Chromium draws them. `list` is "side-cards" or "thread-list".
STATE_JS = """(list) => {
    const rgb = (text) => (String(text).match(/[0-9.]+/g) || []).slice(0, 3).map(Number);
    const hex = (text) => { const m = /#([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})/i.exec(String(text));
        return m ? [1, 2, 3].map((at) => parseInt(m[at], 16)) : []; };
    const right = list === 'side-cards';
    const cards = [...document.querySelectorAll(`#${list} > li`)].map((li) => {
        const r = li.getBoundingClientRect();
        const text = li.querySelector(right ? '.side-card-text' : '.thread-card-text');
        const button = li.querySelector(right ? '.side-card' : '.thread-open');
        const drawn = text.getClientRects().length > 0;
        return {id: li.getAttribute(right ? 'data-id' : 'data-context-id'),
                lit: li.getAttribute('data-highlighted') === 'true',
                selected: li.getAttribute('data-selected') === 'true',
                expanded: button.getAttribute('aria-expanded'),
                left: r.left, top: r.top, bottom: r.bottom, height: r.height,
                edge: rgb(getComputedStyle(li).borderLeftColor),
                about: drawn ? text.textContent : '',
                clamped: drawn && text.scrollHeight > text.clientHeight + 2,
                lines: drawn ? Math.round(text.clientHeight / parseFloat(getComputedStyle(text).lineHeight)) : 0};
    });
    const svg = document.getElementById('column-links');
    const corner = svg.getBoundingClientRect();
    const links = [...svg.querySelectorAll('path.column-link')].map((path) => {
        const line = (path.getAttribute('d').match(/-?\\d+(?:\\.\\d+)?/g) || []).map(Number);
        const style = getComputedStyle(path);
        const marks = getComputedStyle(path.nextElementSibling);
        const whole = path.getAttribute('data-link') === 'both';
        return {thread: path.getAttribute('data-thread'), whole,
                selected: path.hasAttribute('data-selected'), dimmed: path.hasAttribute('data-dimmed'),
                stroke: rgb(style.stroke), opacity: parseFloat(style.opacity),
                fill: rgb(marks.fill), fillOpacity: parseFloat(marks.opacity),
                lane: whole ? line[2] + corner.left : 0,
                from: whole ? line[1] + corner.top : 0, to: whole ? line[3] + corner.top : 0};
    });
    return {cards, links, grey: hex(getComputedStyle(document.documentElement).getPropertyValue('--thread-grey')),
            focus: document.activeElement && document.activeElement.matches(`#${list} > li`)
                ? document.activeElement.getAttribute(right ? 'data-id' : 'data-context-id') : null,
            overflow: document.documentElement.scrollWidth - window.innerWidth};
}"""

# The colour of each CSS-pixel point in a screenshot (`png`, base64) of the viewport.
PIXELS_JS = """async ({png, points}) => {
    const image = new Image();
    image.src = `data:image/png;base64,${png}`;
    await image.decode();
    const canvas = document.createElement('canvas');
    canvas.width = image.naturalWidth;
    canvas.height = image.naturalHeight;
    const context = canvas.getContext('2d');
    context.drawImage(image, 0, 0);
    const scale = image.naturalWidth / window.innerWidth;
    return points.map(([x, y]) => [...context.getImageData(Math.floor(x * scale), Math.floor(y * scale), 1, 1).data].slice(0, 3));
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
        help="directory to write one PNG per state into (default: none are kept)",
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
        for label, width, height, phone in SIZES:
            report.append(walk(playwright.chromium, args, label, width, height, phone))
    for line in report:
        print(line)
    return 0


def walk(chromium: BrowserType, args: argparse.Namespace, label: str, width: int, height: int, phone: bool) -> str:
    """One reader at one size, against a fresh fake API and a fresh browser profile."""
    api = ColumnsApi()
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(api))
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-card-focus-") as profile:
            if phone:
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
            desk = Desk(page, args.screenshots, label, width, height)
            page.goto(f"http://127.0.0.1:{server.server_port}/voice", wait_until="load")
            if phone:
                phone_walk(desk)
            else:
                desk_walk(desk)
            check(not errors, f"{label}: the page threw: {errors}")
            context.close()
            return f"{version} at {width}x{height}: " + "; ".join(desk.notes)
    finally:
        api.stopping.set()
        server.shutdown()
        server.server_close()
        thread.join()


# --- reading what was measured -----------------------------------------------------------------------


def items(found: Json, key: str) -> list[Json]:
    value = found[key]
    assert isinstance(value, list)
    return [item for item in value if isinstance(item, dict)]


def number(item: Json, key: str) -> float:
    value = item[key]
    assert isinstance(value, (int, float)), f"{key} is {value!r}"
    return float(value)


def colour(item: Json, key: str) -> list[float]:
    value = item[key]
    assert isinstance(value, list)
    channels = [float(channel) for channel in value if isinstance(channel, (int, float))]
    assert len(channels) == 3, f"{key} is {value!r}"
    return channels


def chroma(rgb: list[float]) -> float:
    """How far a colour is from grey: its strongest channel less its weakest."""
    return max(rgb) - min(rgb)


def near(one: list[float], other: list[float]) -> bool:
    return max(abs(a - b) for a, b in zip(one, other)) <= 2


def state(desk: Desk, list_id: str) -> Json:
    found: Json = desk.page.evaluate(STATE_JS, list_id)
    return found


def by_id(found: Json, key: str = "cards", field: str = "id") -> dict[str, Json]:
    return {str(item[field]): item for item in items(found, key)}


def pixels(desk: Desk, points: list[tuple[float, float]]) -> list[list[float]]:
    """The colour drawn at each viewport point, from a screenshot: what the reader sees there."""
    png = base64.b64encode(desk.page.screenshot()).decode("ascii")
    found = desk.page.evaluate(PIXELS_JS, {"png": png, "points": [list(point) for point in points]})
    assert isinstance(found, list)
    return [[float(channel) for channel in rgb] for rgb in found if isinstance(rgb, list)]


def edge_pixels(desk: Desk, found: Json) -> dict[str, list[float]]:
    """What is drawn halfway into each card's left edge, halfway down it, by card."""
    cards = items(found, "cards")
    points = [(number(card, "left") + 1.5, (number(card, "top") + number(card, "bottom")) / 2) for card in cards]
    return {str(card["id"]): rgb for card, rgb in zip(cards, pixels(desk, points))}


def lane_pixels(desk: Desk, found: Json) -> dict[str, list[float]]:
    """What is drawn halfway along each whole link's run through the gap, by thread: the run long enough to sample."""
    links = [link for link in items(found, "links") if link["whole"] and abs(number(link, "to") - number(link, "from")) >= 8]
    points = [(number(link, "lane"), (number(link, "from") + number(link, "to")) / 2) for link in links]
    return {str(link["thread"]): rgb for link, rgb in zip(links, pixels(desk, points))}


def no_overflow(desk: Desk, found: Json, where: str) -> None:
    desk.check(number(found, "overflow") <= 0, f"{where}: the page scrolls sideways")


# --- at a desk, the right column's cards -------------------------------------------------------------


def open_two_columns(desk: Desk) -> None:
    page = desk.page
    page.fill("#api-token", TOKEN)
    page.click("#save-token")
    page.wait_for_selector("#search-toggle", state="visible", timeout=10_000)
    page.click("#view-switch")
    desk.wait_for("() => document.querySelectorAll('#discord-log > li[data-id]').length === 24",
                  "the channel did not open in All")
    page.click("#column-toggle")
    desk.wait_for("() => document.querySelectorAll('#side-cards > li[data-card]').length === 3", "the cards are not listed")
    desk.settled()


def whole_links(desk: Desk) -> list[str]:
    desk.settled()
    desk.settled()
    return sorted(str(link["thread"]) for link in items(state(desk, "side-cards"), "links") if link["whole"])


def scroll_to_pairs(desk: Desk) -> list[str]:
    """Main scrolled, from its foot up, until the long question's root and another are linked to their cards."""
    page = desk.page
    end = float(page.evaluate("() => { const a = document.getElementById('scroll-area'); return a.scrollHeight - a.clientHeight; }"))
    top = end
    while top >= 0:
        page.evaluate(f"() => {{ document.getElementById('scroll-area').scrollTop = {top}; }}")
        drawn = whole_links(desk)
        if REPLICA in drawn and len(drawn) >= 2:
            return drawn
        top -= 80
    raise AssertionError(f"{desk.label}: no place in Main links the long question's root and another to their cards")


def check_coloured(desk: Desk, found: Json, where: str) -> None:
    """Every card's edge and every link in its thread's colour, as computed and as drawn; none marked grey."""
    for card in items(found, "cards"):
        desk.check(chroma(colour(card, "edge")) >= COLOURED, f"{where}: the edge of {card['id']} is {colour(card, 'edge')}, not its colour")
        desk.check(not card["lit"] and card["expanded"] == "false", f"{where}: {card['id']} is highlighted")
    for thread, rgb in edge_pixels(desk, found).items():
        desk.check(chroma(rgb) >= COLOURED_PIXEL, f"{where}: the edge of {thread} is drawn {rgb}, not in its colour")
    for link in items(found, "links"):
        desk.check(not link["dimmed"] and chroma(colour(link, "stroke")) >= COLOURED,
                   f"{where}: the link of {link['thread']} is {colour(link, 'stroke')}, dimmed {link['dimmed']}")
    for thread, rgb in lane_pixels(desk, found).items():
        desk.check(chroma(rgb) >= COLOURED_PIXEL, f"{where}: the link of {thread} is drawn {rgb}, not in its colour")


def check_greyed(desk: Desk, found: Json, lit: str, where: str) -> tuple[list[float], list[float]]:
    """Card `lit` in its colour and every other card's edge and link grey, as computed and as drawn.
    Returns a grey card's edge as drawn, and a grey link's, for the report."""
    grey = colour(found, "grey")
    desk.check(chroma(grey) <= GREY, f"{where}: the page's grey is {grey}")
    for card in items(found, "cards"):
        edge = colour(card, "edge")
        if card["id"] == lit:
            desk.check(bool(card["lit"]) and card["expanded"] == "true", f"{where}: {lit} is not highlighted")
            desk.check(chroma(edge) >= COLOURED, f"{where}: the highlighted card's edge is {edge}, not its colour")
        else:
            desk.check(not card["lit"], f"{where}: {card['id']} is highlighted beside {lit}")
            desk.check(near(edge, grey), f"{where}: the edge of {card['id']} is {edge}, not the grey {grey}")
    drawn = edge_pixels(desk, found)
    for thread, rgb in drawn.items():
        if thread == lit:
            desk.check(chroma(rgb) >= COLOURED_PIXEL, f"{where}: the highlighted card's edge is drawn {rgb}")
        else:
            desk.check(chroma(rgb) <= GREY, f"{where}: the edge of {thread} is drawn {rgb}, not grey")
    grey_edge = next(rgb for thread, rgb in drawn.items() if thread != lit)
    links = items(found, "links")
    desk.check(any(link["thread"] == lit for link in links) and any(link["thread"] != lit for link in links),
               f"{where}: the links drawn are {[link['thread'] for link in links]}, so this proves little")
    for link in links:
        stroke, fill = colour(link, "stroke"), colour(link, "fill")
        if link["thread"] == lit:
            desk.check(bool(link["selected"]) and not link["dimmed"], f"{where}: the highlighted card's link is not drawn heavy")
            desk.check(chroma(stroke) >= COLOURED and number(link, "opacity") == 1,
                       f"{where}: the highlighted card's link is {stroke} at {number(link, 'opacity')}")
        else:
            desk.check(bool(link["dimmed"]) and not link["selected"], f"{where}: the link of {link['thread']} is not marked grey")
            desk.check(near(stroke, grey) and near(fill, grey), f"{where}: the link of {link['thread']} is {stroke} and {fill}, not {grey}")
            desk.check(number(link, "opacity") < 0.8 and number(link, "fillOpacity") < 0.8,
                       f"{where}: the grey link of {link['thread']} is as strong as the others were")
    runs = lane_pixels(desk, found)
    desk.check(lit in runs and any(thread != lit for thread in runs), f"{where}: the links' runs sampled are {sorted(runs)}")
    for thread, rgb in runs.items():
        if thread == lit:
            desk.check(chroma(rgb) >= COLOURED_PIXEL, f"{where}: the highlighted card's link is drawn {rgb}")
        else:
            desk.check(chroma(rgb) <= GREY, f"{where}: the link of {thread} is drawn {rgb}, not grey")
    grey_run = next(rgb for thread, rgb in runs.items() if thread != lit)
    return grey_edge, grey_run


def card_point(desk: Desk, list_id: str, card_id: str, part: str) -> tuple[float, float]:
    """The middle of `part` of a card: where a pointer presses it."""
    attribute = "data-id" if list_id == "side-cards" else "data-context-id"
    box = desk.page.locator(f"#{list_id} > li[{attribute}='{card_id}'] {part}").first.bounding_box()
    desk.check(box is not None, f"{part} of {card_id} is not drawn")
    assert box is not None
    return box["x"] + box["width"] / 2, box["y"] + min(box["height"] / 2, 12)


def desk_walk(desk: Desk) -> None:
    page = desk.page
    open_two_columns(desk)
    drawn = scroll_to_pairs(desk)
    page.mouse.move(5, desk.height - 5)
    desk.settled()
    before = state(desk, "side-cards")
    cards = by_id(before)
    heights = {round(number(card, "height")) for card in cards.values()}
    desk.check(len(heights) == 1, f"the cards are not one height: {heights}")
    desk.check(bool(cards[REPLICA]["clamped"]) and number(cards[REPLICA], "lines") == 3, "the long card is not three lines")
    check_coloured(desk, before, "before a highlight")
    no_overflow(desk, before, "before a highlight")
    desk.shot("1-cards")

    # The mouse over the long card: selected, and still three lines.
    page.mouse.move(*card_point(desk, "side-cards", REPLICA, ".side-card-text"))
    desk.wait_for(f"() => document.querySelector(\"#side-cards > li[data-id='{REPLICA}']\").getAttribute('data-selected') === 'true'",
                  "the card under the mouse is not the selected one")
    desk.settled()
    hovered = by_id(state(desk, "side-cards"))
    desk.check(bool(hovered[REPLICA]["clamped"]) and number(hovered[REPLICA], "lines") == 3, "the card under the mouse grew")
    for card_id, card in hovered.items():
        desk.check(round(number(card, "height")) == round(number(cards[card_id], "height")), f"{card_id} changed height under the mouse")
        desk.check(not card["lit"], f"the mouse highlighted {card_id}")

    # A click: highlighted, shown whole, not entered; every other card and link grey.
    page.mouse.click(*card_point(desk, "side-cards", REPLICA, ".side-card-text"))
    desk.wait_for(f"() => document.querySelector(\"#side-cards > li[data-id='{REPLICA}']\").getAttribute('data-highlighted') === 'true'",
                  "a click on the card did not highlight it")
    desk.settled()
    desk.settled()
    desk.check(page.is_visible("#side-list") and not page.is_visible("#side-pane"), "a click on the card entered its thread")
    lit = state(desk, "side-cards")
    shown = by_id(lit)
    desk.check(not shown[REPLICA]["clamped"] and number(shown[REPLICA], "lines") > 3,
               f"the highlighted card does not show its whole text: {shown[REPLICA]}")
    for card_id, card in shown.items():
        if card_id != REPLICA:
            desk.check(round(number(card, "height")) == round(number(cards[card_id], "height")), f"{card_id} changed height")
    desk.check(whole_links(desk) == drawn, f"the links drawn whole changed with the highlight: {whole_links(desk)} from {drawn}")
    grey_edge, grey_run = check_greyed(desk, lit, REPLICA, "highlighted")
    no_overflow(desk, lit, "highlighted")
    desk.shot("2-highlighted")
    desk.notes.append(f"a click highlighted the long card, {number(shown[REPLICA], 'lines'):.0f} lines, not entering it; "
                      f"the other cards' edges went from their colours to {grey_edge} on the screen and their links' runs "
                      f"to {grey_run}, the highlighted card's link heavy in its colour")

    # The mouse over another card: the highlight and the grey stay.
    page.mouse.move(*card_point(desk, "side-cards", CANARY, ".side-card-text"))
    desk.wait_for(f"() => document.querySelector(\"#side-cards > li[data-id='{CANARY}']\").getAttribute('data-selected') === 'true'",
                  "the card under the mouse is not the selected one")
    desk.settled()
    desk.settled()
    check_greyed(desk, state(desk, "side-cards"), REPLICA, "the mouse over another card")

    # Its replies button: its thread. <- back out to the cards, on that card, still highlighted.
    page.mouse.click(*card_point(desk, "side-cards", REPLICA, ".side-card-open"))
    desk.wait_for(f"() => !document.getElementById('side-pane').hidden"
                  f" && document.querySelectorAll('#side-log > li[data-id]').length === {THREAD_ROWS[REPLICA]}",
                  "the replies button did not enter the card's thread")
    page.keyboard.press("ArrowLeft")
    desk.wait_for("() => !document.getElementById('side-list').hidden && document.getElementById('side-pane').hidden",
                  "<- from a thread entered from its card did not come back to the cards")
    desk.wait_for(f"() => document.activeElement && document.activeElement.matches(\"#side-cards > li[data-id='{REPLICA}']\")",
                  "<- did not put the keys on the card the thread was entered from")
    desk.settled()
    desk.settled()
    back = state(desk, "side-cards")
    desk.check(bool(by_id(back)[REPLICA]["lit"]), "coming back out lost the highlight")

    # Esc: the highlight out, every colour back.
    page.keyboard.press("Escape")
    desk.wait_for("() => !document.querySelector('#side-cards > li[data-highlighted]')", "Esc did not put the highlight out")
    desk.settled()
    desk.settled()
    desk.check(page.is_visible("#side-list"), "Esc left the cards")
    check_coloured(desk, state(desk, "side-cards"), "after Esc")
    desk.shot("3-put-out")

    # j to the next card and ->: its thread; Esc back to the cards.
    page.keyboard.press("j")
    desk.wait_for(f"() => document.activeElement && document.activeElement.matches(\"#side-cards > li[data-id='{CANARY}']\")",
                  "j did not move the keys to the next card")
    page.keyboard.press("ArrowRight")
    desk.wait_for(f"() => !document.getElementById('side-pane').hidden"
                  f" && document.querySelectorAll('#side-log > li[data-id]').length === {THREAD_ROWS[CANARY]}",
                  "-> did not enter the thread of the card the keys are on")
    page.keyboard.press("Escape")
    desk.wait_for("() => !document.getElementById('side-list').hidden", "Esc in the thread did not come back to the cards")
    desk.notes.append("the mouse over another card left the highlight and the grey; its replies button entered its thread, "
                      "<- came back to it still highlighted, Esc put the colours back, and j then -> entered the next")


# --- on a phone, the Threads list ---------------------------------------------------------------------


def phone_walk(desk: Desk) -> None:
    page = desk.page
    page.fill("#api-token", TOKEN)
    page.click("#save-token")
    page.wait_for_selector("#search-toggle", state="visible", timeout=10_000)
    page.click("#view-switch")
    desk.wait_for("() => document.querySelectorAll('#discord-log > li[data-id]').length === 24",
                  "the channel did not open in All")
    # The Threads list: one column's list of thread cards, drawn as the view tabs drew it.
    page.evaluate("() => document.getElementById('channel-view-threads').click()")
    desk.wait_for("() => !document.getElementById('thread-list').hidden"
                  " && document.querySelectorAll('#thread-list > li.thread-card').length === 3",
                  "the Threads list is not on screen")
    desk.settled()
    before = state(desk, "thread-list")
    for card in items(before, "cards"):
        desk.check(chroma(colour(card, "edge")) >= COLOURED, f"the edge of {card['id']} is {colour(card, 'edge')}")
        desk.check(card["about"] == "" and not card["lit"], f"{card['id']} draws what its thread is about, contracted")
    for thread, rgb in edge_pixels(desk, before).items():
        desk.check(chroma(rgb) >= COLOURED_PIXEL, f"the edge of {thread} is drawn {rgb}, not in its colour")
    no_overflow(desk, before, "the Threads list")
    desk.shot("1-threads")

    # A tap on the card's name: highlighted, what it is about drawn, the list still up; the rest grey.
    # The canary's card, whose name says less than its first message: the long question's name is the
    # whole of it, so there is nothing more for its card to draw.
    page.touchscreen.tap(*card_point(desk, "thread-list", CANARY, ".thread-open strong"))
    desk.wait_for(f"() => document.querySelector(\"#thread-list > li[data-context-id='{CANARY}']\").getAttribute('data-highlighted') === 'true'",
                  "a tap on the card did not highlight it")
    desk.settled()
    desk.settled()
    desk.check(page.is_visible("#thread-list") and not page.is_visible("#thread-heading"), "a tap on the card entered its thread")
    lit = state(desk, "thread-list")
    shown = by_id(lit)
    desk.check(str(shown[CANARY]["about"]).startswith("Message 215. Thread starts here."),
               f"the highlighted card does not say what its thread is about: {shown[CANARY]['about']!r}")
    grey = colour(lit, "grey")
    drawn = edge_pixels(desk, lit)
    for card_id, card in shown.items():
        edge = colour(card, "edge")
        if card_id == CANARY:
            desk.check(chroma(edge) >= COLOURED and chroma(drawn[card_id]) >= COLOURED_PIXEL,
                       f"the highlighted card's edge is {edge}, drawn {drawn[card_id]}")
        else:
            desk.check(near(edge, grey), f"the edge of {card_id} is {edge}, not the grey {grey}")
            desk.check(chroma(drawn[card_id]) <= GREY, f"the edge of {card_id} is drawn {drawn[card_id]}, not grey")
            desk.check(card["about"] == "", f"{card_id} draws what its thread is about beside the highlighted card")
    no_overflow(desk, lit, "highlighted")
    desk.shot("2-highlighted")
    grey_edge = next(rgb for card_id, rgb in drawn.items() if card_id != CANARY)
    desk.notes.append(f"a tap highlighted a card and drew what its thread is about, not entering it; the others' edges "
                      f"went grey, {grey_edge} on the screen")

    # Its replies button: the thread. Back: the list, still highlighted.
    page.touchscreen.tap(*card_point(desk, "thread-list", CANARY, ".thread-card-open"))
    desk.wait_for("() => !document.getElementById('thread-heading').hidden"
                  f" && document.querySelectorAll('#discord-log > li[data-id]').length === {THREAD_ROWS[CANARY]}",
                  "the replies button did not enter the card's thread")
    desk.check(page.evaluate("() => document.getElementById('thread-select').value") == f"thread:{CANARY}",
               "the replies button entered another thread")
    desk.shot("3-thread")
    box = page.locator("#thread-back").bounding_box()
    desk.check(box is not None, "the thread has no way back")
    assert box is not None
    page.touchscreen.tap(box["x"] + box["width"] / 2, box["y"] + box["height"] / 2)
    desk.wait_for("() => !document.getElementById('thread-list').hidden", "Back did not come back to the Threads list")
    desk.settled()
    back = state(desk, "thread-list")
    desk.check(bool(by_id(back)[CANARY]["lit"]), "coming back to the Threads list lost the highlight")
    no_overflow(desk, back, "back on the Threads list")
    desk.notes.append("its replies button entered the thread, and Back came to the list with the card still highlighted")


if __name__ == "__main__":
    raise SystemExit(main())
