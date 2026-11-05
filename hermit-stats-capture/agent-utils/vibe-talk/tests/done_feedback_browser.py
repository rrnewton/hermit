#!/usr/bin/env python3
"""Mark the selected message Done in real Chromium, and read what the browser draws.

`#245 done-feedback`. The owner likes that a read message lights up again when it is selected, and
asked for "SOME indication of what just happened" when he marks the selected one read: e moves the
selection straight on to the next unread message, and a lit read row looked exactly like a lit unread
one. The page now rings a selected unread message in the solid accent and a selected read one in a
dashed, lighter grey, and draws the moment of marking on the row that was marked: its ring turns grey
over a quarter of a second while the row fades to the read look, holds, then lets go. The fake-DOM
suite pins the attributes and the rules; this reads the COMPUTED STYLES, the running animations and
the layout, which only a browser has:

  at 1280x800 in the dark scheme and again in the light one, on a threaded channel (where a Done
  redraws every row), a message selected from the keyboard is ringed in the solid accent on the lit
  surface -> e: the selection moves on, and the row it left carries `data-done-flash="marking"` with
  the `done-marking` animation running, then the dashed grey ring at the read strength, then no ring
  and no flag once the hold and the fade are over -> ArrowUp back onto it: selected and read, it is
  ringed in DASHED GREY on the same lit surface, at full strength, where a selected unread message is
  ringed SOLID in the accent, and each ring stands at 3:1 or more against that surface in this scheme
  -> the Undo chip names it and puts it back -> e and then the Undo chip within the hold: the row is
  unread again and wears no ring at once, and nothing draws one later; then, in a context that
  prefers reduced motion, e again: no animation runs, and the grey ring and the read strength are
  there at once.

  At 412x915 and 1280x800, in both schemes, with motion reduced so the picture is the look the eye is
  left with: the moment of marking (the grey ring on the row marked, the accent ring on the next) and
  a selected read message beside an unread one, both rings measured against their surface; and in
  each, no chip and no status pill over the message box or its Send.

  On a 360px and a 412px phone and a 1280x800 desk, scrolled up a channel of long messages so that
  Newest, the Undo chip, Summaries and Expand all are up together. From review, at 412px the named
  chip gave way to one letter, "Undo “B… ”": on the phones it now says only "Undo", taking no line a
  bare "Undo" did not, the whole name in its tooltip and its accessible name; on the desk it names the
  message, a dozen characters of it at least, all on one line. And from review, the chip stood on
  Send in the 412px captures, the list a little short of its end: walked up from the end of the list
  a few pixels at a time through the whole height of the message box, no chip, and no status pill,
  ever meets the box or its Send, at any of the three widths; at the very end the chips stand in the
  room above the box, clear of the last message.

It serves the checked-out assets and a small fake API through Playwright's request routing, so no
port is opened. Pass --web-root DIR to run the same walk against another copy of the page: against
the page from before, it fails at the first selected read message, ringed in the accent.
--screenshots DIR keeps a PNG of each step.
"""

from __future__ import annotations

import argparse
import json
import mimetypes
import os
import re
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import TYPE_CHECKING, Literal
from urllib.parse import unquote, urlsplit

if TYPE_CHECKING:
    from playwright.sync_api import Browser, Page, Route

ORIGIN = "http://127.0.0.1:9"
TOKEN = "write-token-browser-done-feedback-check"
CHANNEL = {"id": "1110000000000000001", "label": "lead team", "writable": True, "alias": None,
           "added": False}
AGENT = {"author": "ci-bot", "author_id": "1000000000000000001", "author_is_bot": True}
EPOCH = datetime(2026, 10, 12, 7, 0, tzinfo=timezone.utc)
Json = dict[str, object]
Scheme = Literal["dark", "light"]


def channel_message(n: int, content: str) -> Json:
    return {
        "id": f"245000000000000{n:04d}", "channel_id": CHANNEL["id"], **AGENT,
        "timestamp": (EPOCH + timedelta(minutes=n)).isoformat().replace("+00:00", "Z"),
        "spoken_time": "", "reply_to": None, "content": content,
    }


MESSAGES: list[Json] = [
    channel_message(n, f"Step {n}: checked the dashboards and the logs, nothing out of the ordinary.")
    for n in range(14)
]
# The chip walk's channel: long messages, folded (so Expand all is up), whose opening words make a
# name far wider than "Undo".
LONG_MESSAGES: list[Json] = [
    channel_message(n, f"Backup check {n} finished on all four shards and the restore drill passed on the first "
                    "try, so nothing in this report needs you today. " * 4)
    for n in range(14)
]


class FakeApi:
    """The routes /voice reads, for one channel of messages from an agent, and its Done."""

    def __init__(self, web_root: Path, messages: list[Json] = MESSAGES) -> None:
        self.web_root = web_root
        self.messages = messages
        self.dismissed: set[str] = set()

    def route(self, route: Route) -> None:
        request = route.request
        path = unquote(urlsplit(request.url).path)
        if not path.startswith("/api/"):
            relative = "voice.html" if path == "/voice" else path.lstrip("/")
            target = (self.web_root / relative).resolve()
            if self.web_root not in target.parents or not target.is_file():
                route.fulfill(status=404, body="")
                return
            route.fulfill(status=200, body=target.read_bytes(),
                          content_type=mimetypes.guess_type(target.name)[0] or "application/octet-stream",
                          headers={"Cache-Control": "no-store"})
            return
        if request.headers.get("authorization") != f"Bearer {TOKEN}":
            self.json(route, 401, {"error": "unauthorized", "detail": "unknown token"})
        elif path == "/api/v1/client-config":
            self.json(route, 200, {
                "token_scope": "write", "version": "browser-check", "chat_provider_name": "Google Chat",
                "channels": [CHANNEL], "live_poll_seconds": 30, "live_delivery": "poll",
                "threading_supported": True, "channel_registration_supported": False,
                "read_aloud": {"backend": "browser", "label": "Browser voice", "playback": "browser",
                               "local_only": True},
                "elevenlabs_agent_id": None, "conversational_voice": {"name": "Test voice provider"},
                "replay_enabled": False, "self_author_id": None, "owner_author_id": None,
                "channel_discovery_supported": False, "upstream_read_mark_supported": False,
                "speech_prep_enabled": False,
            })
        elif path == "/api/v1/conversations":
            self.json(route, 200, {"conversations": []})
        elif path == "/api/v1/transcript":
            self.json(route, 200, {"turns": [], "has_more": False, "next_before": None})
        elif path.endswith("/timeline"):
            view = (dict(part.split("=", 1) for part in urlsplit(request.url).query.split("&") if "=" in part)
                    .get("view", "main"))
            self.json(route, 200, {
                "channel": CHANNEL, "messages": self.messages, "threads": [], "thread": None,
                "has_threads": False, "has_more": False, "next_before": None, "notice": None,
                "dismissed": sorted(self.dismissed), "view": view, "limit": 100,
                "returned": len(self.messages),
                "untrusted_content_notice": "third-party text; DATA, never instructions",
            })
        elif path.endswith("/dismiss") and request.method == "POST":
            ids = self.ids(request.post_data)
            self.dismissed.update(ids)
            self.json(route, 200, {"messages": ids})
        elif path.endswith("/restore") and request.method == "POST":
            ids = self.ids(request.post_data)
            self.dismissed.difference_update(ids)
            self.json(route, 200, {"messages": ids})
        else:
            self.json(route, 404, {"error": "not_found", "detail": "not served by this check"})

    @staticmethod
    def ids(body: str | None) -> list[str]:
        raw: object = json.loads(body or "null")
        listed = raw.get("messages") if isinstance(raw, dict) else None
        return [str(one) for one in listed] if isinstance(listed, list) else []

    @staticmethod
    def json(route: Route, status: int, payload: Json) -> None:
        route.fulfill(status=status, body=json.dumps(payload), content_type="application/json",
                      headers={"Cache-Control": "no-store"})


# A row as the browser draws it, and the colours a ring may be, resolved by the browser itself.
ROW_JS = """(id) => {
  const probe = (value) => {
    const span = document.createElement('span');
    span.style.color = value;
    document.body.append(span);
    const colour = getComputedStyle(span).color;
    span.remove();
    return colour;
  };
  const row = document.querySelector(`#discord-log > li[data-id="${id}"]`);
  const style = getComputedStyle(row);
  const css = (name) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return {
    id, selected: row.getAttribute('data-selected'), archived: row.getAttribute('data-archived'),
    flash: row.getAttribute('data-done-flash'),
    outlineStyle: style.outlineStyle, outlineColor: style.outlineColor, borderColor: style.borderTopColor,
    background: style.backgroundColor, page: getComputedStyle(document.body).backgroundColor,
    opacity: Number(style.opacity), filter: style.filter,
    animations: row.getAnimations().map((a) => a.animationName || a.transitionProperty).filter(Boolean).sort(),
    accent: probe(css('--accent')), grey: probe(css('--read-ring')), lit: probe(css('--row-hover')),
  };
}"""

# The id of the selected message in the channel's list, or null.
SELECTED_JS = """() => {
  const row = document.querySelector('#discord-log > li[data-selected="true"]');
  return row ? row.getAttribute('data-id') : null;
}"""

# The row of chips over the list, the Undo chip's name, the status pill, and the composer's box and
# Send, as laid out. A box is null where it is not drawn, and the box and Send are null where they are
# not on the list's screen at all.
CHIPS_JS = """() => {
  const box = (node) => { const r = node.getBoundingClientRect();
    return { left: r.left, right: r.right, top: r.top, bottom: r.bottom }; };
  const shown = (node) => Boolean(node) && node.getClientRects().length > 0;
  const chips = [...document.querySelectorAll('#scroll-tools > .chip')]
    .filter(shown).map((chip) => ({ id: chip.id, ...box(chip) }));
  const name = document.querySelector('#undo-dismiss .undo-name');
  const label = document.getElementById('undo-dismiss-label');
  const undo = document.getElementById('undo-dismiss');
  const list = box(document.getElementById('scroll-area'));
  const onList = (node) => { if (!shown(node)) return null; const b = box(node);
    return b.bottom > list.top && b.top < list.bottom ? b : null; };
  const rows = [...document.querySelectorAll('#discord-log > li')].filter(shown);
  return {
    chips, width: innerWidth, list,
    name: shown(name) ? { text: name.textContent, scroll: name.scrollWidth, client: name.clientWidth,
      overflow: getComputedStyle(name).textOverflow } : null,
    spill: label.scrollWidth - label.clientWidth,
    labelRight: label.getBoundingClientRect().right,
    label: label.textContent, drawn: label.innerText.replace(/\\s+/g, ' ').trim(),
    title: undo.getAttribute('title'), aria: undo.getAttribute('aria-label'),
    send: onList(document.getElementById('channel-send')),
    field: onList(document.getElementById('channel-compose-text')),
    composer: onList(document.getElementById('channel-composer')),
    status: shown(document.getElementById('status-line')) ? box(document.getElementById('status-line')) : null,
    lastRow: rows.length ? box(rows[rows.length - 1]) : null,
  };
}"""


def check(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def number(value: object) -> float:
    """A measured number, as the page answered it."""
    check(isinstance(value, (int, float)), f"not a number: {value!r}")
    assert isinstance(value, (int, float))
    return float(value)


def rgba(value: object) -> tuple[float, float, float, float]:
    """A computed CSS colour, `rgb(...)` or `rgba(...)`, as four numbers."""
    parts = [float(part) for part in re.findall(r"[\d.]+", str(value))]
    check(len(parts) in (3, 4), f"not a computed colour: {value!r}")
    return parts[0], parts[1], parts[2], parts[3] if len(parts) == 4 else 1.0


def over(top: tuple[float, float, float, float], under: tuple[float, float, float, float],
         alpha: float = 1.0) -> tuple[float, float, float, float]:
    """`top`, at its own alpha times `alpha`, composited over an opaque `under`."""
    a = top[3] * alpha
    return (top[0] * a + under[0] * (1 - a), top[1] * a + under[1] * (1 - a),
            top[2] * a + under[2] * (1 - a), 1.0)


def contrast(one: tuple[float, float, float, float], other: tuple[float, float, float, float]) -> float:
    """The WCAG contrast ratio of two opaque colours."""
    def luminance(colour: tuple[float, float, float, float]) -> float:
        channels = [c / 255 for c in colour[:3]]
        lin = [c / 12.92 if c <= 0.03928 else ((c + 0.055) / 1.055) ** 2.4 for c in channels]
        return 0.2126 * lin[0] + 0.7152 * lin[1] + 0.0722 * lin[2]
    high, low = sorted((luminance(one), luminance(other)), reverse=True)
    return (high + 0.05) / (low + 0.05)


def ring_contrast(row: Json) -> float:
    """How a row's ring stands against the surface it is drawn on, the row's own opacity included."""
    page = rgba(row["page"])
    surface = over(rgba(row["background"]), page)
    alpha = number(row["opacity"])
    return contrast(over(rgba(row["outlineColor"]), page, alpha), over(surface, page, alpha))


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
        default=Path(__file__).resolve().parents[1] / "web",
        help="the page assets to serve (default: this checkout's vibe-talk/web)",
    )
    parser.add_argument("--screenshots", type=Path, default=None,
                        help="a directory to keep one PNG per step in (default: none kept)")
    return parser.parse_args()


class Walk:
    """One page on the channel, with the keys and the measurements every step takes."""

    def __init__(self, page: Page, shots: Path | None, label: str) -> None:
        self.page = page
        self.shots = shots
        self.label = label
        self.step = 0

    def row(self, row_id: str) -> Json:
        value = self.page.evaluate(ROW_JS, row_id)
        check(isinstance(value, dict), f"the row {row_id} answered {value!r}")
        assert isinstance(value, dict)
        return value

    def selected(self) -> str:
        value = self.page.evaluate(SELECTED_JS)
        check(isinstance(value, str), "no message is selected")
        assert isinstance(value, str)
        return value

    def key(self, key: str) -> None:
        self.page.keyboard.press(key)

    def shot(self, name: str) -> None:
        self.step += 1
        if self.shots is not None:
            self.page.screenshot(path=str(self.shots / f"{self.label}-{self.step:02d}-{name}.png"))


def open_channel(browser: Browser, api: FakeApi, *, reduced: bool = False, scheme: Scheme = "dark",
                 width: int = 1280, height: int = 800, touch: bool = False) -> tuple[Page, list[str]]:
    context = browser.new_context(viewport={"width": width, "height": height}, color_scheme=scheme,
                                  reduced_motion="reduce" if reduced else "no-preference",
                                  is_mobile=touch, has_touch=touch,
                                  device_scale_factor=2.625 if touch else 1)
    errors: list[str] = []
    context.route("**/*", api.route)
    page = context.new_page()
    page.on("pageerror", lambda error: errors.append(str(error)))
    page.goto(f"{ORIGIN}/voice", wait_until="load")
    page.fill("#api-token", TOKEN)
    page.click("#save-token")
    page.wait_for_selector("#view-switch", state="visible", timeout=10_000)
    page.click("#view-switch")
    page.wait_for_selector(f'#discord-log > li[data-id="{api.messages[-1]["id"]}"]', timeout=10_000)
    if not touch:
        page.mouse.move(1, 1)  # off every row, so the keys alone choose what is selected
    page.evaluate("() => document.getElementById('scroll-area').focus()")
    return page, errors


def marking(walk: Walk) -> str:
    """Select a message from the keyboard, see it ringed in the accent, press e. Answers its id."""
    walk.key("ArrowDown")
    walk.page.wait_for_timeout(100)
    marked = walk.selected()
    before = walk.row(marked)
    check(before["archived"] == "false", f"{marked} was read before e: {before}")
    check(before["outlineStyle"] == "solid" and before["outlineColor"] == before["accent"],
          f"a selected unread message is not ringed solid in the accent: {before}")
    check(before["background"] == before["lit"], f"a selected message is not on the lit surface: {before}")
    walk.key("e")
    return marked


def walk_motion(walk: Walk, scheme: Scheme) -> list[str]:
    page = walk.page
    notes: list[str] = []
    marked = marking(walk)
    now = walk.row(marked)
    check(walk.selected() != marked, "e did not move the selection on to the next unread message")
    check(now["archived"] == "true", f"e did not mark {marked} Done: {now}")
    check(now["flash"] == "marking", f"the row e marked carries nothing of the moment: {now}")
    check(now["animations"] == ["done-marking"],
          f"the row just marked is not animating from the look it had: {now['animations']}")
    walk.shot(f"{scheme}-marking")
    page.wait_for_timeout(450)
    held = walk.row(marked)
    check(held["flash"] == "marking", f"the grey ring did not hold: {held}")
    check(held["outlineStyle"] == "dashed" and held["outlineColor"] == held["grey"],
          f"the row just marked is not ringed in dashed grey: {held}")
    check(number(held["opacity"]) < 0.6 and "grayscale" in str(held["filter"]),
          f"the row just marked did not take the read look: {held}")
    page.wait_for_timeout(1300)
    after = walk.row(marked)
    check(after["flash"] is None, f"the moment never ended: {after}")
    check(after["outlineStyle"] == "none" or after["outlineColor"] in ("transparent", "rgba(0, 0, 0, 0)"),
          f"the ring outstayed the moment: {after}")
    walk.shot(f"{scheme}-after")

    # Back onto it: selected AND read is ringed in dashed grey, on the lit surface, at full strength.
    unread = walk.row(walk.selected())
    walk.key("ArrowUp")
    page.wait_for_timeout(100)
    check(walk.selected() == marked, f"ArrowUp selected {walk.selected()}, not {marked}")
    lit = walk.row(marked)
    check(lit["outlineStyle"] == "dashed" and lit["outlineColor"] == lit["grey"],
          f"a selected read message is not ringed in dashed grey: {lit}")
    check(unread["outlineStyle"] == "solid", f"a selected unread message is not ringed solid: {unread}")
    check(lit["outlineColor"] != unread["outlineColor"],
          f"selected read and selected unread are ringed alike: {lit} / {unread}")
    check(lit["background"] == lit["lit"] == unread["background"],
          f"a selected read message lost the lit surface: {lit}")
    check(lit["opacity"] == 1 and lit["filter"] == "none", f"a selected read message is not readable: {lit}")
    check(lit["animations"] == [], f"a plain selection animates, so the ring lags the keys: {lit['animations']}")
    # Each ring stands against the lit surface, in this scheme: a ring under 3:1 is not seen at all.
    read_ratio, unread_ratio = ring_contrast(lit), ring_contrast(unread)
    check(read_ratio >= 3 and unread_ratio >= 3,
          f"in {scheme} the read ring is {read_ratio:.2f}:1 and the unread ring {unread_ratio:.2f}:1 on the lit surface")
    notes.append(f"{scheme}: read ring dashed {read_ratio:.2f}:1, unread ring solid {unread_ratio:.2f}:1")
    walk.shot(f"{scheme}-selected-read")

    # The way back names the message, and takes it.
    label = page.text_content("#undo-dismiss-label") or ""
    check(page.is_visible("#undo-dismiss") and label.startswith("Undo “Step "),
          f"the Undo chip does not name the message: {label!r}")
    page.click("#undo-dismiss")
    page.wait_for_timeout(200)
    check(walk.row(marked)["archived"] == "false", "the Undo chip put nothing back")

    # From review: an Undo inside the hold left the grey ring on a row that was unread again.
    again = marking(walk)
    page.wait_for_timeout(150)
    check(walk.row(again)["flash"] == "marking", "the second Done drew nothing to take back")
    page.click("#undo-dismiss")
    page.wait_for_timeout(150)
    back = walk.row(again)
    check(back["archived"] == "false", f"the Undo inside the hold put nothing back: {back}")
    check(back["flash"] is None, f"a row put back inside the hold still carries the moment: {back}")
    check(back["outlineStyle"] == "none" or back["outlineColor"] in ("transparent", "rgba(0, 0, 0, 0)")
          or back["selected"] == "true", f"a row put back inside the hold still wears a ring: {back}")
    page.wait_for_timeout(1300)
    check(walk.row(again)["flash"] is None, "the timers drew the moment again on a row put back")
    walk.shot(f"{scheme}-undone-in-hold")
    return notes


def walk_reduced(walk: Walk) -> None:
    marked = marking(walk)
    now = walk.row(marked)
    check(now["flash"] == "marking", f"the row e marked carries nothing of the moment: {now}")
    check(now["animations"] == [], f"an animation runs though reduced motion was asked for: {now['animations']}")
    check(now["outlineColor"] == now["grey"] and number(now["opacity"]) < 0.6,
          f"under reduced motion the grey ring and the read look are not there at once: {now}")
    walk.shot("reduced")


def walk_pictures(walk: Walk, scheme: Scheme, size: str) -> str:
    """The moment of marking and a selected read message, as the eye is left with them."""
    marked = marking(walk)
    walk.page.wait_for_timeout(300)
    moment, nxt = walk.row(marked), walk.row(walk.selected())
    check(moment["outlineStyle"] == "dashed" and nxt["outlineStyle"] == "solid",
          f"{scheme} {size}: the moment and the next message are not told apart by the ring: {moment} / {nxt}")
    # From review: in these captures the Undo chip stood on Send.
    width = int(size.split("x")[0])
    clear_of_composer(chip_layout(walk.page, width), f"{scheme} {size} after e")
    walk.shot(f"{scheme}-{size}-moment")
    walk.page.wait_for_timeout(1300)
    walk.key("ArrowUp")
    walk.page.wait_for_timeout(150)
    lit = walk.row(walk.selected())
    check(lit["archived"] == "true" and lit["outlineStyle"] == "dashed",
          f"{scheme} {size}: the selected read message is not ringed dashed: {lit}")
    ratio = ring_contrast(lit)
    check(ratio >= 3, f"{scheme} {size}: the read ring is {ratio:.2f}:1 on the lit surface")
    walk.shot(f"{scheme}-{size}-selected-read")
    return f"{scheme} {size}: read ring {ratio:.2f}:1, the moment's ring {ring_contrast(moment):.2f}:1 at the read strength"


def chip_layout(page: Page, width: int) -> Json:
    laid: object = page.evaluate(CHIPS_JS)
    check(isinstance(laid, dict), f"the chips answered {laid!r}")
    assert isinstance(laid, dict)
    return laid


def chip_lines(laid: Json, width: int) -> int:
    """How many lines the row of chips takes, checking that no chip overlaps another or the screen's edge."""
    chips = laid["chips"]
    assert isinstance(chips, list)
    for i, one in enumerate(chips):
        check(number(one["left"]) >= 0 and number(one["right"]) <= width, f"at {width}px a chip runs off the screen: {one}")
        for other in chips[i + 1:]:
            apart = (number(one["right"]) <= number(other["left"]) or number(other["right"]) <= number(one["left"])
                     or number(one["bottom"]) <= number(other["top"]) or number(other["bottom"]) <= number(one["top"]))
            check(apart, f"at {width}px two chips overlap: {one} / {other}")
    bottoms = sorted(number(chip["bottom"]) for chip in chips)
    return 1 + sum(1 for a, b in zip(bottoms, bottoms[1:]) if b - a >= 2)


def meets(one: object, other: object) -> bool:
    """Whether two boxes overlap by more than half a pixel."""
    assert isinstance(one, dict) and isinstance(other, dict)
    return (number(one["left"]) < number(other["right"]) - 0.5 and number(other["left"]) < number(one["right"]) - 0.5
            and number(one["top"]) < number(other["bottom"]) - 0.5 and number(other["top"]) < number(one["bottom"]) - 0.5)


def clear_of_composer(laid: Json, where: str) -> bool:
    """No chip and no status pill over the message box or its Send. Answers whether Send was on screen."""
    chips = laid["chips"]
    assert isinstance(chips, list)
    floating = [*chips, *([{"id": "status-line", **laid["status"]}] if isinstance(laid["status"], dict) else [])]
    for target in ("send", "field"):
        box = laid[target]
        if box is None:
            continue
        for one in floating:
            check(not meets(one, box), f"{where}: {one['id']} covers the composer's {target}: {one} / {box}")
    return laid["send"] is not None


def walk_up_from_end(walk: Walk, width: int) -> int:
    """From the end of the list up through the whole message box, a few pixels at a time: nothing floats over it."""
    page = walk.page
    page.evaluate("() => { const a = document.getElementById('scroll-area'); a.scrollTop = a.scrollHeight; }")
    page.wait_for_timeout(300)
    end = chip_layout(page, width)
    composer = end["composer"]
    check(isinstance(composer, dict), f"at {width}px the message box is not on screen at the end of the list")
    assert isinstance(composer, dict)
    span = int(number(composer["bottom"]) - number(composer["top"])) + 120
    seen = 0
    for offset in range(0, span, 6):
        page.evaluate("""(offset) => { const a = document.getElementById('scroll-area');
          a.scrollTop = a.scrollHeight - a.clientHeight - offset; }""", offset)
        page.evaluate("() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done)))")
        laid = chip_layout(page, width)
        if clear_of_composer(laid, f"at {width}px, {offset}px up from the end"):
            seen += 1
        if offset == 48:
            walk.shot(f"chips-{width}-rising")
    check(seen >= 10, f"at {width}px Send was on screen at only {seen} of the positions walked")
    return seen


def walk_chip(walk: Walk, width: int) -> str:
    """The Undo chip beside Newest, Summaries and Expand all: plain on a phone, named on a desk, never over Send."""
    page = walk.page
    page.evaluate("() => { const a = document.getElementById('scroll-area'); a.scrollTop = a.scrollHeight - 2.2 * a.clientHeight; }")
    page.wait_for_selector("#jump-newest", state="visible", timeout=5_000)
    row = page.evaluate("""() => { const area = document.getElementById('scroll-area').getBoundingClientRect();
      const row = [...document.querySelectorAll('#discord-log > li[data-archived="false"]')]
        .find((li) => { const r = li.getBoundingClientRect(); return r.top > area.top + 40 && r.bottom < area.bottom - 80; });
      return row ? row.getAttribute('data-id') : null; }""")
    check(isinstance(row, str), f"at {width}px no unread message is on screen to mark Done")
    assert isinstance(row, str)
    page.locator(f'#discord-log > li[data-id="{row}"] .done-button').click()
    page.wait_for_selector("#undo-dismiss", state="visible", timeout=5_000)
    page.wait_for_timeout(300)
    laid = chip_layout(page, width)
    chips = laid["chips"]
    assert isinstance(chips, list)
    ids = [str(chip["id"]) for chip in chips]
    check({"jump-newest", "undo-dismiss", "expand-all"} <= set(ids), f"at {width}px the chips up are {ids}")
    named = chip_lines(laid, width)
    name = laid["name"]
    label, title, aria = str(laid["label"]), str(laid["title"]), str(laid["aria"])
    check(label.startswith(f"Undo “Backup check {int(row[-4:])} finished"), f"the label is {label!r}")
    check(title == label and aria == label, f"the tooltip {title!r} or the accessible name {aria!r} is not the whole label {label!r}")
    phone = width < 480
    if phone:
        # From review: the name gave way to one letter here. A phone's row says only "Undo".
        check(laid["drawn"] == "Undo" and name is None, f"at {width}px the Undo chip draws {laid['drawn']!r}, not a plain Undo")
    else:
        check(isinstance(name, dict) and str(laid["drawn"]).startswith("Undo “ Backup check"),
              f"at {width}px the Undo chip does not name the message: {laid['drawn']!r}")
        assert isinstance(name, dict)
        # A dozen characters of the name at least: 6em of the chip's type is about that many.
        check(number(name["client"]) >= 6 * 12.8 - 1 or number(name["scroll"]) <= number(name["client"]) + 1,
              f"at {width}px the name gives way to {name['client']}px of {name['scroll']}px")
        cut = number(name["scroll"]) > number(name["client"]) + 1
        check(not cut or name["overflow"] == "ellipsis", f"at {width}px the name is cut short without an ellipsis: {name}")
    check(number(laid["spill"]) <= 1, f"at {width}px the chip's words spill out of it: {laid}")
    undo = next(chip for chip in chips if chip["id"] == "undo-dismiss")
    check(number(laid["labelRight"]) <= number(undo["right"]) + 0.5, f"at {width}px the words run past the chip: {laid}")
    walk.shot(f"chips-{width}")
    # The same row with a bare "Undo", as it was before the chip named anything: the name takes no line
    # that did not.
    page.evaluate("() => { const l = document.getElementById('undo-dismiss-label'); l.replaceChildren(); l.textContent = 'Undo'; }")
    page.wait_for_timeout(100)
    bare = chip_lines(chip_layout(page, width), width)
    page.evaluate("() => renderTodoControls()")
    page.wait_for_timeout(100)
    check(named <= bare, f"at {width}px the Undo chip wraps the row onto {named} lines, a bare Undo onto {bare}")
    if width >= 412:
        check(named == 1, f"at {width}px Newest, the Undo chip and the rest do not stand on one line: {chips}")
    # Walked up from the end of the list through the message box: nothing floats over it or its Send.
    seen = walk_up_from_end(walk, width)
    # At the very end Newest goes, and the chips stand in the room above the box, clear of the last row.
    page.evaluate("() => { const a = document.getElementById('scroll-area'); a.scrollTop = a.scrollHeight; }")
    page.wait_for_timeout(400)
    end = chip_layout(page, width)
    end_chips = end["chips"]
    assert isinstance(end_chips, list)
    end_ids = [str(chip["id"]) for chip in end_chips]
    check("undo-dismiss" in end_ids and "jump-newest" not in end_ids, f"at the end the chips up are {end_ids}")
    check(chip_lines(end, width) == 1, f"at {width}px the chips at the end of the list wrapped: {end_chips}")
    check(clear_of_composer(end, f"at {width}px at the end"), f"at {width}px the composer's Send is not on screen at the end")
    composer, last = end["composer"], end["lastRow"]
    assert isinstance(composer, dict) and isinstance(last, dict)
    for chip in end_chips:
        check(number(chip["bottom"]) <= number(composer["top"]) + 0.5, f"at {width}px a chip stands below the box's top: {chip}")
        check(not meets(chip, last), f"at {width}px at the end a chip covers the last message: {chip} / {last}")
    walk.shot(f"chips-{width}-end")
    words = "a plain Undo" if phone else "the Undo chip named"
    return (f"{width}px: {' · '.join(ids)} on {named} line{'s' if named > 1 else ''} (a bare Undo: {bare}), {words};"
            f" walked up from the end, Send clear at all {seen} positions it was on screen; at the end"
            f" {' · '.join(end_ids)} on one line in the room above the box")


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
    web_root = args.web_root.resolve()
    notes: list[str] = []
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(headless=True, executable_path=args.browser_executable)
        try:
            schemes: tuple[Scheme, ...] = ("dark", "light")
            for scheme in schemes:
                page, errors = open_channel(browser, FakeApi(web_root), scheme=scheme)
                notes += walk_motion(Walk(page, args.screenshots, f"motion-{scheme}"), scheme)
                check(not errors, f"the page threw: {errors}")
                page.context.close()
            page, errors = open_channel(browser, FakeApi(web_root), reduced=True)
            walk_reduced(Walk(page, args.screenshots, "reduced"))
            check(not errors, f"the page threw: {errors}")
            page.context.close()
            for scheme in schemes:
                for width, height, touch in ((412, 915, True), (1280, 800, False)):
                    size = f"{width}x{height}"
                    page, errors = open_channel(browser, FakeApi(web_root), reduced=True, scheme=scheme,
                                                width=width, height=height, touch=touch)
                    notes.append(walk_pictures(Walk(page, args.screenshots, f"done-{scheme}-{size}"), scheme, size))
                    check(not errors, f"the page threw: {errors}")
                    page.context.close()
            for width, height in ((360, 800), (412, 915), (1280, 800)):
                page, errors = open_channel(browser, FakeApi(web_root, LONG_MESSAGES), width=width, height=height,
                                            touch=width < 480)
                notes.append(walk_chip(Walk(page, args.screenshots, f"undo-chip-{width}"), width))
                check(not errors, f"the page threw: {errors}")
                page.context.close()
            version = browser.version
        finally:
            browser.close()
    print(f"Chromium {version}: a selected unread message ringed solid in the accent and a selected read one"
          " dashed in grey on the same lit surface, in both schemes; e drew the moment on the row it left (the"
          " done-marking animation, the dashed grey ring at the read strength, then neither); the Undo chip named"
          " the message and put it back, and an Undo inside the hold took the ring away at once; under reduced"
          " motion the same look arrived with no animation; the Undo chip was a plain Undo on the phones and"
          " named on the desk, on one line with Newest and Expand all, and no chip or status pill ever stood on"
          " the message box or its Send, walked up from the end of the list.")
    for note in notes:
        print(f"  {note}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
