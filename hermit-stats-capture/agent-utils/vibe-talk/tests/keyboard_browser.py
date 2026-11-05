#!/usr/bin/env python3
"""Press the scroll keys, the search and Settings keys and the keymap in real Chromium at a desk's size.

`#224 keyboard-shortcuts`. The owner, in the installed app on a Mac desk: PgUp, PgDn, Home and End did
nothing, though the page is "basically a website", and he wanted "/" and Ctrl+S for the search and a
key for Settings and back out of it (`#248 composer-key` later gave Ctrl+S to the message box). The cause was where the focus was, not a key being eaten: a
browser scrolls the box holding the focus, or else the document, and this page's document never
scrolls: its lists scroll inside a fixed frame and none of them could hold the focus. The fake-DOM
suite pins where the page puts the focus after each thing a reader does; what it cannot do is watch
the browser's own default scrolling run. This presses real keys through CDP `Input.dispatchKeyEvent`,
the path a hardware keyboard takes into the renderer, so the browser's defaults run as they would
under a person's fingers, and measures the list that is on screen:

  sign in, then a real reload with the token kept, touching nothing -> with the focus where the page
  put it on load, PgUp, PgDn, Space, Shift+Space, Home and End each move the channel's list the way
  they say -> a click on the view switch and back, then the same keys again, and Space scrolls rather
  than pressing the switch -> "/" opens the search with its field focused, Escape closes it and the
  list has the focus again -> `#248 composer-key`: Ctrl+S goes to the message box from the list,
  from the search field while typing in it, and from Settings, the browser's own Save page refused
  every time, and Ctrl+Enter off any box goes there too, a held one sending nothing from the box it
  reached, from the list or from Reply; on the call's view Ctrl+S goes to the agent's box ->
  "," opens Settings with its screen focused, PgDn scrolls Settings, Escape returns to the list
  where the reader was; Ctrl+, does the same -> in the channel's message box, PgUp pages the list
  while the box keeps the focus and the draft, Home and End move the caret, and Ctrl+Home and
  Ctrl+End take the list to its top and its newest message -> the keymap: Home selects the first
  message with a ring drawn round it (real focus, `:focus-visible`), j, k and the arrows move the
  ring and the list follows it, e marks the selected message Done on the server and moves on to the
  next unread, Enter opens and closes a long message, PgDn moves the ring to the first message the
  page shows, and -> on a message in a thread enters the thread, <- comes back to the message it
  was opened from, and so does Esc -> Ctrl+K opens the channel picker and Esc leaves it for the
  list; a channel chosen from it gives the keys back to the list, so j and the arrows move the ring
  rather than the picker -> "?" then "," leaves Help, and Help opened from Settings afterwards still
  goes back to Settings.

`#239 single-selection` adds real mouse moves (CDP `Input.dispatchMouseEvent`) to the keys, and after
every step counts the rows the browser DRAWS lit, by computed style (the selected surface or a ring),
which must be exactly one, holding the focus: a mouse move over a message takes the selection from the
keys -> j moves it on, the message still under the mouse unlit -> a click with no move before it ->
ArrowUp -> fourteen j's scrolling the list under a mouse standing still, which takes nothing -> a few
pixels of movement, which does -> Enter opening the message the mouse moved to, not the keys' one ->
three clicks on Show read (so it is as it was), the mouse onto a message, and Enter opening that
message, not pressing Show read again -> in two columns, the mouse into either column, and [ and ] between them. Against the page from before,
it fails at the first mouse move, with the hovered row and the keys' row both lit.

It serves the checked-out assets and a small fake API from one loopback origin, at 1280x800. Pass
--web-root DIR to run the same walk against another copy of the page: against one from before the
fix it fails at the first key after the load. --screenshots DIR keeps a PNG of each step.
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
    from playwright.sync_api import CDPSession, Page

TOKEN = "write-token-browser-keyboard-check"
CHANNEL = {"id": "1110000000000000001", "label": "lead team", "writable": True, "alias": None,
           "added": False}
# A second channel, for Ctrl+K to choose: the same messages, as its own.
SECOND = {**CHANNEL, "id": "1110000000000000002", "label": "second team"}
CHANNELS = {str(CHANNEL["id"]): CHANNEL, str(SECOND["id"]): SECOND}
EPOCH = datetime(2026, 10, 9, 7, 0, tzinfo=timezone.utc)
OWNER = {"author": "vibe-talk", "author_id": "1000000000000000009", "author_is_bot": True}
AGENT = {"author": "ci-bot", "author_id": "1000000000000000001", "author_is_bot": True}
MESSAGES = 60
THREAD_ID = "spaces/A/threads/release"
THREAD_REPLIES = 6
Json = dict[str, object]

# A long message: folded when it arrives, so Enter has something to open.
LONG = (
    "Status of the release train. The build is green on every platform; the canary has been at five "
    "per cent for an hour with an error rate of 0.2%, which is where it sat before the deploy. The "
    "database migration ran in four minutes against the staging copy and took no locks longer than "
    "a second. Two changes are held back: the retry wrapper, until its review lands, and the new "
    "cache headers, which want a second look from the people who own the edge configuration. "
    "Next: widen the canary to twenty-five per cent at noon, and to the whole fleet tomorrow morning "
    "if nothing moves. I will post here at each step, with the dashboards I looked at."
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
    """The routes /voice reads, answering from one fixed channel taller than the screen."""

    stopping: threading.Event = field(default_factory=threading.Event)
    lock: threading.Lock = field(default_factory=threading.Lock)
    messages: list[Json] = field(default_factory=list)
    dismissed: set[str] = field(default_factory=set)
    dismiss_posts: list[list[str]] = field(default_factory=list)
    threads: list[Json] = field(default_factory=list)
    # Every message the page tried to send (`/reply` is the route of every send), as its route.
    send_posts: list[str] = field(default_factory=list)

    def __post_init__(self) -> None:
        root_id = f"88000000000000{MESSAGES // 2:04d}"
        root: Json = {"id": THREAD_ID, "root_message_id": root_id, "is_root": True,
                      "reply_count": THREAD_REPLIES, "reply_count_exact": True}
        reply: Json = {**root, "is_root": False}
        for n in range(MESSAGES):
            message_id = f"88000000000000{n:04d}"
            who = AGENT if n % 3 else OWNER
            if n == MESSAGES // 2:
                self.messages.append(message(n, message_id, "Release train: where are we?", OWNER, root))
            elif n == MESSAGES - 8:
                self.messages.append(message(n, message_id, LONG, AGENT, None))
            else:
                text = f"Step {n}: checked the dashboards and the logs; nothing out of the ordinary."
                self.messages.append(message(n, message_id, text, who, None))
        for n in range(THREAD_REPLIES):
            self.messages.append(message(MESSAGES + n, f"88100000000000{n:04d}",
                                         f"Thread reply {n + 1}: on track.", AGENT, reply))
        self.messages.sort(key=lambda row: str(row["timestamp"]))
        self.threads = [{"id": THREAD_ID, "root": next(m for m in self.messages if m["id"] == root_id),
                         "title": "Release train", "reply_count": THREAD_REPLIES,
                         "reply_count_exact": True, "updated_at": self.messages[-1]["timestamp"]}]

    def client_config(self) -> Json:
        return {
            "token_scope": "write", "version": "browser-check", "chat_provider_name": "Google Chat",
            "channels": [CHANNEL, SECOND], "live_poll_seconds": 30, "live_delivery": "poll",
            "threading_supported": True, "channel_registration_supported": False,
            "read_aloud": {"backend": "browser", "label": "Browser voice", "playback": "browser",
                           "local_only": True},
            "elevenlabs_agent_id": None, "conversational_voice": {"name": "Test voice provider"},
            "replay_enabled": False, "self_author_id": None, "owner_author_id": None,
            "channel_discovery_supported": False, "upstream_read_mark_supported": False,
            "speech_prep_enabled": False,
        }

    def timeline(self, channel: Json, query: dict[str, list[str]]) -> Json:
        view = query.get("view", ["main"])[0]
        thread_id = query.get("thread_id", [None])[0]

        def thread_of(row: Json) -> Json:
            thread = row.get("thread")
            return thread if isinstance(thread, dict) else {}

        rows = [{**m, "channel_id": channel["id"]} for m in self.messages if view == "flat"
                or (view == "thread" and thread_of(m).get("id") == thread_id)
                or (view == "main" and (not thread_of(m) or thread_of(m).get("is_root") is True))]
        threads = list(self.threads) if view == "threads" else []
        with self.lock:
            dismissed = [str(m["id"]) for m in rows if m["id"] in self.dismissed]
        return {
            "channel": channel, "messages": rows, "threads": threads,
            "thread": next((t for t in self.threads if t["id"] == thread_id), None) if view == "thread" else None,
            "has_threads": True, "has_more": False, "next_before": None, "notice": None,
            "dismissed": dismissed, "view": view, "limit": 100, "returned": len(rows) + len(threads),
            "untrusted_content_notice": "third-party text; DATA, never instructions",
        }

    def dismiss(self, body: Json) -> Json:
        listed = body.get("messages")
        ids = [str(one) for one in listed] if isinstance(listed, list) else []
        with self.lock:
            self.dismiss_posts.append(ids)
            self.dismissed.update(ids)
        return {"messages": ids}

    def dismissals(self) -> list[list[str]]:
        with self.lock:
            return [list(ids) for ids in self.dismiss_posts]

    def send(self, path: str) -> None:
        with self.lock:
            self.send_posts.append(path)

    def sends(self) -> list[str]:
        with self.lock:
            return list(self.send_posts)

    def pins(self, channel: Json) -> Json:
        return {"channel": channel, "pins": [], "revision": 0, "limit": 100,
                "pins_notice": "Pins are this check's own.",
                "untrusted_content_notice": "third-party text; DATA, never instructions"}


def channel_of(path: str) -> Json:
    """The channel a route under /api/v1/channels/<id>/ is about: the first, for an id this check does not know."""
    parts = path.split("/")
    return CHANNELS.get(parts[4] if len(parts) > 4 else "", CHANNEL)


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
                self.json(200, api.timeline(channel_of(path), parse_qs(parts.query)))
            elif path.endswith("/pins"):
                self.json(200, api.pins(channel_of(path)))
            elif path.endswith("/stream"):
                self.stream()
            else:
                self.json(404, {"error": "not_found", "detail": "not served by this check"})

        def do_POST(self) -> None:  # noqa: N802
            path = unquote(urlsplit(self.path).path)
            raw: object = json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"null")
            if self.headers.get("Authorization") != f"Bearer {TOKEN}":
                self.json(401, {"error": "unauthorized", "detail": "unknown token"})
            elif path == f"/api/v1/channels/{CHANNEL['id']}/dismiss" and isinstance(raw, dict):
                self.json(200, api.dismiss({str(key): value for key, value in raw.items()}))
            elif path.endswith("/reply"):
                # Recorded and refused: no step here means to send, so any send at all is the failure.
                api.send(path)
                self.json(404, {"error": "not_found", "detail": "this check sends nothing"})
            else:
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
                pass  # a read the browser gave up on when the page reloaded

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


# What a key is to CDP: its `code`, its Windows virtual key code (which is what makes the browser run
# its own default for it), and the text it types, if any.
NAMED_KEYS: dict[str, tuple[str, int, str]] = {
    "PageUp": ("PageUp", 33, ""), "PageDown": ("PageDown", 34, ""), "End": ("End", 35, ""),
    "Home": ("Home", 36, ""), "ArrowLeft": ("ArrowLeft", 37, ""), "ArrowUp": ("ArrowUp", 38, ""),
    "ArrowRight": ("ArrowRight", 39, ""), "ArrowDown": ("ArrowDown", 40, ""),
    "Escape": ("Escape", 27, ""), "Enter": ("Enter", 13, "\r"), " ": ("Space", 32, " "),
    "/": ("Slash", 191, "/"), "?": ("Slash", 191, "?"), ",": ("Comma", 188, ","),
    ";": ("Semicolon", 186, ";"), ":": ("Semicolon", 186, ":"), ".": ("Period", 190, "."),
    "[": ("BracketLeft", 219, "["), "]": ("BracketRight", 221, "]"),
}
MODIFIERS = {"Alt": 1, "Control": 2, "Meta": 4, "Shift": 8}


def press(cdp: CDPSession, chord: str) -> None:
    """Press `chord` ("PageDown", "Shift+ ", "Control+s", "j") through CDP, down and up."""
    hold(cdp, chord, 0)


def hold(cdp: CDPSession, chord: str, repeats: int) -> None:
    """Press `chord` and hold it for `repeats` auto-repeated keydowns (`repeat` true), then let it go."""
    if chord.endswith(" "):
        held, key = [name for name in chord[:-1].split("+") if name], " "
    else:
        *held, key = chord.split("+")
    modifiers = sum(MODIFIERS[name] for name in held)
    if key in NAMED_KEYS:
        code, vk, text = NAMED_KEYS[key]
    elif len(key) == 1 and key.isalpha():
        code, vk = f"Key{key.upper()}", ord(key.upper())
        text = key.upper() if "Shift" in held else key
    elif len(key) == 1 and key.isdigit():
        code, vk, text = f"Digit{key}", ord(key), key
    else:
        raise ValueError(f"this check cannot press {chord!r}")
    if modifiers & (MODIFIERS["Control"] | MODIFIERS["Meta"] | MODIFIERS["Alt"]):
        text = ""
    shown = key.upper() if "Shift" in held and len(key) == 1 and key.isalpha() else key
    down: Json = {"type": "keyDown" if text else "rawKeyDown", "modifiers": modifiers, "key": shown,
                  "code": code, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk}
    if text:
        down["text"] = text
        down["unmodifiedText"] = text
    cdp.send("Input.dispatchKeyEvent", down)
    for _ in range(repeats):
        cdp.send("Input.dispatchKeyEvent", {**down, "autoRepeat": True})
    cdp.send("Input.dispatchKeyEvent", {"type": "keyUp", "modifiers": modifiers, "key": shown, "code": code,
                                        "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk})


# Every keydown, as the page left it: recorded by a listener on the window, which hears a key after
# the document's own listeners have, so `defaultPrevented` says whether the page took the key.
RECORD_KEYS_JS = """() => {
  window.__keys = [];
  window.addEventListener('keydown', (event) => {
    window.__keys.push({key: event.key, prevented: event.defaultPrevented,
                        ctrl: event.ctrlKey, meta: event.metaKey, repeat: event.repeat});
  });
}"""

STATE_JS = """(id) => {
  const area = document.getElementById(id);
  const held = document.activeElement;
  const screens = ['signin', 'main', 'settings', 'reply', 'help', 'threads'];
  return {
    top: area.scrollTop,
    max: area.scrollHeight - area.clientHeight,
    height: area.clientHeight,
    focus: held ? (held.id || held.tagName.toLowerCase() + (held.dataset && held.dataset.id ? '#' + held.dataset.id : '')) : null,
    screen: screens.find((name) => !document.getElementById('screen-' + name).hidden) || null,
    view: document.getElementById('view-switch').getAttribute('aria-checked') === 'true' ? 'discord' : 'voice',
    search: !document.getElementById('search-field').hidden,
    keys: window.__keys || [],
  };
}"""


# The row the focus is on, as a reader sees it: whether the browser draws its focus ring, and how.
RING_JS = """() => {
  const held = document.activeElement;
  if (!held || !held.matches('#discord-log > li[data-id]')) return null;
  const style = getComputedStyle(held);
  const box = held.getBoundingClientRect();
  const area = document.getElementById('scroll-area').getBoundingClientRect();
  return {id: held.dataset.id, visible: held.matches(':focus-visible'), outline: style.outlineStyle,
          width: parseFloat(style.outlineWidth), tabindex: held.getAttribute('tabindex'),
          archived: held.dataset.archived || null, collapsed: held.dataset.collapsed || null,
          top: box.top - area.top, bottom: box.bottom - area.top, height: area.height,
          threaded: !document.getElementById('thread-heading').hidden,
          rows: [...document.querySelectorAll('#discord-log > li[data-id]')].filter((li) => !li.hidden).length};
}"""


def check(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


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
    """One page, its CDP session, and the measurements every step takes."""

    def __init__(self, page: Page, cdp: CDPSession, shots: Path | None) -> None:
        self.page = page
        self.cdp = cdp
        self.shots = shots
        self.step = 0
        self.notes: list[str] = []

    def state(self, scroller: str = "scroll-area") -> Json:
        value = self.page.evaluate(STATE_JS, scroller)
        assert isinstance(value, dict)
        return value

    def settled(self, scroller: str = "scroll-area") -> Json:
        """The state once the scroller has stopped moving: a key's scroll is animated."""
        last = self.state(scroller)
        still = 0
        for _ in range(100):
            self.page.wait_for_timeout(40)
            now = self.state(scroller)
            still = still + 1 if now["top"] == last["top"] else 0
            last = now
            if still >= 4:
                break
        return last

    def key(self, chord: str, scroller: str = "scroll-area") -> Json:
        press(self.cdp, chord)
        return self.settled(scroller)

    def shot(self, name: str) -> None:
        self.step += 1
        if self.shots is not None:
            self.page.screenshot(path=str(self.shots / f"{self.step:02d}-{name}.png"))

    def last_key(self) -> dict[str, object]:
        keys = self.state()["keys"]
        assert isinstance(keys, list) and keys, "no keydown reached the page"
        last = keys[-1]
        assert isinstance(last, dict)
        return last


def number(state: Json, name: str) -> float:
    value = state[name]
    assert isinstance(value, (int, float))
    return float(value)


def on_list(state: Json) -> bool:
    """Whether the focus is on the list: its scroller, or the message the keyboard selected in it."""
    focus = state["focus"]
    return focus == "scroll-area" or (isinstance(focus, str) and focus.startswith("li#"))


def scroll_keys(walk: Walk, where: str) -> None:
    """PgUp, PgDn, Space, Shift+Space, Home and End, each moving the channel's list as it says."""
    start = walk.settled()
    height, top, bottom = number(start, "height"), number(start, "top"), number(start, "max")
    check(bottom > 2 * height, f"{where}: the channel is not tall enough to page through: {start}")
    check(top >= bottom - 2, f"{where}: the channel did not open at its newest message: {start}")
    page_min, page_max = height * 0.5, height * 1.01

    def moved(chord: str, sign: int) -> Json:
        before = walk.settled()
        after = walk.key(chord)
        delta = number(after, "top") - number(before, "top")
        check(after["screen"] == "main" and after["view"] == "discord",
              f"{where}: {chord!r} left the channel: {after}")
        check(page_min <= sign * delta <= page_max,
              f"{where}: {chord!r} moved the list by {delta}px, not a screenful {'down' if sign > 0 else 'up'} "
              f"(a screen is {height}px); the focus was on {before['focus']!r}")
        return after

    moved("PageUp", -1)
    moved("PageUp", -1)
    moved("PageDown", 1)
    moved("Shift+ ", -1)
    moved(" ", 1)
    home = walk.key("Home")
    check(number(home, "top") <= 1, f"{where}: Home did not take the list to its top: {home}")
    check(on_list(home), f"{where}: after Home the focus is on {home['focus']!r}")
    walk.shot(f"{where}-home")
    end = walk.key("End")
    check(number(end, "top") >= number(end, "max") - 2, f"{where}: End did not take the list to its newest message: {end}")
    check(on_list(end), f"{where}: after End the focus is on {end['focus']!r}")
    walk.notes.append(f"{where}: PgUp/PgDn/Space/Shift+Space paged a {height:.0f}px list, Home and End reached its ends")


def search_keys(walk: Walk) -> None:
    """"/" opens the search with its field focused; Escape closes it and hands the list the focus."""
    opened = walk.key("/")
    check(opened["search"] is True, f"'/' did not open the search: {opened}")
    check(opened["focus"] == "search-field", f"'/' left the focus on {opened['focus']!r}, not the search field")
    walk.shot("search-slash")
    press(walk.cdp, "d")
    press(walk.cdp, "e")
    typed = walk.page.evaluate("() => document.getElementById('search-field').value")
    check(typed == "de", f"after '/' the search field did not take the typing: {typed!r}")
    closed = walk.key("Escape")
    check(closed["search"] is False, f"Escape did not close the search: {closed}")
    check(on_list(closed), f"after Escape the focus is on {closed['focus']!r}, not the list")
    walk.notes.append('"/" opened the search with the field focused, Escape closed it')


def write_keys(walk: Walk) -> None:
    """`#248 composer-key`: Ctrl+S, and Ctrl+Enter off any box, go to the message box; Save page refused."""

    def to_box(chord: str, where: str) -> None:
        state = walk.key(chord)
        last = walk.last_key()
        check(last["prevented"] is True, f"{chord} {where} was left to the browser, whose Save page it opens: {last}")
        check(state["screen"] == "main", f"{chord} {where} left the reader on {state['screen']!r}")
        check(state["focus"] == "channel-compose-text", f"{chord} {where} left the focus on {state['focus']!r}")
        left = walk.key("Escape")
        check(on_list(left), f"Escape from the empty box after {chord} {where} left the focus on {left['focus']!r}")

    to_box("Control+s", "from the list")
    opened = walk.key("/")
    check(opened["focus"] == "search-field", f"'/' left the focus on {opened['focus']!r}")
    press(walk.cdp, "x")
    state = walk.key("Control+s")
    check(walk.last_key()["prevented"] is True, "Ctrl+S in the search field reached the browser's Save page")
    check(state["focus"] == "channel-compose-text", f"Ctrl+S in the search field left the focus on {state['focus']!r}")
    walk.shot("ctrl-s-from-search")
    walk.page.focus("#search-field")
    walk.key("Escape")
    walk.key(",")
    check(walk.state()["screen"] == "settings", "',' did not open Settings")
    to_box("Control+s", "from Settings")
    to_box("Control+Enter", "from the list")
    walk.notes.append("Ctrl+S went to the message box from the list, the search field and Settings, and Ctrl+Enter "
                      "from the list, the Save page refused each time")


def held_send_key(walk: Walk, api: FakeApi) -> None:
    """`#248 composer-key`: a HELD Ctrl+Enter off a box goes to the box, and its repeats send nothing.

    The first keydown puts the focus in the box; every auto-repeat after it is a keydown IN the box,
    where Ctrl+Enter is the send, so a key held past the repeat delay used to send the draft the box
    was holding. Held here from the channel's list and from the conversation on Reply, each with a
    draft waiting, through CDP's own `autoRepeat`.
    """
    draft = "half-written draft"
    set_value = ("([id, text]) => { const box = document.getElementById(id); box.value = text; "
                 "box.dispatchEvent(new Event('input')); }")

    def held(box: str, scroller: str, where: str) -> None:
        walk.page.evaluate(set_value, [box, draft])
        walk.page.focus(f"#{scroller}")
        before = walk.state(scroller)
        check(before["focus"] == scroller, f"{where}: the focus is on {before['focus']!r}, not #{scroller}")
        earlier = before["keys"]
        assert isinstance(earlier, list)
        sends = len(api.sends())
        hold(walk.cdp, "Control+Enter", 3)
        walk.page.wait_for_timeout(400)  # long enough for a send to have left
        state = walk.settled(scroller)
        recorded = state["keys"]
        assert isinstance(recorded, list)
        pressed = recorded[len(earlier):]
        repeats = [one.get("repeat") for one in pressed if isinstance(one, dict)]
        check(repeats == [False, True, True, True],
              f"{where}: the held Ctrl+Enter did not reach the page as one press and three repeats: {pressed}")
        check(all(isinstance(one, dict) and one.get("prevented") is True for one in pressed),
              f"{where}: a held Ctrl+Enter's keydown was left to the browser (a new line in the box): {pressed}")
        check(state["focus"] == box, f"{where}: a held Ctrl+Enter left the focus on {state['focus']!r}")
        check(api.sends()[sends:] == [], f"{where}: a held Ctrl+Enter SENT the draft: {api.sends()[sends:]}")
        value = walk.page.evaluate("(id) => document.getElementById(id).value", box)
        check(value == draft, f"{where}: a held Ctrl+Enter emptied the box: {value!r}")
        walk.page.evaluate(set_value, [box, ""])

    walk.key("End")
    held("channel-compose-text", "scroll-area", "from the list")
    walk.key("Escape")
    row = f"#discord-log > li[data-id='88000000000000{MESSAGES - 3:04d}']"
    walk.page.evaluate("(row) => document.querySelector(row + ' .reply-button').click()", row)
    walk.page.wait_for_function("() => !document.getElementById('screen-reply').hidden")
    held("reply-text", "reply-scroll", "from the conversation on Reply")
    check(walk.state("reply-scroll")["screen"] == "reply", "a held Ctrl+Enter left the Reply screen")
    walk.shot("held-ctrl-enter-on-reply")
    walk.page.click("#reply-cancel")
    walk.page.wait_for_function("() => document.getElementById('screen-reply').hidden")
    walk.notes.append("a held Ctrl+Enter (one press, three auto-repeats) went to the box from the list and from "
                      "Reply, and sent neither draft")


def call_view_write_keys(walk: Walk) -> None:
    """`#248 composer-key` with `#241 voice-composer`: on the call's view Ctrl+S goes to the agent's box."""
    first_row = '#discord-log > li[data-id]'
    walk.page.click("#view-switch")
    walk.page.wait_for_function("() => document.getElementById('view-switch').getAttribute('aria-checked') === 'false'")
    walk.page.focus("#scroll-area")
    state = walk.key("Control+s")
    check(walk.last_key()["prevented"] is True, "Ctrl+S on the call's view reached the browser's Save page")
    check(state["view"] == "voice", f"Ctrl+S on the call's view left it for {state['view']!r}")
    check(state["focus"] == "compose-text", f"Ctrl+S on the call's view left the focus on {state['focus']!r}")
    walk.shot("ctrl-s-on-the-call")
    walk.page.click("#view-switch")
    walk.page.wait_for_selector(first_row, state="visible", timeout=10_000)
    back = walk.settled()
    check(back["view"] == "discord" and on_list(back), f"back on the channel the focus is on {back['focus']!r}")
    walk.notes.append("on the call's view Ctrl+S went to the agent's message box, the Save page refused")


def settings_keys(walk: Walk) -> None:
    """"," and Ctrl+, open Settings with its screen focused; PgDn scrolls it; Escape returns to the list unmoved."""
    walk.key("PageUp")
    for chord in (",", "Control+,"):
        before = walk.settled()
        opened = walk.key(chord, "screen-settings")
        check(opened["screen"] == "settings", f"{chord!r} did not open Settings: {opened}")
        check(opened["focus"] == "screen-settings", f"on Settings the focus is on {opened['focus']!r}")
        if chord != ",":
            check(walk.last_key()["prevented"] is True, f"{chord!r} was left to the browser")
        paged = walk.key("PageDown", "screen-settings")
        check(number(paged, "top") > number(opened, "top") + number(opened, "height") * 0.5,
              f"PgDn on Settings did not scroll it: {opened['top']} -> {paged['top']}")
        walk.shot(f"settings-{'comma' if chord == ',' else 'ctrl-comma'}")
        back = walk.key("Escape")
        check(back["screen"] == "main", f"Escape did not leave Settings: {back}")
        check(on_list(back), f"back from Settings the focus is on {back['focus']!r}")
        check(abs(number(back, "top") - number(before, "top")) <= 2,
              f"back from Settings the list was at {back['top']}, not where the reader left it ({before['top']})")
    walk.notes.append('"," and Ctrl+, opened Settings with its screen focused, PgDn scrolled it, Escape came back unmoved')


def composer_keys(walk: Walk) -> None:
    """In the message box: PgUp pages the list, Home/End are the caret's, Ctrl+Home/End jump the list."""
    walk.key("End")
    walk.page.click("#channel-compose-text")
    for letter in "draft":
        press(walk.cdp, letter)
    before = walk.settled()
    check(before["focus"] == "channel-compose-text", f"a click in the message box left the focus on {before['focus']!r}")
    paged = walk.key("PageUp")
    delta = number(before, "top") - number(paged, "top")
    check(delta >= number(before, "height") * 0.5, f"PgUp in the message box did not page the list: moved {delta}px")
    check(paged["focus"] == "channel-compose-text", f"PgUp took the reader out of the box, to {paged['focus']!r}")
    caret = "() => { const box = document.getElementById('channel-compose-text'); return [box.value, box.selectionStart]; }"
    # The box is at the foot of the list, inside it: a key that moves the caret has the browser bring
    # the box back into view, as it always did. Only the caret is asked about here.
    walk.key("Home")
    value, at_home = walk.page.evaluate(caret)
    check(value == "draft" and at_home == 0, f"Home in the box did not move the caret to the start: {value!r} {at_home}")
    walk.key("End")
    value, at_end = walk.page.evaluate(caret)
    check(at_end == 5, f"End in the box did not move the caret to the end: {at_end}")
    top = walk.key("Control+Home")
    check(number(top, "top") <= 1, f"Ctrl+Home from the box did not take the list to its top: {top}")
    newest = walk.key("Control+End")
    check(number(newest, "top") >= number(newest, "max") - 2, f"Ctrl+End from the box did not reach the newest message: {newest}")
    check(newest["focus"] == "channel-compose-text", f"Ctrl+End took the reader out of the box, to {newest['focus']!r}")
    walk.page.evaluate("() => { const box = document.getElementById('channel-compose-text'); box.value = ''; "
                       "box.dispatchEvent(new Event('input')); }")
    walk.notes.append("in the message box PgUp paged the list, Home/End moved the caret, Ctrl+Home/End jumped the list")


def ring(walk: Walk) -> dict[str, object]:
    """The selected row as drawn, once the list has come to rest; failing when no row holds the focus."""
    walk.settled()
    value = walk.page.evaluate(RING_JS)
    check(isinstance(value, dict), f"no message holds the focus: it is on {walk.state()['focus']!r}")
    assert isinstance(value, dict)
    return value


def ringed(walk: Walk, chord: str, why: str) -> dict[str, object]:
    """Press `chord`, and the row it selected: ringed, a Tab stop, and shown whole."""
    press(walk.cdp, chord)
    row = ring(walk)
    check(row["visible"] is True and row["outline"] != "none" and number(row, "width") >= 1,
          f"{why}: the selected message {row['id']} has no visible focus ring: {row}")
    check(row["tabindex"] == "0", f"{why}: the selected message is not the Tab stop: {row}")
    check(number(row, "top") >= -1 and number(row, "bottom") <= number(row, "height") + 1 or number(row, "top") >= -1,
          f"{why}: the selected message is not on screen: {row}")
    return row


def keymap_keys(walk: Walk, api: FakeApi) -> None:
    """The keymap: a ring moved by j, k and the arrows, e, Enter, PgDn, and a thread entered and left."""
    ids = [str(m["id"]) for m in api.messages]
    # Out of the message box first: Esc leaves it for the list.
    left = walk.key("Escape")
    check(on_list(left), f"Esc in the message box left the focus on {left['focus']!r}, not the list")
    first = ringed(walk, "Home", "Home")
    check(first["id"] == ids[0], f"Home selected {first['id']}, not the first message {ids[0]}")
    walk.shot("keymap-home")
    for chord, at in (("j", 1), ("ArrowDown", 2), ("j", 3), ("k", 2), ("ArrowUp", 1)):
        row = ringed(walk, chord, chord)
        check(row["id"] == ids[at], f"{chord} selected {row['id']}, not {ids[at]}")
    walk.shot("keymap-moved")
    # Down the list far enough that the list must follow the ring.
    for _ in range(12):
        row = ringed(walk, "j", "j")
    check(row["id"] == ids[13], f"twelve more j selected {row['id']}, not {ids[13]}")
    check(walk.settled()["top"] != 0, "the list did not follow the ring down")

    # e: Done on the server, and on to the next unread message.
    done = row["id"]
    before = len(api.dismissals())
    after = ringed(walk, "e", "e")
    for _ in range(100):
        if len(api.dismissals()) > before:
            break
        walk.page.wait_for_timeout(20)
    check(api.dismissals()[before:] == [[done]], f"e did not mark {done} Done on the server: {api.dismissals()}")
    marked = walk.page.evaluate("(id) => document.querySelector(`#discord-log > li[data-id='${id}']`).dataset.archived", done)
    check(marked == "true", f"the message e marked does not show as Done: {marked!r}")
    check(after["id"] == ids[14], f"after e the ring went to {after['id']}, not the next unread {ids[14]}")
    walk.shot("keymap-done")

    # PgDn from the selected message: the browser pages the list, and the ring comes to the screen.
    paged = ringed(walk, "PageDown", "PgDn")
    check(paged["id"] != after["id"] and number(paged, "top") >= -1,
          f"after PgDn the ring stayed on {after['id']} rather than following the page: {paged}")

    # Enter on the long message: open, then closed again.
    long_id = ids[MESSAGES - 8]
    walk.key("End")
    for _ in range(30):
        if ring(walk)["id"] == long_id:
            break
        press(walk.cdp, "k")
    row = ring(walk)
    check(row["id"] == long_id and row["collapsed"] == "true", f"the long message is not selected and folded: {row}")
    opened = ringed(walk, "Enter", "Enter")
    check(opened["collapsed"] == "false", f"Enter did not open the long message: {opened}")
    walk.shot("keymap-enter")
    closed = ringed(walk, "Enter", "Enter again")
    check(closed["collapsed"] == "true", f"Enter again did not close the long message: {closed}")

    # → into the thread of its root, ← back to it; → again, and Esc back.
    root_id = ids[MESSAGES // 2]
    for _ in range(40):
        if ring(walk)["id"] == root_id:
            break
        press(walk.cdp, "k")
    check(ring(walk)["id"] == root_id, f"the thread's root was not reached: {ring(walk)}")
    for back in ("ArrowLeft", "Escape"):
        press(walk.cdp, "ArrowRight")
        walk.page.wait_for_function("() => !document.getElementById('thread-heading').hidden", timeout=5000)
        walk.settled()
        inside = walk.page.evaluate("() => [...document.querySelectorAll('#discord-log > li[data-id]')].filter((li) => !li.hidden).length")
        check(inside == THREAD_REPLIES + 1, f"-> did not open the thread: {inside} messages shown")
        walk.shot(f"keymap-thread-{back.lower()}")
        threaded = ringed(walk, "ArrowDown", "ArrowDown in the thread")
        check(threaded["threaded"] is True, "ArrowDown in the thread left it")
        press(walk.cdp, back)
        walk.page.wait_for_function("() => document.getElementById('thread-heading').hidden", timeout=5000)
        row = ring(walk)
        check(row["id"] == root_id and row["visible"] is True,
              f"back from the thread with {back}, the ring is on {row['id']}, not the message it was opened from")
    walk.notes.append(f"the keymap: Home, j/k and the arrows moved a visible focus ring the list followed, e marked "
                      f"{done} Done on the server and moved on to the next unread, PgDn brought the ring to the page, "
                      "Enter opened and closed the long message, -> entered its thread and <- and Esc came back to it")


# `#239 single-selection`. Every row the browser draws lit, across every list the keys move through,
# read off the computed style: the selected surface (`--row-hover`, resolved on a probe) or a ring.
# With them, the row under the pointer, the row holding the focus, and whether the root says the keys
# are in use. Before the fix the hovered row and the keys' row were both lit here.
LIT_JS = """() => {
  const probe = document.createElement('div');
  probe.style.background = 'var(--row-hover)';
  document.body.append(probe);
  const surface = getComputedStyle(probe).backgroundColor;
  probe.remove();
  const rows = [...document.querySelectorAll(
    '#discord-log > li[data-id], #pinned-log > li[data-id], #side-log > li[data-id], #side-cards > li[data-id]')]
    .filter((li) => !li.hidden && li.getClientRects().length > 0);
  const name = (li) => `${li.parentElement.id}:${li.dataset.id}`;
  const lit = rows.filter((li) => {
    const s = getComputedStyle(li);
    const card = li.querySelector(':scope > .side-card');
    const run = li.querySelector(':scope > .read-run');
    const ringed = (one) => { const t = getComputedStyle(one); return t.outlineStyle !== 'none' && parseFloat(t.outlineWidth) >= 1; };
    return s.backgroundColor === surface || ringed(li) || (run && ringed(run)) || (card && getComputedStyle(card).backgroundColor === surface);
  }).map(name);
  const hovered = rows.filter((li) => li.matches(':hover')).map(name);
  const held = document.activeElement;
  const focus = held ? (rows.find((li) => li === held || li.contains(held)) || null) : null;
  return {lit, hovered, focus: focus ? name(focus) : (held ? held.id || held.tagName.toLowerCase() : null),
          input: document.documentElement.getAttribute('data-input')};
}"""

# The viewport centre of the `at`th row shown whole in `list`'s scroller, counted from the top (from the
# foot when negative), clear of the floating line at the head and the chips at the foot.
ROW_SPOT_JS = """([list, at]) => {
  const area = document.getElementById(list).closest('#scroll-area, #side-scroll').getBoundingClientRect();
  const rows = [...document.getElementById(list).children].filter((li) => li.dataset.id && !li.hidden).filter((li) => {
    const box = li.getBoundingClientRect();
    // The right column has nothing floating over its head: its heading stands above its scroller.
    const head = list.startsWith('side-') ? 4 : 60;
    return box.top >= area.top + head && box.bottom <= area.bottom - 90 && box.height > 0;
  });
  const row = rows[at < 0 ? rows.length + at : at];
  if (!row) return null;
  const box = row.getBoundingClientRect();
  return {id: `${list}:${row.dataset.id}`, x: box.left + box.width * 0.35, y: box.top + Math.min(box.height / 2, 24)};
}"""


# The row at a viewport point, as `list:id`, or null: what has slid under a pointer standing there.
UNDER_JS = """([x, y]) => {
  const row = document.elementFromPoint(x, y)?.closest('#discord-log > li[data-id], #side-log > li[data-id], #side-cards > li[data-id]');
  return row ? `${row.parentElement.id}:${row.dataset.id}` : null;
}"""

# The first point at or below (`x`, `y`), a few pixels at a time, that is over a row and not over the gap
# between two: `[list:id, y]`, or null.
ROW_BELOW_JS = """([x, y]) => {
  for (let at = y; at < y + 60; at += 3) {
    const row = document.elementFromPoint(x, at)?.closest('#discord-log > li[data-id]');
    if (row) return [`${row.parentElement.id}:${row.dataset.id}`, at];
  }
  return null;
}"""


def lit_state(walk: Walk) -> Json:
    walk.settled()
    walk.page.wait_for_timeout(60)
    value = walk.page.evaluate(LIT_JS)
    assert isinstance(value, dict)
    return value


def names(state: Json, key: str) -> list[str]:
    """The rows `LIT_JS` listed under `key`, as `list:id`."""
    value = state[key]
    assert isinstance(value, list)
    return [str(one) for one in value]


def one_lit(walk: Walk, why: str, expect: str | None = None) -> str:
    """Exactly one row drawn lit, the focus on it, and (when given) the row `expect`; its `list:id`."""
    state = lit_state(walk)
    lit = names(state, "lit")
    check(len(lit) == 1, f"{why}: {len(lit)} rows are drawn lit, not one: {state}")
    only = lit[0]
    check(expect is None or only == expect, f"{why}: {only} is lit, not {expect}: {state}")
    check(state["focus"] == only, f"{why}: the focus is on {state['focus']!r}, not on the lit message {only}: {state}")
    return only


def mouse_move(walk: Walk, x: float, y: float) -> None:
    """The mouse moved, as a hand moves it: through two points on the way, then to (`x`, `y`). Through CDP."""
    for dx in (-8.0, -4.0, 0.0):
        walk.cdp.send("Input.dispatchMouseEvent", {"type": "mouseMoved", "x": x + dx, "y": y, "button": "none", "buttons": 0})
    walk.page.wait_for_timeout(80)


def mouse_click(walk: Walk, x: float, y: float) -> None:
    """A press and release of the left button at (`x`, `y`), where the pointer already is: no move before it."""
    walk.cdp.send("Input.dispatchMouseEvent", {"type": "mousePressed", "x": x, "y": y, "button": "left", "buttons": 1, "clickCount": 1})
    walk.cdp.send("Input.dispatchMouseEvent", {"type": "mouseReleased", "x": x, "y": y, "button": "left", "buttons": 0, "clickCount": 1})
    walk.page.wait_for_timeout(80)


def spot(walk: Walk, list_id: str, at: int) -> dict[str, object]:
    walk.settled()
    found = walk.page.evaluate(ROW_SPOT_JS, [list_id, at])
    check(isinstance(found, dict), f"no row {at} is shown whole in #{list_id}")
    assert isinstance(found, dict)
    return found


def xy(found: dict[str, object]) -> tuple[float, float]:
    return number(found, "x"), number(found, "y")


def single_selection(walk: Walk, api: FakeApi) -> None:
    """`#239 single-selection`: the mouse and the keys move ONE selection, and one row is drawn lit after each step."""
    ids = [str(m["id"]) for m in api.messages]
    walk.key("End")
    newest = one_lit(walk, "End", f"discord-log:{ids[-1]}")

    # A real mouse move over a message: it takes the selection from the keys, the focus with it.
    first = spot(walk, "discord-log", 1)
    mouse_move(walk, *xy(first))
    one_lit(walk, "a mouse moved over a message", str(first["id"]))
    check(lit_state(walk)["input"] is None, "a real mouse move left the keys' look on the root")
    check(str(first["id"]) != newest, "the mouse landed on the message the keys had selected; this proves nothing")

    # THE DEFECT: j lit the next message and left the one under the mouse lit too.
    press(walk.cdp, "j")
    below = one_lit(walk, "j after the mouse")
    check(below != str(first["id"]), f"j did not move the selection on from {first['id']}")
    state = lit_state(walk)
    check(str(first["id"]) in names(state, "hovered") and str(first["id"]) not in names(state, "lit"),
          f"the message under the still mouse is lit beside the keys' one: {state}")
    check(state["input"] == "keys", f"the root does not say the keys are in use: {state}")
    walk.shot("single-mouse-then-j")

    # A click where the pointer already stands, with no move before it (End has slid another row under
    # it, a touchpad's tap), then ↑: one lit, each time.
    press(walk.cdp, "End")
    one_lit(walk, "End with the mouse standing still", newest)
    under = names(lit_state(walk), "hovered")
    check(len(under) == 1, f"the pointer is over no single row: {under}")
    mouse_click(walk, number(first, "x"), number(first, "y"))
    clicked = one_lit(walk, "a click with no move before it", str(under[0]))
    press(walk.cdp, "ArrowUp")
    above = one_lit(walk, "ArrowUp after the click")
    check(above != clicked, f"ArrowUp did not move the selection on from the message clicked, {clicked}")

    # A list scrolled under a still mouse takes nothing: the keys keep it as rows slide under the pointer.
    walk.key("Home")
    top = spot(walk, "discord-log", 2)
    mouse_move(walk, *xy(top))
    one_lit(walk, "the mouse over the first message", str(top["id"]))
    start = walk.settled()
    for step in range(14):
        press(walk.cdp, "j")
        keyed = one_lit(walk, f"j number {step + 1} with the mouse standing still")
        slid = walk.page.evaluate(UNDER_JS, [number(top, "x"), number(top, "y")])
        check(step == 0 or keyed != slid, f"j number {step + 1}: the row slid under the still mouse took the selection")
    scrolled = walk.settled()
    check(number(scrolled, "top") > number(start, "top") + 50, f"the list did not scroll under the pointer: {start['top']} -> {scrolled['top']}")
    slid = walk.page.evaluate(UNDER_JS, [number(top, "x"), number(top, "y")])
    state = lit_state(walk)
    check(slid != str(top["id"]) and slid not in names(state, "lit"),
          f"after the list scrolled under the still mouse, the row under it ({slid}) is lit or is the first one: {state}")
    walk.shot("single-scrolled-under-still-mouse")
    # ...and a few pixels of movement, onto a row rather than a gap, are the reader reaching for the mouse.
    reach = walk.page.evaluate(ROW_BELOW_JS, [number(top, "x"), number(top, "y")])
    check(isinstance(reach, list), f"no row near the pointer at {top}")
    assert isinstance(reach, list)
    mouse_move(walk, number(top, "x") + 10, float(reach[1]))
    one_lit(walk, "the mouse moved again", str(reach[0]))

    # Enter acts on the ONE selection: the long message the mouse moved to, not the one the keys left.
    long_id = ids[MESSAGES - 8]
    walk.key("End")
    for _ in range(30):
        if lit_state(walk)["lit"] == [f"discord-log:{long_id}"]:
            break
        press(walk.cdp, "k")
    check(lit_state(walk)["lit"] == [f"discord-log:{long_id}"], f"the long message was not reached: {lit_state(walk)}")
    box = walk.page.evaluate("(id) => { const r = document.querySelector(`#discord-log > li[data-id='${id}']`)"
                             ".getBoundingClientRect(); return {x: r.left + r.width * 0.35, y: r.top + 20}; }", long_id)
    press(walk.cdp, "j")
    keys_row = one_lit(walk, "j off the long message")
    mouse_move(walk, float(box["x"]), float(box["y"]))
    one_lit(walk, "the mouse back on the long message", f"discord-log:{long_id}")
    folded = "(id) => document.querySelector(`#discord-log > li[data-id='${id}']`).dataset.collapsed"
    press(walk.cdp, "Enter")
    walk.settled()
    check(walk.page.evaluate(folded, long_id) == "false", "Enter did not open the message the mouse selected")
    one_lit(walk, "Enter on the mouse's message", f"discord-log:{long_id}")
    walk.shot("single-enter-on-mouse-selection")
    press(walk.cdp, "Enter")
    walk.settled()
    check(walk.page.evaluate(folded, long_id) == "true", "Enter again did not close it")
    check(keys_row != f"discord-log:{long_id}", "the keys and the mouse were on the same message; Enter proved nothing")

    # A control a click focused gives the focus up to the message the mouse moves to. Found in review:
    # Show read (or Summarise), clicked, kept the focus with no ring (a click's focus is not
    # `:focus-visible`), the mouse lit a message ring and all, and Enter pressed the button again, the
    # lit message untouched. Three clicks, Collapse read, Hide read and Show read again, so the list is
    # as it was and only the focus is left behind on the button.
    press(walk.cdp, "j")
    keys_row = one_lit(walk, "j off the long message, before the clicks on Show read")
    mode = "() => document.getElementById('todo-filter').getAttribute('aria-pressed')"
    mode_was = walk.page.evaluate(mode)
    for _ in range(3):
        centre = walk.page.evaluate("() => { const r = document.getElementById('todo-filter').getBoundingClientRect();"
                                    " return {x: r.left + r.width / 2, y: r.top + r.height / 2, w: r.width}; }")
        check(float(centre["w"]) > 0, f"Show read is not on screen to be clicked: {centre}")
        mouse_move(walk, float(centre["x"]), float(centre["y"]))
        mouse_click(walk, float(centre["x"]), float(centre["y"]))
        walk.settled()
    check(walk.page.evaluate(mode) == mode_was, f"three clicks on Show read left it {walk.page.evaluate(mode)!r}, not {mode_was!r}")
    held = walk.page.evaluate("() => document.activeElement && (document.activeElement.id || document.activeElement.tagName)")
    check(held == "todo-filter", f"the clicks left the focus on {held!r}, not on Show read; this step proves nothing")
    # The redraws may have moved the list: the long message back into view, by script, which moves no focus.
    box = walk.page.evaluate("(id) => { const li = document.querySelector(`#discord-log > li[data-id='${id}']`);"
                             " li.scrollIntoView({block: 'center'}); const r = li.getBoundingClientRect();"
                             " return {x: r.left + r.width * 0.35, y: r.top + 20}; }", long_id)
    walk.settled()
    check(lit_state(walk)["lit"] != [f"discord-log:{long_id}"], "the long message was lit before the mouse reached it; this proves nothing")
    mouse_move(walk, float(box["x"]), float(box["y"]))
    one_lit(walk, "the mouse onto the long message after a click on Show read", f"discord-log:{long_id}")
    press(walk.cdp, "Enter")
    walk.settled()
    check(walk.page.evaluate(folded, long_id) == "false",
          "Enter after a click on Show read did not open the message the mouse lit")
    check(walk.page.evaluate(mode) == mode_was, "Enter after a click on Show read pressed Show read again")
    one_lit(walk, "Enter on the mouse's message after a click on Show read", f"discord-log:{long_id}")
    walk.shot("single-enter-after-dock-click")
    press(walk.cdp, "Enter")
    walk.settled()
    check(walk.page.evaluate(folded, long_id) == "true", "Enter again did not close it")
    check(keys_row != f"discord-log:{long_id}", "the keys were on the long message before the clicks; this proves nothing")
    # Off the list, for the steps that follow: the dock is not a row.
    walk.cdp.send("Input.dispatchMouseEvent", {"type": "mouseMoved", "x": 2, "y": 790, "button": "none", "buttons": 0})
    walk.notes.append("one selection (#239 single-selection): a real mouse move took it from the keys, j and ArrowUp took it back with "
                      "the hovered row unlit, a click with no move selected, 14 j's scrolled the list under a still "
                      "mouse without it taking anything, and Enter opened the message the mouse had moved to, also "
                      "after a click on Show read had focused it; one row drawn lit, holding the focus, after every step")


def two_columns_single(walk: Walk) -> None:
    """`#239 single-selection` x `#229 desktop-two-column`: one lit message across both columns, the mouse choosing the column."""
    walk.page.click("#column-toggle")
    walk.page.wait_for_function("() => document.getElementById('side-column').getClientRects().length > 0", timeout=5000)
    walk.page.wait_for_selector("#side-cards > li[data-id]", state="visible", timeout=10_000)
    left = spot(walk, "discord-log", 1)
    mouse_move(walk, *xy(left))
    one_lit(walk, "two columns, the mouse over the left one", str(left["id"]))
    card = spot(walk, "side-cards", 0)
    mouse_move(walk, *xy(card))
    one_lit(walk, "the mouse into the right column, over a card", str(card["id"]))
    walk.shot("single-two-columns-card")
    press(walk.cdp, "[")
    one_lit(walk, "[ back to the left column", str(left["id"]))
    press(walk.cdp, "]")
    one_lit(walk, "] to the right column", str(card["id"]))
    mouse_move(walk, *xy(left))
    one_lit(walk, "the mouse back over the left column", str(left["id"]))
    walk.page.click("#column-toggle")
    walk.page.wait_for_function("() => document.getElementById('side-column').getClientRects().length === 0", timeout=5000)
    walk.notes.append("two columns: the mouse into either column took the one selection there, [ and ] handed it "
                      "across, and one row was drawn lit throughout")


PICKER_JS = "() => document.getElementById('discord-channel').value"


def picker_and_help_keys(walk: Walk) -> None:
    """Ctrl+K and a channel chosen gives the keys back to the list; Esc leaves the picker; Help's Back after "?"."""
    opened = walk.key("Control+k")
    check(opened["focus"] == "discord-channel", f"Ctrl+K left the focus on {opened['focus']!r}, not the channel picker")
    check(walk.last_key()["prevented"] is True, "Ctrl+K was left to the browser")
    left = walk.key("Escape")
    if left["focus"] == "discord-channel":
        # The first Esc closed the picker's own popup, which Ctrl+K opened: that one never reaches the
        # page. The next, on the closed picker, is the page's.
        left = walk.key("Escape")
    check(on_list(left), f"Esc on the channel picker left the focus on {left['focus']!r}, not the list")
    check(walk.page.evaluate(PICKER_JS) == CHANNEL["id"], "Esc on the channel picker changed the channel")

    walk.key("Control+k")
    walk.page.select_option("#discord-channel", str(SECOND["id"]))
    walk.page.wait_for_selector('#discord-log > li[data-id]', state="visible", timeout=10_000)
    chosen = walk.settled()
    check(walk.page.evaluate(PICKER_JS) == SECOND["id"], "the channel chosen from Ctrl+K is not the one shown")
    check(on_list(chosen), f"after a channel was chosen from Ctrl+K the focus is on {chosen['focus']!r}, not the list")
    first = ringed(walk, "Home", "Home after choosing a channel")
    row = ringed(walk, "j", "j after choosing a channel")
    check(row["id"] != first["id"], f"j after choosing a channel did not move the ring from {first['id']}")
    back = ringed(walk, "ArrowUp", "ArrowUp after choosing a channel")
    check(back["id"] == first["id"], f"ArrowUp after choosing a channel selected {back['id']}, not {first['id']}")
    check(walk.page.evaluate(PICKER_JS) == SECOND["id"], "ArrowUp after choosing a channel chose another channel")
    walk.shot("picker-chosen")

    # "?" opens Help over the messages, and "," leaves it; then Help from a group's "?" in Settings
    # goes back to Settings, as it always has.
    helped = walk.key("Shift+?", "screen-help")
    check(helped["screen"] == "help", f'"?" did not open Help: {helped}')
    left_help = walk.key(",")
    check(left_help["screen"] == "main", f'"," on Help did not leave it: {left_help}')
    walk.page.click("#open-settings")
    walk.page.click("#help-link-keyboard")
    walk.page.click("#close-help")
    again = walk.settled()
    check(again["screen"] == "settings", f'after "?" and ",", Back on Help from Settings went to {again["screen"]!r}')
    walk.key("Escape")
    walk.notes.append("Ctrl+K opened the channel picker, Esc left it for the list, a channel chosen from it gave the "
                      'keys back to the list (j and ArrowUp moved the ring, not the channel), and Help left by "," '
                      "still went back to Settings from Settings")


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

    api = FakeApi()
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(api, args.web_root.resolve()))
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    url = f"http://127.0.0.1:{server.server_port}/voice"
    first_row = '#discord-log > li[data-id]'

    try:
        with sync_playwright() as playwright, tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-") as profile:
            context = playwright.chromium.launch_persistent_context(
                profile, headless=True, executable_path=args.browser_executable,
                viewport={"width": 1280, "height": 800},
            )
            page = context.pages[0]
            errors: list[str] = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            cdp = context.new_cdp_session(page)
            walk = Walk(page, cdp, args.screenshots)

            # Sign in and open the channel once, then a real reload: the page comes back on the
            # channel by itself, and nothing on it is touched before the first key.
            page.goto(url, wait_until="load")
            page.fill("#api-token", TOKEN)
            page.click("#save-token")
            page.wait_for_selector("#view-switch", state="visible", timeout=10_000)
            page.click("#view-switch")
            page.wait_for_selector(first_row, state="visible", timeout=10_000)
            page.reload(wait_until="load")
            page.wait_for_selector(first_row, state="visible", timeout=10_000)
            page.evaluate(RECORD_KEYS_JS)
            loaded = walk.settled()
            check(loaded["screen"] == "main" and loaded["view"] == "discord",
                  f"the reload did not come back on the channel: {loaded}")
            walk.shot("loaded")
            scroll_keys(walk, "on load")

            # A click on the view switch and back: the switch must not keep the focus, or Space
            # presses it again.
            page.click("#view-switch")
            page.wait_for_function("() => document.getElementById('view-switch').getAttribute('aria-checked') === 'false'")
            page.click("#view-switch")
            page.wait_for_selector(first_row, state="visible", timeout=10_000)
            switched = walk.settled()
            check(on_list(switched), f"after a view switch the focus is on {switched['focus']!r}")
            scroll_keys(walk, "after a view switch")

            search_keys(walk)
            write_keys(walk)
            held_send_key(walk, api)
            call_view_write_keys(walk)
            settings_keys(walk)
            composer_keys(walk)
            keymap_keys(walk, api)
            single_selection(walk, api)
            picker_and_help_keys(walk)
            two_columns_single(walk)

            check(not errors, f"the page threw: {errors}")
            browser_version = context.browser.version if context.browser else "Chromium"
            context.close()

        print(f"{browser_version} at 1280x800, keys through CDP Input.dispatchKeyEvent: " + "; ".join(walk.notes))
        return 0
    finally:
        api.stopping.set()
        server.shutdown()
        server.server_close()
        thread.join()


if __name__ == "__main__":
    raise SystemExit(main())
