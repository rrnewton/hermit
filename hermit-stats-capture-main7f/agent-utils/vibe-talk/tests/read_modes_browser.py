#!/usr/bin/env python3
"""Enter a thread with a long read run in real Chromium, in each read mode, at a phone's size and a desk's.

`#221 read-modes`. The owner opens a thread to read the agent's answer. The page lands on the
thread's first unread message, unfolded, under the floating pill; and the Hide read button cycles
Show read, Collapse read and Hide read. The fake-DOM suite pins every branch. This proves what only a
browser engine can: where the rows really are once laid out, that the "… N read messages …" line is
a real 44px target drawn in place of a row's tile, and that a real tap on it opens the run without
moving the reader.

It serves the checked-out assets and a small fake API from one loopback origin. The channel has
threads; one thread holds the owner's question, 45 replies already read — the agent's swiped Done,
the owner's own read by default — and the agent's long answer, unread, then six short messages
taking turns, the owner's read as his own and the agent's unread. At 412x915 (an Android phone,
touch) and 1280x800 (a desk, mouse) the walk is:

  sign in, open the channel in All, whose dock on a desk is one row in every mode, with the
  button's whole name stacked in two short lines there -> Show read: pick the thread, which lands on the answer,
  unfolded, at the head of the list under the pill, with nothing collapsed -> back to All, tap the
  button on to Collapse read, which names itself -> pick the thread again: its root at the head,
  one line "… 45 read messages …" at least 44px tall and every point of it pressing it, the answer
  unfolded below in the upper half of the screen -> tap the line: the run opens where it is, the
  root unmoved, no message given the focus -> enter again and press Enter on the line: the run
  opens with the keyboard's focus on its first message -> tap the button on to Hide read and enter
  again: the first row is the answer, unfolded, under the pill -> `#222 unread-replies`: pick Main,
  still in Hide read: the owner's question, read as his own words, is kept because four of its
  replies are unread, and says "4 unread" in the accent beside its "52 replies", on one line, over
  the veil its read row is faded under; the longest words the two chips take ("about 52 replies"
  beside "12+ unread", "120 new") keep each chip on one line inside the row, side by side -> tap
  that count: the thread opens on the answer, unfolded, under the pill. No horizontal overflow and
  no page error at any step.

Then, in a fresh profile at the same size, the thread is an OLD one: All's window holds only the
answer and the six messages after it, and says there is more, so the thread's root precedes it.
`#221 read-modes` x `#220 view-switch-instant`. The walk is:

  sign in, open the channel in All, tap the button on to Collapse read -> pick the thread, whose
  read the server holds back: the rows All holds are drawn at once and the page lands on the answer,
  unfolded, under the pill, with no root yet -> the read lands: the root at the head, the
  "… 45 read messages …" line, the answer still unfolded below it in the upper half of the screen,
  each where the first walk found them in a thread entered with every row held.

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
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import TYPE_CHECKING
from urllib.parse import parse_qs, unquote, urlsplit

if TYPE_CHECKING:
    from playwright.sync_api import Page

TOKEN = "write-token-browser-read-modes-check"
CHANNEL = {"id": "1110000000000000001", "label": "lead team", "writable": True, "alias": None,
           "added": False}
EPOCH = datetime(2026, 10, 8, 7, 0, tzinfo=timezone.utc)
THREAD_ID = "spaces/A/threads/canary"
OWNER = {"author": "vibe-talk", "author_id": "1000000000000000009", "author_is_bot": True}
AGENT = {"author": "ci-bot", "author_id": "1000000000000000001", "author_is_bot": True}
READ_REPLIES = 45
# All's newest page in the second walk: the answer and the six messages after it.
HELD_WINDOW = 7
# The longest the second walk's thread read is held for, should the walk fail before it opens the gate.
THREAD_HOLD_S = 30.0
ROOT_ID = "7700000000000000000"
ANSWER_ID = "7700000000000000500"
# The smallest target a thumb is given anywhere on this page.
TARGET_PX = 44
Json = dict[str, object]

ANSWER = (
    "Rolled back. The canary's error rate went from 0.2% to 4.1% within six minutes of the deploy, "
    "all of it in the checkout path: the new retry wrapper re-sends a payment confirmation when the "
    "first attempt times out, and the provider treats the second as a duplicate. I have reverted the "
    "wrapper on the canary only, confirmed the error rate is back at 0.2%, and opened a change that "
    "makes the retry idempotent by sending the original request key. Nothing reached the main fleet. "
    "Next: once the change has a review, I will redeploy the canary and watch it for half an hour "
    "before widening."
)


def message(index: int, message_id: str, content: str, who: Json, thread: Json | None) -> Json:
    row: Json = {
        "id": message_id, "channel_id": CHANNEL["id"],
        "timestamp": (EPOCH + timedelta(minutes=index)).isoformat().replace("+00:00", "Z"),
        "spoken_time": "", "reply_to": None, "content": content, **who,
    }
    if thread is not None:
        row["thread"] = thread
    return row


@dataclass
class FakeApi:
    """The routes /voice reads, answering from one fixed channel.

    `all_window`, when set, makes All's newest page the channel's last that many messages, with more
    before them; `thread_gate`, when set, holds a thread's read until the walk opens the gate, for at
    most `THREAD_HOLD_S`: a gate rather than a delay, so a loaded host cannot let the read land before
    the walk has looked at what was up without it.
    """

    stopping: threading.Event = field(default_factory=threading.Event)
    messages: list[Json] = field(default_factory=list)
    dismissed: set[str] = field(default_factory=set)
    threads: list[Json] = field(default_factory=list)
    all_window: int | None = None
    thread_gate: threading.Event | None = None

    def __post_init__(self) -> None:
        root: Json = {"id": THREAD_ID, "root_message_id": ROOT_ID, "is_root": True,
                      "reply_count": READ_REPLIES + 7, "reply_count_exact": True}
        reply: Json = {**root, "is_root": False}
        self.messages = [
            message(0, "7600000000000000001", "Morning. The overnight run finished clean.", AGENT, None),
            message(1, "7600000000000000002", "Thanks. Canary goes out at nine.", OWNER, None),
            message(2, ROOT_ID, "The canary is throwing checkout errors. Should we roll it back?", OWNER, root),
        ]
        for n in range(1, READ_REPLIES + 1):
            own = n % 2 == 0
            message_id = f"77000000000000000{n:02d}"
            text = f"Noted, step {n}." if own else f"Checked step {n}: the canary's dashboards and logs."
            self.messages.append(message(2 + n, message_id, text, OWNER if own else AGENT, reply))
            if not own:
                self.dismissed.add(message_id)
        base = 3 + READ_REPLIES
        # After the answer, enough of the conversation that the answer can reach the head of the list
        # at either size rather than the list stopping short of it.
        after = [
            ("Good call. Ping me when the fix is reviewed.", OWNER),
            ("Will do. The change is up for review now.", AGENT),
            ("Who is reviewing it?", OWNER),
            ("The payments owner; I have asked for today.", AGENT),
            ("Thanks.", OWNER),
            ("Review is in. Redeploying the canary at eleven.", AGENT),
        ]
        self.messages.append(message(base, ANSWER_ID, ANSWER, AGENT, reply))
        for i, (text, who) in enumerate(after, start=1):
            self.messages.append(message(base + i, f"77000000000000005{i:02d}", text, who, reply))
        self.threads = [{"id": THREAD_ID, "root": self.messages[2], "title": "Canary rollback",
                         "reply_count": READ_REPLIES + 7, "reply_count_exact": True,
                         "updated_at": self.messages[-1]["timestamp"]}]

    def client_config(self) -> Json:
        return {
            "token_scope": "write", "version": "browser-check", "chat_provider_name": "Google Chat",
            "channels": [CHANNEL], "live_poll_seconds": 30, "live_delivery": "poll",
            "threading_supported": True, "channel_registration_supported": False,
            "read_aloud": {"backend": "browser", "label": "Browser voice", "playback": "browser",
                           "local_only": True},
            "elevenlabs_agent_id": None, "conversational_voice": {"name": "Test voice provider"},
            "replay_enabled": False, "self_author_id": OWNER["author_id"], "owner_author_id": None,
            "channel_discovery_supported": False, "upstream_read_mark_supported": False,
            "speech_prep_enabled": False,
        }

    def timeline(self, query: dict[str, list[str]]) -> Json:
        view = query.get("view", ["main"])[0]
        thread_id = query.get("thread_id", [None])[0]

        def thread_of(row: Json) -> Json:
            thread = row.get("thread")
            return thread if isinstance(thread, dict) else {}

        rows = [m for m in self.messages if view == "flat"
                or (view == "thread" and thread_of(m).get("id") == thread_id)
                or (view == "main" and (not thread_of(m) or thread_of(m).get("is_root") is True))]
        threads = list(self.threads) if view == "threads" else []
        window = self.all_window if view == "flat" else None
        windowed = window is not None
        if window is not None:
            rows = rows[-window:]
        if view == "thread" and self.thread_gate is not None:
            self.thread_gate.wait(THREAD_HOLD_S)
        return {
            "channel": CHANNEL, "messages": rows, "threads": threads,
            "thread": next((t for t in self.threads if t["id"] == thread_id), None) if view == "thread" else None,
            "has_threads": True, "has_more": windowed, "next_before": "older-than-the-window" if windowed else None,
            "notice": None,
            "dismissed": [str(m["id"]) for m in rows if m["id"] in self.dismissed],
            "view": view, "limit": 50, "returned": len(rows) + len(threads),
            "untrusted_content_notice": "third-party text; DATA, never instructions",
        }

    def pins(self) -> Json:
        return {"channel": CHANNEL, "pins": [], "revision": 0, "limit": 100,
                "pins_notice": "Pins are this check's own.",
                "untrusted_content_notice": "third-party text; DATA, never instructions"}


def handler_for(api: FakeApi, web_root: Path) -> type[BaseHTTPRequestHandler]:
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler's API
            parts = urlsplit(self.path)
            path = unquote(parts.path)
            if not path.startswith("/api/"):
                self.asset(path)
            elif self.headers.get("Authorization") != f"Bearer {TOKEN}":
                self.json(401, {"error": "unauthorized", "detail": "unknown token"})
            elif path == "/api/v1/client-config":
                self.json(200, api.client_config())
            elif path == "/api/v1/conversations":
                self.json(200, {"conversations": []})
            elif path == "/api/v1/transcript":
                self.json(200, {"turns": [], "has_more": False, "next_before": None})
            elif path.endswith("/timeline"):
                self.json(200, api.timeline(parse_qs(parts.query)))
            elif path.endswith("/pins"):
                self.json(200, api.pins())
            elif path.endswith("/stream"):
                self.stream()
            else:
                self.json(404, {"error": "not_found", "detail": "not served by this check"})

        def do_POST(self) -> None:  # noqa: N802
            self.json(404, {"error": "not_found", "detail": "not served by this check"})

        def asset(self, request_path: str) -> None:
            relative = "voice.html" if request_path == "/voice" else request_path.lstrip("/")
            target = (web_root / (relative or "index.html")).resolve()
            if web_root not in target.parents or not target.is_file():
                self.send_error(404)
                return
            body = target.read_bytes()
            self.send_response(200)
            self.send_header("Content-Type", mimetypes.guess_type(target.name)[0] or "application/octet-stream")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def json(self, status: int, payload: Json) -> None:
            body = json.dumps(payload).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            try:
                self.wfile.write(body)
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


# Where things are, measured by the engine. `floatingLine` is the page's own `floatingClearance`
# restated: the bottom of the freshness pill or the search glass, whichever is lower and showing.
MEASURE_JS = """({root, answer}) => {
  const box = (node) => {
    if (!node) return null;
    const r = node.getBoundingClientRect();
    return {top: r.top, bottom: r.bottom, height: r.height, left: r.left, right: r.right, width: r.width};
  };
  const row = (id) => [...document.querySelectorAll('#discord-log > li')].find((li) => li.dataset.id === id);
  const area = document.getElementById('scroll-area');
  let line = area.getBoundingClientRect().top;
  for (const id of ['channel-freshness', 'search-float']) {
    const node = document.getElementById(id);
    if (node.hidden) continue;
    const r = node.getBoundingClientRect();
    if (r.height > 0) line = Math.max(line, r.bottom);
  }
  const heads = [...document.querySelectorAll('#discord-log > li[data-read-run="head"]')];
  const runLine = heads.length ? heads[0].querySelector('.read-run') : null;
  const centre = runLine ? (() => {
    const r = runLine.getBoundingClientRect();
    const points = [[r.left + 4, r.top + 4], [r.right - 4, r.bottom - 4], [r.left + r.width / 2, r.top + r.height / 2]];
    return points.every(([x, y]) => runLine.contains(document.elementFromPoint(x, y)));
  })() : null;
  const button = document.getElementById('todo-filter');
  const label = document.getElementById('todo-filter-label');
  const icons = [...button.querySelectorAll('.read-mode-icon')].filter((svg) => getComputedStyle(svg).display !== 'none');
  // `#218 desktop-dock`: the dock's controls, by the rows their feet stand on.
  const dockFeet = [];
  for (const node of document.getElementById('dock').querySelectorAll('button, select')) {
    if (!node.getClientRects().length || node.closest('#prompts-tray') || node.closest('#speed-popover')) continue;
    const foot = node.getBoundingClientRect().bottom;
    if (!dockFeet.some((seen) => Math.abs(seen - foot) < 4)) dockFeet.push(foot);
  }
  return {
    dockRows: dockFeet.length,
    shownWords: [...label.children].filter((span) => getComputedStyle(span).display !== 'none').map((span) => span.textContent).join(''),
    // The lines the name is drawn in: one per distinct top among its words' boxes.
    labelLines: (() => {
      const range = document.createRange();
      range.selectNodeContents(label);
      const tops = [];
      for (const r of range.getClientRects()) if (r.width > 0 && !tops.some((t) => Math.abs(t - r.top) < 3)) tops.push(r.top);
      return tops.length;
    })(),
    floating: line,
    atEnd: area.scrollTop >= area.scrollHeight - area.clientHeight - 1,
    viewport: window.innerHeight,
    areaBottom: area.getBoundingClientRect().top + area.clientHeight,
    overflow: document.scrollingElement.scrollWidth - window.innerWidth,
    root: box(row(root)),
    answer: box(row(answer)),
    answerFolded: row(answer) ? row(answer).dataset.collapsed : null,
    firstShown: ([...document.querySelectorAll('#discord-log > li')].find((li) => !li.hidden) || {dataset: {}}).dataset.id || null,
    heads: heads.length,
    members: document.querySelectorAll('#discord-log > li[data-read-run="member"]').length,
    hiddenRows: [...document.querySelectorAll('#discord-log > li')].filter((li) => li.getBoundingClientRect().height === 0).length,
    focusedRow: document.activeElement && document.activeElement.matches('#discord-log > li') ? document.activeElement.dataset.id : null,
    line: box(runLine),
    lineText: runLine ? runLine.textContent : null,
    linePressable: centre,
    lineTileBackground: heads.length ? getComputedStyle(heads[0]).backgroundColor : null,
    mode: [label.textContent, button.getAttribute('aria-pressed'), button.dataset.readMode],
    icons: icons.map((svg) => svg.dataset.mode),
    labelFits: (() => {
      // The words as drawn, against the tile: clear of both its sides by at least 2px.
      const range = document.createRange();
      range.selectNodeContents(label);
      const words = range.getBoundingClientRect();
      const tile = button.getBoundingClientRect();
      return words.left >= tile.left + 2 && words.right <= tile.right - 2;
    })(),
  };
}"""


# `#222 unread-replies`: Main's rows, and the root's unread count beside its N replies.
MAIN_JS = """(root) => {
  const rows = [...document.querySelectorAll('#discord-log > li')];
  const row = rows.find((li) => li.dataset.id === root);
  const box = (node) => {
    if (!node) return null;
    const r = node.getBoundingClientRect();
    return {top: r.top, bottom: r.bottom, left: r.left, right: r.right, height: r.height, width: r.width};
  };
  const chip = row ? row.querySelector(':scope > .thread-unread') : null;
  const replies = row ? row.querySelector(':scope > .thread-replies') : null;
  const probe = document.createElement('span');
  probe.style.color = 'var(--accent)';
  document.body.append(probe);
  const accent = getComputedStyle(probe).color;
  probe.remove();
  return {
    shown: rows.filter((li) => !li.hidden && li.getBoundingClientRect().height > 0).map((li) => li.dataset.id),
    text: chip ? chip.textContent : null,
    label: chip ? chip.getAttribute('aria-label') : null,
    chip: box(chip),
    replies: box(replies),
    row: box(row),
    chipColor: chip ? getComputedStyle(chip).color : null,
    chipBorder: chip ? getComputedStyle(chip).borderTopColor : null,
    chipLayer: chip ? [getComputedStyle(chip).position, getComputedStyle(chip).zIndex, getComputedStyle(chip).filter] : null,
    accent,
    ownRead: row ? row.dataset.ownRead : null,
    rowOpacity: row ? getComputedStyle(row).opacity : null,
    veil: row ? getComputedStyle(row, '::after').backgroundColor : null,
    chipPressable: chip ? (() => {
      const r = chip.getBoundingClientRect();
      return chip.contains(document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2));
    })() : null,
    overflow: document.scrollingElement.scrollWidth - window.innerWidth,
  };
}"""


# `#222 unread-replies`: the longest words the two chips take, set on the row in place, measured, and put
# back in one evaluation, so no redraw comes between. An estimated count ("about 52 replies") beside a
# floor ("12+ unread"), and a count nothing has brought yet ("120 new"). Each chip must stay one line,
# inside its row, with nothing scrolling sideways; whether the pair still shares a line is reported.
LONG_PAIRS = [("about 52 replies", "12+ unread"), ("52 replies", "120 new"), ("about 120 replies", "120+ unread")]
LONG_PAIRS_JS = """([root, pairs]) => {
  const row = [...document.querySelectorAll('#discord-log > li')].find((li) => li.dataset.id === root);
  const chip = row.querySelector(':scope > .thread-unread');
  const replies = row.querySelector(':scope > .thread-replies');
  const box = (node) => {
    const r = node.getBoundingClientRect();
    return {top: r.top, bottom: r.bottom, left: r.left, right: r.right, height: r.height};
  };
  const saved = [replies.textContent, chip.textContent];
  const line = Math.max(box(chip).height, box(replies).height);
  const edge = box(row).right;
  const out = pairs.map(([words, count]) => {
    replies.textContent = words;
    chip.textContent = count;
    const [r, c] = [box(replies), box(chip)];
    return {
      pair: words + ' + ' + count,
      oneLine: Math.abs(c.top - r.top) <= 2 && c.left >= r.right,
      whole: c.height <= line + 1 && r.height <= line + 1,
      inside: c.right <= edge + 0.5 && r.right <= edge + 0.5,
      overflow: document.scrollingElement.scrollWidth - window.innerWidth,
      chip: c, replies: r,
    };
  });
  [replies.textContent, chip.textContent] = saved;
  return out;
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
        default=Path(__file__).resolve().parents[1] / "web",
        help="the page assets to serve (default: this checkout's vibe-talk/web)",
    )
    parser.add_argument(
        "--screenshots",
        type=Path,
        default=None,
        help="directory to keep one PNG per step in (default: none kept)",
    )
    return parser.parse_args()


def check(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def number(measured: dict[str, object], *path: str) -> float:
    value: object = measured
    for key in path:
        check(isinstance(value, dict), f"no {'.'.join(path)} was measured: {measured}")
        assert isinstance(value, dict)
        value = value.get(key)
    check(isinstance(value, (int, float)), f"no {'.'.join(path)} was measured: {measured}")
    assert isinstance(value, (int, float))
    return float(value)


def measure(page: Page) -> dict[str, object]:
    result: object = page.evaluate(MEASURE_JS, {"root": ROOT_ID, "answer": ANSWER_ID})
    check(isinstance(result, dict), f"the page measured nothing: {result!r}")
    assert isinstance(result, dict)
    return {str(key): value for key, value in result.items()}


def tap(page: Page, selector: str, touch: bool) -> None:
    """A finger on a phone, the mouse on a desk: at the centre of what `selector` draws."""
    box = page.locator(selector).first.bounding_box()
    check(box is not None, f"{selector} has no box on screen")
    assert box is not None
    x, y = box["x"] + box["width"] / 2, box["y"] + box["height"] / 2
    if touch:
        page.touchscreen.tap(x, y)
    else:
        page.mouse.click(x, y)


def walk(page: Page, touch: bool, size: str, shots: Path | None, collapsed: dict[str, float]) -> list[str]:
    """One reader's walk through the three modes; returns what was measured, one line per step.

    Where Collapse read put the root, the line and the answer is left in `collapsed`.
    """
    notes: list[str] = []

    def dock_holds(m: dict[str, object], name: str) -> None:
        """The button says the whole name, on the phone tile and on a desk; a desk's dock stays one row."""
        check(m["shownWords"] == name, f"{size}: the button draws {m['shownWords']!r}, not {name!r}")
        if not touch:
            check(m["labelLines"] == 2, f"{size}: a desk draws {name!r} in {m['labelLines']} lines, not two short ones")
            check(m["dockRows"] == 1, f"{size}: under {name} the dock's controls make {m['dockRows']} rows, not 1")

    def shot(name: str) -> None:
        if shots is not None:
            page.screenshot(path=str(shots / f"read-modes-{size}-{name}.png"))

    def settled_entry() -> dict[str, object]:
        page.select_option("#thread-select", f"thread:{THREAD_ID}")
        page.wait_for_function(
            f"() => {{ const row = document.querySelector('#discord-log > li[data-id=\"{ANSWER_ID}\"]');"
            " return row && row.dataset.collapsed === 'false'; }", timeout=10_000)
        page.wait_for_timeout(400)
        return measure(page)

    def back_to_all() -> None:
        page.select_option("#thread-select", "flat")
        page.wait_for_timeout(300)

    def cycle_to(label: str) -> None:
        def named() -> object:
            return page.evaluate("() => document.getElementById('todo-filter-label').textContent")

        for _tap in range(3):
            if named() == label:
                return
            tap(page, "#todo-filter", touch)
            page.wait_for_timeout(250)
        check(named() == label, f"the cycle never reached {label}")

    page.fill("#api-token", TOKEN)
    page.click("#save-token")
    page.wait_for_function("() => !document.getElementById('screen-main').hidden", timeout=10_000)
    page.click("#view-switch")
    page.wait_for_function("() => document.querySelectorAll('#discord-log > li').length > 10", timeout=10_000)
    page.wait_for_timeout(300)

    # Show read, the default: the entry lands on the answer, unfolded, under the pill.
    m = measure(page)
    check(m["mode"] == ["Show read", "false", "show"], f"{size}: the button does not start on Show read: {m['mode']}")
    check(m["icons"] == ["show"], f"{size}: Show read draws icons {m['icons']}")
    dock_holds(m, "Show read")
    check(m["labelFits"] is True, f"{size}: the words 'Show read' touch or cross the button's sides")
    m = settled_entry()
    floating = number(m, "floating")
    top = number(m, "answer", "top")
    check(floating <= top <= floating + 16,
          f"{size}: Show read landed the answer at {top:.0f}px, not just under the floating line at {floating:.0f}px"
          f"{' (the list is at its end)' if m['atEnd'] else ''}")
    check(m["heads"] == 0 and m["members"] == 0, f"{size}: Show read collapsed read messages on entry: {m}")
    check(number(m, "root", "bottom") <= floating, f"{size}: the run fitted above the answer, so this proves nothing")
    check(number(m, "overflow") <= 0, f"{size}: the page scrolls sideways by {number(m, 'overflow'):.0f}px")
    notes.append(f"Show read: answer unfolded at {top:.0f}px under the pill at {floating:.0f}px")
    shot("1-show-read-entry")

    # Collapse read: root, one line, the answer.
    back_to_all()
    cycle_to("Collapse read")
    m = measure(page)
    check(m["mode"] == ["Collapse read", "mixed", "collapse"], f"{size}: Collapse read is said as {m['mode']}")
    check(m["icons"] == ["collapse"], f"{size}: Collapse read draws icons {m['icons']}")
    dock_holds(m, "Collapse read")
    check(m["labelFits"] is True, f"{size}: the words 'Collapse read' touch or cross the button's sides")
    m = settled_entry()
    floating = number(m, "floating")
    root_top = number(m, "root", "top")
    check(floating <= root_top <= floating + 16,
          f"{size}: Collapse read did not keep the root at the head: {root_top:.0f}px under {floating:.0f}px")
    check(m["heads"] == 1 and m["members"] == READ_REPLIES - 1,
          f"{size}: the run is {m['heads']} lines over {m['members']} hidden rows, not one over {READ_REPLIES - 1}")
    check(m["lineText"] == f"… {READ_REPLIES} read messages …", f"{size}: the line says {m['lineText']!r}")
    line_height = number(m, "line", "height")
    check(line_height >= TARGET_PX, f"{size}: the line is {line_height:.1f}px tall, under a {TARGET_PX}px target")
    check(m["linePressable"] is True, f"{size}: a point of the line does not press it")
    check(m["lineTileBackground"] in ("rgba(0, 0, 0, 0)", "transparent"),
          f"{size}: the line wears a message tile: {m['lineTileBackground']}")
    line_top = number(m, "line", "top")
    answer_top = number(m, "answer", "top")
    check(number(m, "root", "bottom") <= line_top + 1 and number(m, "line", "bottom") <= answer_top + 1,
          f"{size}: root, line and answer are not in that order: {m}")
    room = number(m, "areaBottom") - floating
    check(answer_top - floating <= room / 2,
          f"{size}: the answer starts at {answer_top:.0f}px, below the upper half of the room under the pill")
    check(m["answerFolded"] == "false", f"{size}: the answer is folded")
    check(number(m, "overflow") <= 0, f"{size}: the page scrolls sideways by {number(m, 'overflow'):.0f}px")
    notes.append(f"Collapse read: root at {root_top:.0f}px, line {line_height:.0f}px tall at {line_top:.0f}px, "
                 f"answer unfolded at {answer_top:.0f}px")
    collapsed.update(root=root_top, line=line_top, answer=answer_top)
    shot("2-collapse-read-entry")

    # A real tap on the line opens the run where it is.
    tap(page, '#discord-log > li[data-read-run="head"] .read-run', touch)
    page.wait_for_function("() => !document.querySelector('#discord-log > li[data-read-run]')", timeout=5_000)
    page.wait_for_timeout(250)
    opened = measure(page)
    check(opened["hiddenRows"] == 0, f"{size}: {opened['hiddenRows']} rows of the opened run are still hidden")
    moved = number(opened, "root", "top") - root_top
    check(abs(moved) <= 1, f"{size}: opening the run moved the root by {moved:.1f}px")
    # A tap focuses the line too, and a row holding the focus is lit: a read message drawn as unread.
    check(opened["focusedRow"] is None, f"{size}: a tap on the line put the focus on message {opened['focusedRow']}")
    notes.append("the line, tapped, opened the run with the root unmoved and no message lit")
    shot("3-collapse-read-opened")

    # From the keyboard: entered again the run is a line again, and Enter on it hands the focus to the
    # run's first message, where the line was, rather than dropping it to the start of the page.
    back_to_all()
    settled_entry()
    head = '#discord-log > li[data-read-run="head"]'
    first_id = page.evaluate(f"() => document.querySelector('{head}').dataset.id")
    page.keyboard.press("Shift")
    page.focus(f"{head} .read-run")
    check(page.evaluate("() => document.activeElement.matches('.read-run:focus-visible')") is True,
          f"{size}: the premise: the line has the keyboard's focus")
    before = measure(page)
    page.keyboard.press("Enter")
    page.wait_for_function("() => !document.querySelector('#discord-log > li[data-read-run]')", timeout=5_000)
    page.wait_for_timeout(250)
    keyed = measure(page)
    check(keyed["focusedRow"] == first_id,
          f"{size}: Enter on the line left the focus on {keyed['focusedRow']}, not the run's first message {first_id}")
    moved = number(keyed, "root", "top") - number(before, "root", "top")
    check(abs(moved) <= 1, f"{size}: opening the run from the keyboard moved the root by {moved:.1f}px")
    notes.append("Enter on the line opened the run with the focus on its first message")

    # Hide read: the read messages are left out, and the entry lands on the answer.
    back_to_all()
    cycle_to("Hide read")
    m = measure(page)
    check(m["mode"] == ["Hide read", "true", "hide"], f"{size}: Hide read is said as {m['mode']}")
    check(m["icons"] == ["hide"], f"{size}: Hide read draws icons {m['icons']}")
    dock_holds(m, "Hide read")
    check(m["labelFits"] is True, f"{size}: the words 'Hide read' touch or cross the button's sides")
    m = settled_entry()
    check(m["firstShown"] == ANSWER_ID, f"{size}: Hide read's thread starts at {m['firstShown']}, not the answer")
    check(m["answerFolded"] == "false", f"{size}: Hide read's answer is folded")
    floating = number(m, "floating")
    top = number(m, "answer", "top")
    check(floating <= top <= floating + 16,
          f"{size}: Hide read landed the answer at {top:.0f}px, not just under the floating line at {floating:.0f}px"
          f"{' (the list is at its end)' if m['atEnd'] else ''}")
    notes.append(f"Hide read: the thread starts at the answer, unfolded at {top:.0f}px under the pill at {floating:.0f}px")
    shot("4-hide-read-entry")

    # `#222 unread-replies`: Main, still in Hide read. The owner's question is read, and kept.
    page.select_option("#thread-select", "main")
    page.wait_for_function(
        f"() => !!document.querySelector('#discord-log > li[data-id=\"{ROOT_ID}\"] > .thread-unread:not([hidden])')",
        timeout=10_000)
    page.wait_for_timeout(300)
    main_js: object = page.evaluate(MAIN_JS, ROOT_ID)
    check(isinstance(main_js, dict), f"{size}: Main measured nothing: {main_js!r}")
    assert isinstance(main_js, dict)
    mm = {str(key): value for key, value in main_js.items()}
    check(mm["shown"] == ["7600000000000000001", ROOT_ID],
          f"{size}: Hide read on Main draws {mm['shown']}, not the agent's update and the kept question")
    check(mm["text"] == "4 unread", f"{size}: the question's unread count says {mm['text']!r}")
    check(mm["label"] == "Open this thread at its first unread reply, 4 unread replies",
          f"{size}: the count's accessible name is {mm['label']!r}")
    check(mm["ownRead"] == "true", f"{size}: the kept question is no longer drawn as read")
    check(mm["veil"] not in ("rgba(0, 0, 0, 0)", "transparent"),
          f"{size}: the kept question is not faded as a read row: {mm['veil']}")
    check(mm["chipColor"] == mm["accent"] and mm["chipBorder"] == mm["accent"],
          f"{size}: the count is drawn {mm['chipColor']} on {mm['chipBorder']}, not the accent {mm['accent']}")
    check(mm["chipLayer"] == ["relative", "1", "none"],
          f"{size}: the count is not lifted over the veil, undrained: {mm['chipLayer']}")
    chip_box, replies_box, row_box = mm["chip"], mm["replies"], mm["row"]
    check(isinstance(chip_box, dict) and isinstance(replies_box, dict) and isinstance(row_box, dict),
          f"{size}: the count, N replies or the row has no box: {mm}")
    assert isinstance(chip_box, dict) and isinstance(replies_box, dict) and isinstance(row_box, dict)
    check(abs(chip_box["top"] - replies_box["top"]) <= 2 and chip_box["left"] >= replies_box["right"],
          f"{size}: the count is not beside N replies on one line: {chip_box} against {replies_box}")
    check(chip_box["right"] <= row_box["right"], f"{size}: the count runs past its row: {chip_box} in {row_box}")
    check(mm["chipPressable"] is True, f"{size}: the middle of the count does not press it")
    check(number(mm, "overflow") <= 0, f"{size}: Main scrolls sideways by {number(mm, 'overflow'):.0f}px")
    notes.append(f"Main, Hide read: the read question kept, '4 unread' beside '52 replies' at {chip_box['top']:.0f}px, "
                 f"{chip_box['width']:.0f}x{chip_box['height']:.0f}px, in the accent over the veil")
    shot("4b-main-unread-replies")
    pairs_js: object = page.evaluate(LONG_PAIRS_JS, [ROOT_ID, [list(pair) for pair in LONG_PAIRS]])
    check(isinstance(pairs_js, list) and len(pairs_js) == len(LONG_PAIRS), f"{size}: the long pairs measured nothing: {pairs_js!r}")
    assert isinstance(pairs_js, list)
    shared = []
    for measured in pairs_js:
        check(isinstance(measured, dict), f"{size}: a long pair measured nothing: {measured!r}")
        assert isinstance(measured, dict)
        pair = measured["pair"]
        check(measured["whole"] is True, f"{size}: a chip of '{pair}' broke onto a second line inside itself: {measured}")
        check(measured["inside"] is True, f"{size}: a chip of '{pair}' runs past its row: {measured}")
        check(number(measured, "overflow") <= 0, f"{size}: '{pair}' makes Main scroll sideways")
        if measured["oneLine"] is True:
            shared.append(str(pair))
    # The two pairs the owner's phone is likeliest to show stay on one line at either size.
    check(f"{LONG_PAIRS[0][0]} + {LONG_PAIRS[0][1]}" in shared and f"{LONG_PAIRS[1][0]} + {LONG_PAIRS[1][1]}" in shared,
          f"{size}: a long pair no longer shares one line: {pairs_js}")
    notes.append(f"the longest pairs each one line, inside the row; side by side: {', '.join(shared) or 'none'}")

    # A real tap on the count enters the thread on its first unread reply.
    tap(page, f'#discord-log > li[data-id="{ROOT_ID}"] > .thread-unread', touch)
    page.wait_for_function(
        f"() => {{ const row = document.querySelector('#discord-log > li[data-id=\"{ANSWER_ID}\"]');"
        " return row && row.dataset.collapsed === 'false'; }", timeout=10_000)
    page.wait_for_timeout(400)
    m = measure(page)
    check(m["firstShown"] == ANSWER_ID, f"{size}: the count opened the thread at {m['firstShown']}, not the answer")
    floating = number(m, "floating")
    top = number(m, "answer", "top")
    check(floating <= top <= floating + 16,
          f"{size}: the count landed the answer at {top:.0f}px, not just under the floating line at {floating:.0f}px")
    notes.append(f"the count, tapped, opened the thread on the answer, unfolded at {top:.0f}px")
    shot("4c-from-the-count")
    return notes


def held_walk(page: Page, touch: bool, size: str, shots: Path | None, collapsed: dict[str, float],
              gate: threading.Event) -> list[str]:
    """An old thread in Collapse read: drawn from the rows All holds, then read behind them.

    The thread's read is held at `gate` until the rows All held have been measured.
    """
    notes: list[str] = []
    gate.clear()
    answer_open = (f"() => {{ const row = document.querySelector('#discord-log > li[data-id=\"{ANSWER_ID}\"]');"
                   " return row && row.dataset.collapsed === 'false'; }")

    def shot(name: str) -> None:
        if shots is not None:
            page.screenshot(path=str(shots / f"read-modes-{size}-{name}.png"))

    def named() -> object:
        return page.evaluate("() => document.getElementById('todo-filter-label').textContent")

    page.fill("#api-token", TOKEN)
    page.click("#save-token")
    page.wait_for_function("() => !document.getElementById('screen-main').hidden", timeout=10_000)
    page.click("#view-switch")
    page.wait_for_function(f"() => document.querySelectorAll('#discord-log > li').length === {HELD_WINDOW}",
                           timeout=10_000)
    page.wait_for_timeout(300)
    for _tap in range(3):
        if named() == "Collapse read":
            break
        tap(page, "#todo-filter", touch)
        page.wait_for_timeout(250)
    check(named() == "Collapse read", f"{size}: the cycle never reached Collapse read")

    # All's window does not reach the root, so the picker offers the thread once a touch has read the
    # channel's thread list, as it does for a thumb.
    page.dispatch_event("#thread-select", "pointerdown")
    page.wait_for_function(f"() => [...document.getElementById('thread-select').options]"
                           f".some((option) => option.value === 'thread:{THREAD_ID}')", timeout=10_000)
    # The server holds the thread's read back: what is up is what All held, landed at once.
    page.select_option("#thread-select", f"thread:{THREAD_ID}")
    page.wait_for_function(answer_open, timeout=5_000)
    m = measure(page)
    check(m["root"] is None, f"{size}: the root was up before the thread's read landed; these are not held rows")
    floating = number(m, "floating")
    top = number(m, "answer", "top")
    check(floating <= top <= floating + 16,
          f"{size}: the held rows landed the answer at {top:.0f}px, not just under the floating line at {floating:.0f}px")
    notes.append(f"held rows: answer unfolded at {top:.0f}px before the read")
    shot("5-held-rows-before-read")
    gate.set()

    # The read lands: the root at the head, the line, the answer still open, as in a thread entered
    # with every row held.
    page.wait_for_function(f"() => document.querySelector('#discord-log > li[data-id=\"{ROOT_ID}\"]')",
                           timeout=15_000)
    page.wait_for_timeout(400)
    m = measure(page)
    floating = number(m, "floating")
    found = {"root": number(m, "root", "top"), "line": number(m, "line", "top"), "answer": number(m, "answer", "top")}
    check(floating <= found["root"] <= floating + 16,
          f"{size}: the read behind the held rows left the root at {found['root']:.0f}px, not at the head under {floating:.0f}px")
    check(m["lineText"] == f"… {READ_REPLIES} read messages …", f"{size}: the line says {m['lineText']!r}")
    check(m["answerFolded"] == "false", f"{size}: the read behind the held rows folded the answer")
    room = number(m, "areaBottom") - floating
    check(found["answer"] - floating <= room / 2,
          f"{size}: the answer starts at {found['answer']:.0f}px, below the upper half of the room under the pill")
    # Within the pixel the first walk itself moves by between entries (108px or 109px under the pill).
    for name, value in found.items():
        check(abs(value - collapsed[name]) <= 2,
              f"{size}: after the read the {name} is at {value:.0f}px, not at {collapsed[name]:.0f}px, where a thread "
              "entered with every row held has it")
    notes.append(f"its read: root at {found['root']:.0f}px, line at {found['line']:.0f}px, "
                 f"answer unfolded at {found['answer']:.0f}px, as entered with every row held")
    shot("6-held-rows-after-read")
    return notes


def serve(api: FakeApi, web_root: Path) -> tuple[ThreadingHTTPServer, threading.Thread, str]:
    """`api` and the page's assets from one loopback origin; the server, its thread, and the page's URL."""
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(api, web_root))
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, thread, f"http://127.0.0.1:{server.server_port}/voice"


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
    api = FakeApi()
    # The old thread: All's newest page starts at the answer, and the thread's read waits at the gate.
    gate = threading.Event()
    held_api = FakeApi(all_window=HELD_WINDOW, thread_gate=gate)
    served = [(api, *serve(api, web_root)), (held_api, *serve(held_api, web_root))]
    url, held_url = served[0][3], served[1][3]
    sizes = (("412x915", 412, 915, True), ("1280x800", 1280, 800, False))
    report: list[str] = []
    try:
        with sync_playwright() as playwright:
            for size, width, height, phone in sizes:
                collapsed: dict[str, float] = {}
                for held in (False, True):
                    with tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-") as profile:
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
                        page.goto(held_url if held else url, wait_until="load")
                        if held:
                            notes = held_walk(page, phone, size, args.screenshots, collapsed, gate)
                        else:
                            notes = walk(page, phone, size, args.screenshots, collapsed)
                        check(not errors, f"{size}: the page threw: {errors}")
                        version = context.browser.version if context.browser else "Chromium"
                        context.close()
                        report.append(f"{version} at {size}{', an old thread' if held else ''}: " + "; ".join(notes))
        for line in report:
            print(line)
        return 0
    finally:
        gate.set()
        for fake, server, thread, _url in served:
            fake.stopping.set()
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == "__main__":
    raise SystemExit(main())
