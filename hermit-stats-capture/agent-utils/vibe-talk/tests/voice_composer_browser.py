#!/usr/bin/env python3
"""Write to the voice agent from the box under the call's transcript, in real Chromium.

`#241 voice-composer`. The owner: "Type mode on voice agent takes away my Voice/Channel toggle ...
when I press Type to type to the agent... then the UI shifts and I lose the toggle. ... Can we just
have a message box at the BOTTOM of the scroll history?" The Type toggle is gone; the call's view
has the channel's composer metaphor, a box and a Send as the last thing in the scrolling list,
always there. The fake-DOM suite pins the send path; what it cannot do is lay the page out, so it
cannot say whether the switch and the box are on the screen, uncovered, while the box has the focus.
This lays it out, at a 412x915 Android phone and a 1280x800 desk:

  idle: the box and the switch on screen and uncovered -> a click into the box and typing open
  nothing (no voice session asked for, no socket, no microphone) -> on the phone, the on-screen
  keyboard's share of the screen taken away, as `interactive-widget=resizes-content` makes Chrome on
  Android do: the dock, the switch in it, and the focused box still on screen and uncovered -> Enter
  starts ONE typed conversation, the message goes once as a `vibe-talk-v1` prompt after
  `session_started`, and the reply lands under it, still without the microphone -> the switch goes
  to the channel and back with the conversation still open -> End chat; then a spoken call (Chromium's
  fake microphone): the box focused during it, the switch, Talk and Hang up on screen, and a message
  sent from it goes down the call's socket after the open audio segment is closed.

The keyboard is simulated by shrinking the viewport, which is exactly what the viewport's
`interactive-widget=resizes-content` asks Chrome on Android to do to the layout; headless Chromium
has no on-screen keyboard to open. The page suite pins that the viewport declares it.

It serves the checked-out assets and a small fake API from one loopback origin and routes the voice
WebSocket to a Playwright mock playing the deployed `vibe-talk-v1` protocol. Pass --web-root DIR to
run it against another copy of the page; against one with the Type toggle it fails at the first look
for the box.
"""

from __future__ import annotations

import argparse
import json
import mimetypes
import os
import re
import tempfile
import threading
from collections.abc import Callable
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import TYPE_CHECKING
from urllib.parse import unquote, urlsplit

if TYPE_CHECKING:
    from playwright.sync_api import BrowserType, Page, WebSocketRoute

TOKEN = "write-token-browser-voice-composer-check"
CHANNEL = {"id": "1110000000000000001", "label": "lead team", "writable": True, "alias": None,
           "added": False}
# A Pixel 7 in portrait, and the share of it a phone keyboard takes: about 330 CSS pixels for Gboard
# with its suggestion strip. What matters is that it is most of the lower half of the screen.
PHONE = ("phone", 412, 915, True)
DESK = ("desk", 1280, 800, False)
KEYBOARD_PX = 330
Json = dict[str, object]

# Every box is judged by its centre and by its corners being inside the viewport, and by what a
# finger at its centre would hit: the element itself, not the dock, a chip or the status pill.
ON_SCREEN_JS = """(id) => {
  const node = document.getElementById(id);
  if (!node) return `#${id} is not in the page`;
  const r = node.getBoundingClientRect();
  const style = getComputedStyle(node);
  if (r.width <= 0 || r.height <= 0 || style.display === 'none' || style.visibility === 'hidden') {
    return `#${id} is not drawn`;
  }
  const w = document.documentElement.clientWidth, h = window.innerHeight;
  if (r.left < -0.5 || r.top < -0.5 || r.right > w + 0.5 || r.bottom > h + 0.5) {
    return `#${id} runs off the ${w}x${h} screen: ${Math.round(r.left)},${Math.round(r.top)} to ` +
      `${Math.round(r.right)},${Math.round(r.bottom)}`;
  }
  const x = r.left + r.width / 2, y = r.top + r.height / 2;
  const hit = document.elementFromPoint(x, y);
  if (!hit || !(hit === node || node.contains(hit))) {
    return `#${id} is covered at its centre by ${hit ? (hit.id ? '#' + hit.id : hit.tagName) : 'nothing'}`;
  }
  return '';
}"""

# The box can be taller than what is left above the keyboard: then what has to be on screen is
# where the reader types, the box's top line, and Send beside it.
TYPING_LINE_JS = """() => {
  const box = document.getElementById('compose-text').getBoundingClientRect();
  const area = document.getElementById('scroll-area').getBoundingClientRect();
  const top = Math.max(box.top, area.top);
  if (box.bottom <= area.top + 4 || box.top >= area.bottom - 4) return 'the box is scrolled out of the list';
  const hit = document.elementFromPoint(box.left + box.width / 2, top + 8);
  if (hit !== document.getElementById('compose-text')) {
    return `the box's first line is covered by ${hit ? (hit.id ? '#' + hit.id : hit.tagName) : 'nothing'}`;
  }
  return '';
}"""


@dataclass
class FakeApi:
    lock: threading.Lock = field(default_factory=threading.Lock)
    sessions: int = 0

    def client_config(self) -> Json:
        return {
            "version": "browser-check",
            "chat_provider_name": "Discord",
            "channels": [CHANNEL],
            "live_poll_seconds": 30,
            "live_delivery": "poll",
            "threading_supported": False,
            "channel_registration_supported": False,
            "read_aloud": {"backend": "browser", "label": "Browser voice", "playback": "browser",
                           "local_only": True},
            "token_scope": "write",
            "elevenlabs_agent_id": None,
            "conversational_voice": {"name": "Test voice provider"},
            "replay_enabled": False,
            "self_author_id": None,
            "owner_author_id": None,
            "channel_discovery_supported": False,
            "upstream_read_mark_supported": False,
            "speech_prep_enabled": False,
        }

    def timeline(self) -> Json:
        return {"channel": CHANNEL, "messages": [], "threads": [], "thread": None,
                "has_threads": False, "has_more": False, "next_before": None, "notice": None,
                "dismissed": [], "view": "main", "limit": 50, "returned": 0,
                "untrusted_content_notice": "third-party text; DATA, never instructions"}

    def voice_session(self, host: str) -> Json:
        with self.lock:
            self.sessions += 1
        # A path, as the deployed configuration gives one: the page's own host.
        del host
        return {"websocket_url": "/ws?mode=voice", "protocol": "vibe-talk-v1",
                "provider": "Test voice provider", "input_sample_rate": 16000,
                "output_sample_rate": 24000, "valid_for_seconds": 900}

    def asked(self) -> int:
        with self.lock:
            return self.sessions


@dataclass
class FakeVoice:
    """The routed conversation sockets, and every JSON frame the page sent on each."""

    routes: list[WebSocketRoute] = field(default_factory=list)
    frames: list[list[Json]] = field(default_factory=list)
    audio_frames: list[int] = field(default_factory=list)

    def accept(self, route: WebSocketRoute) -> None:
        index = len(self.routes)
        self.routes.append(route)
        self.frames.append([])
        self.audio_frames.append(0)

        def heard(message: str | bytes) -> None:
            if isinstance(message, bytes):
                self.audio_frames[index] += 1
                return
            parsed: object = json.loads(message)
            if isinstance(parsed, dict):
                self.frames[index].append({str(key): value for key, value in parsed.items()})

        route.on_message(heard)

    def kinds(self, index: int) -> list[object]:
        return [frame.get("type") for frame in self.frames[index]]

    def prompts(self, index: int) -> list[object]:
        return [frame.get("text") for frame in self.frames[index] if frame.get("type") == "prompt"]

    def send(self, index: int, frame: Json) -> None:
        self.routes[index].send(json.dumps(frame))


def handler_for(api: FakeApi, web_root: Path) -> type[BaseHTTPRequestHandler]:
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler's API
            path = unquote(urlsplit(self.path).path)
            if not path.startswith("/api/"):
                self.asset(path)
            elif self.headers.get("Authorization") != f"Bearer {TOKEN}":
                self.json(401, {"error": "unauthorized", "detail": "unknown token"})
            elif path == "/api/v1/client-config":
                self.json(200, api.client_config())
            elif path == "/api/v1/voice-session":
                self.json(200, api.voice_session(str(self.headers.get("Host"))))
            elif path == "/api/v1/post-proposals":
                self.json(200, {"proposal": None})
            elif path.endswith("/timeline"):
                self.json(200, api.timeline())
            else:
                self.json(404, {"error": "not_found", "detail": "not served by this check"})

        def do_POST(self) -> None:  # noqa: N802
            self.rfile.read(int(self.headers.get("Content-Length") or 0))
            if self.headers.get("Authorization") != f"Bearer {TOKEN}":
                self.json(401, {"error": "unauthorized", "detail": "unknown token"})
            else:
                # Timing, health and the transcript record: accepted and dropped. Nothing here is
                # about them.
                self.json(204, {})

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
            body = b"" if status == 204 else json.dumps(payload).encode()
            self.send_response(status)
            if status != 204:
                self.send_header("Content-Type", "application/json")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            try:
                self.wfile.write(body)
            except OSError:
                pass

        def log_message(self, _format: str, *_args: object) -> None:
            pass

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
        default=Path(__file__).resolve().parents[1] / "web",
        help="the page assets to serve (default: this checkout's vibe-talk/web)",
    )
    parser.add_argument(
        "--screenshots",
        type=Path,
        help="keep a PNG of each profile's focused box here, the phone's with its keyboard simulated "
             "(default: none kept)",
    )
    return parser.parse_args()


def check(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def until(page: Page, condition: Callable[[], bool], what: str, timeout_ms: int = 10_000) -> None:
    """Wait, while the browser runs (so routed sockets are served), until `condition()` holds."""
    for _ in range(timeout_ms // 20):
        if condition():
            return
        page.wait_for_timeout(20)
    raise AssertionError(f"timed out waiting for {what}")


# The status pill floats over the foot of the list with `pointer-events: none`, so a tap passes
# through it and `elementFromPoint` does not see it; whether it is DRAWN over the box or Send is a
# separate question, asked here of the boxes themselves.
CLEAR_OF_PILL_JS = """(id) => {
  const pill = document.getElementById('status-line');
  if (!pill || pill.hidden) return '';
  const p = pill.getBoundingClientRect(), r = document.getElementById(id).getBoundingClientRect();
  const overlap = Math.min(p.right, r.right) - Math.max(p.left, r.left) > 0.5 &&
    Math.min(p.bottom, r.bottom) - Math.max(p.top, r.top) > 0.5;
  return overlap ? `the status pill is drawn over #${id}` : '';
}"""


def on_screen(page: Page, where: str, *ids: str) -> None:
    for element_id in ids:
        problem = page.evaluate(ON_SCREEN_JS, element_id)
        check(problem == "", f"{where}: {problem}")
        if element_id in ("send-text", "compose-text"):
            problem = page.evaluate(CLEAR_OF_PILL_JS, element_id)
            check(problem == "", f"{where}: {problem}")


def focused_id(page: Page) -> object:
    return page.evaluate("() => document.activeElement && document.activeElement.id")


def walk(chromium: BrowserType, args: argparse.Namespace, url: str, api: FakeApi,
         profile: tuple[str, int, int, bool]) -> str:
    """One profile's walk. Answers a line for the summary."""
    label, width, height, mobile = profile
    voice = FakeVoice()
    with tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-composer-") as directory:
        context = chromium.launch_persistent_context(
            directory,
            headless=True,
            executable_path=args.browser_executable,
            args=["--use-fake-ui-for-media-stream", "--use-fake-device-for-media-stream"],
            viewport={"width": width, "height": height},
            is_mobile=mobile,
            has_touch=mobile,
            device_scale_factor=2.625 if mobile else 1,
        )
        try:
            origin = url.rsplit("/voice", 1)[0]
            context.grant_permissions(["microphone"], origin=origin)
            context.route_web_socket(re.compile(r"/ws\?mode=voice$"), voice.accept)
            # Counted from the page's own realm, before a line of it runs: the microphone is the one
            # thing a typed conversation promises never to ask for.
            context.add_init_script(
                "(() => { window.__micAsks = 0; const md = navigator.mediaDevices;"
                " const real = md.getUserMedia.bind(md);"
                " md.getUserMedia = (c) => { window.__micAsks += 1; return real(c); }; })()"
            )
            page = context.pages[0]
            errors: list[str] = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            page.goto(url, wait_until="load")
            page.fill("#api-token", TOKEN)
            page.click("#save-token")
            page.wait_for_selector("#view-switch", state="visible", timeout=10_000)
            check(page.evaluate("() => !document.getElementById('pane-voice').hidden") is True,
                  f"{label}: the page did not open on the call's view")
            check(page.locator("#text-entry").count() == 0, f"{label}: the Type toggle is still on the page")
            viewport = str(page.evaluate("() => document.querySelector('meta[name=viewport]').content"))
            check("interactive-widget=resizes-content" in viewport,
                  f"{label}: the viewport does not ask for the content to be resized under the keyboard")

            # 1. Idle. The box and the switch, on screen and uncovered, and the box the last thing
            #    in the list rather than below the fold of an empty transcript.
            on_screen(page, f"{label}, idle", "compose-text", "send-text", "view-switch", "talk")

            # 2. A click into the box, and typing, open nothing.
            page.click("#compose-text")
            page.keyboard.type("what landed overnight")
            page.wait_for_timeout(300)
            check(focused_id(page) == "compose-text", f"{label}: a click in the box left the focus on {focused_id(page)!r}")
            check(api.asked() == 0, f"{label}: focusing or typing in the box asked for a voice session")
            check(not voice.routes, f"{label}: focusing or typing in the box opened a conversation")
            check(page.evaluate("() => window.__micAsks") == 0, f"{label}: the box asked for the microphone")
            on_screen(page, f"{label}, the box focused", "view-switch", "send-text", "open-settings")
            check(page.evaluate(TYPING_LINE_JS) == "", f"{label}, the box focused: {page.evaluate(TYPING_LINE_JS)}")

            keyboard = ""
            if mobile:
                # 3. The on-screen keyboard, as `resizes-content` has Chrome lay it out: the layout
                #    is what is left above the keys. The dock rides above them with the switch in
                #    it, and the box the reader is typing in is still on screen.
                page.set_viewport_size({"width": width, "height": height - KEYBOARD_PX})
                page.wait_for_timeout(300)
                where = f"{label} with a {KEYBOARD_PX}px keyboard up"
                check(focused_id(page) == "compose-text", f"{where}: the box lost the focus")
                on_screen(page, where, "view-switch", "send-text", "talk")
                problem = page.evaluate(TYPING_LINE_JS)
                check(problem == "", f"{where}: {problem}")
                dock = page.evaluate("() => document.getElementById('dock').getBoundingClientRect().bottom")
                check(float(dock) <= height - KEYBOARD_PX + 0.5, f"{where}: the dock runs under the keyboard ({dock})")
                if args.screenshots:
                    args.screenshots.mkdir(parents=True, exist_ok=True)
                    page.screenshot(path=str(args.screenshots / f"{label}-keyboard.png"), animations="disabled")
                keyboard = f", and with a {KEYBOARD_PX}px keyboard up the dock, the switch and the box stayed on screen"
                page.set_viewport_size({"width": width, "height": height})
                page.wait_for_timeout(200)
            elif args.screenshots:
                args.screenshots.mkdir(parents=True, exist_ok=True)
                page.screenshot(path=str(args.screenshots / f"{label}-focused.png"), animations="disabled")

            # 4. Enter: ONE typed conversation, and the message goes once it is open, as a prompt.
            page.focus("#compose-text")
            page.keyboard.press("Enter")
            until(page, lambda: len(voice.routes) == 1, "the first send to open a conversation")
            check(api.asked() == 1, f"{label}: the first send asked for {api.asked()} voice sessions")
            page.wait_for_function("() => document.getElementById('status-line').dataset.state === 'live'",
                                   timeout=10_000)
            check(voice.prompts(0) == [], f"{label}: a prompt went before the session started")
            # Still the reader's until it is on the wire: a bridge that never starts its session
            # must leave the message where it was written.
            check(page.input_value("#compose-text") == "what landed overnight",
                  f"{label}: the message left the box before the session started")
            voice.send(0, {"type": "session_started", "greeting": False})
            until(page, lambda: voice.prompts(0) == ["what landed overnight"], "the held message to go as a prompt")
            voice.send(0, {"type": "transcript", "role": "assistant", "turn": 1, "text": "Two merges and a revert."})
            voice.send(0, {"type": "turn_complete", "turn": 1})
            page.wait_for_function(
                "() => [...document.querySelectorAll('#transcript > li.theirs')]"
                ".some((li) => li.textContent.includes('Two merges'))", timeout=10_000)
            mine = page.evaluate("() => [...document.querySelectorAll('#transcript > li.mine')]"
                                 ".map((li) => li.textContent).filter((t) => t.includes('what landed overnight')).length")
            check(mine == 1, f"{label}: the sent message is in the transcript {mine} times")
            check(page.input_value("#compose-text") == "", f"{label}: the sent message stayed in the box")
            check(len(voice.routes) == 1 and api.asked() == 1, f"{label}: a second conversation was opened")
            check(voice.kinds(0) == ["prompt"], f"{label}: a typed call sent {voice.kinds(0)}, not one prompt")
            check(page.evaluate("() => window.__micAsks") == 0, f"{label}: the typed conversation asked for the microphone")
            check(voice.audio_frames[0] == 0, f"{label}: a typed conversation sent audio")
            page.click("#compose-text")
            on_screen(page, f"{label}, a typed conversation, the box focused", "view-switch", "send-text", "hang-up")

            # 4b. A conversation longer than the screen, where the box is in the list and can be half
            #     under the dock. A real tap on the part that shows (not a locator click, which would
            #     scroll it into view first) takes the whole box into view, Send included; on the
            #     phone the keyboard coming up keeps it there.
            for turn in range(2, 14):
                voice.send(0, {"type": "transcript", "role": "assistant", "turn": turn,
                               "text": f"Answer {turn}: " + "the nightly ran green on every target. " * 3})
                voice.send(0, {"type": "turn_complete", "turn": turn})
            page.wait_for_function("() => document.querySelectorAll('#transcript > li.theirs').length >= 13",
                                   timeout=10_000)
            page.evaluate("() => document.activeElement && document.activeElement.blur()")
            page.evaluate(
                "() => { const a = document.getElementById('scroll-area'); "
                "const r = a.getBoundingClientRect(); "
                "a.scrollTop += document.getElementById('compose-text').getBoundingClientRect().top - (r.bottom - 40); }")
            # Let what a scroll back brings up settle first — the jump to the newest message floats
            # over the list's lower left — and then tap the middle of the box's showing strip, after
            # checking that a finger there lands on the box and not on a chip.
            page.wait_for_timeout(400)
            half = page.evaluate(
                "() => { const r = document.getElementById('scroll-area').getBoundingClientRect(); "
                "const f = document.getElementById('compose-text').getBoundingClientRect(); "
                "const x = f.left + f.width / 2, y = Math.min(f.top + 12, r.bottom - 8); "
                "const hit = document.elementFromPoint(x, y); "
                "return [x, y, r.bottom, document.getElementById('send-text').getBoundingClientRect().top, "
                "hit ? (hit.id || hit.tagName) : '']; }")
            assert isinstance(half, list)
            x, y, area_bottom, send_top = (float(value) for value in half[:4])
            check(send_top > area_bottom, f"{label}: the premise, Send under the dock, does not hold")
            check(half[4] == "compose-text", f"{label}: the point to tap on the half-showing box is on {half[4]!r}")
            page.mouse.click(x, y)
            page.wait_for_timeout(200)
            where = f"{label}, a long typed conversation, the box tapped where it showed"
            check(focused_id(page) == "compose-text", f"{where}: the tap did not focus the box")
            on_screen(page, where, "send-text", "view-switch", "hang-up")
            if mobile:
                page.set_viewport_size({"width": width, "height": height - KEYBOARD_PX})
                page.wait_for_timeout(300)
                where = f"{label}, a long typed conversation, a {KEYBOARD_PX}px keyboard up"
                check(focused_id(page) == "compose-text", f"{where}: the box lost the focus")
                problem = page.evaluate(TYPING_LINE_JS)
                check(problem == "", f"{where}: {problem}")
                on_screen(page, where, "send-text", "view-switch", "hang-up")
                if args.screenshots:
                    page.screenshot(path=str(args.screenshots / f"{label}-keyboard-long.png"), animations="disabled")
                page.set_viewport_size({"width": width, "height": height})
                page.wait_for_timeout(200)

            # 5. The switch works from a typed conversation as it does from a call: the conversation
            #    goes on, and its end control with it.
            page.click("#view-switch")
            page.wait_for_function("() => !document.getElementById('pane-discord').hidden", timeout=5_000)
            on_screen(page, f"{label}, the channel during a typed conversation", "view-switch", "hang-up")
            page.click("#view-switch")
            page.wait_for_function("() => !document.getElementById('pane-voice').hidden", timeout=5_000)
            check(page.inner_text("#hangup-label") == "End chat", f"{label}: the typed conversation did not survive the switch")
            page.click("#hang-up")
            page.wait_for_function("() => !document.getElementById('talk').hidden", timeout=5_000)

            # 6. A spoken call, on Chromium's fake microphone. The box during it: the switch, Talk
            #    and Hang up all on screen while it has the focus, and a message from it goes down the
            #    call's socket once the open audio segment has been closed.
            page.click("#talk")
            until(page, lambda: len(voice.routes) == 2, "Talk to open a second conversation")
            page.wait_for_function("() => document.getElementById('status-line').dataset.state === 'live'",
                                   timeout=10_000)
            voice.send(1, {"type": "session_started", "greeting": False})
            until(page, lambda: "audio_start" in voice.kinds(1), "the call to open its audio segment")
            check(page.evaluate("() => window.__micAsks") == 1, f"{label}: the call did not ask for the microphone once")
            page.click("#compose-text")
            page.keyboard.type("and the nightly")
            where = f"{label}, a call, the box focused"
            on_screen(page, where, "view-switch", "talk", "hang-up", "send-text")
            check(page.inner_text("#hangup-label") == "Hang up", f"{where}: the call is not a spoken one")
            page.keyboard.press("Enter")
            until(page, lambda: "audio_end" in voice.kinds(1), "the call to close its audio segment for the message")
            voice.send(1, {"type": "turn_complete", "turn": 1})
            until(page, lambda: voice.prompts(1) == ["and the nightly"], "the message to go down the call's socket")
            check(len(voice.routes) == 2, f"{label}: a message during a call opened another conversation")
            check(api.asked() == 2, f"{label}: a message during a call asked for another voice session")
            on_screen(page, f"{where}, after sending", "view-switch", "talk", "hang-up")
            page.click("#hang-up")
            check(not errors, f"{label}: the page threw: {errors}")
        finally:
            context.close()
    return (f"{label} at {width}x{height}: the box and the switch on screen and uncovered at idle, focused,"
            f" in a typed conversation and in a call; focus and typing opened nothing{keyboard}; the first"
            " Enter opened one typed conversation whose message went once as a prompt, with no microphone;"
            " a message during a call went down its socket after the audio segment closed")


def main() -> int:
    args = arguments()
    try:
        from playwright.sync_api import sync_playwright
    except ImportError as error:
        raise SystemExit(
            "Python Playwright is required: python3 -m pip install --user playwright && "
            "python3 -m playwright install chromium"
        ) from error

    api = FakeApi()
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(api, args.web_root.resolve()))
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    url = f"http://127.0.0.1:{server.server_port}/voice"
    try:
        lines = []
        with sync_playwright() as playwright:
            for profile in (PHONE, DESK):
                api.sessions = 0
                lines.append(walk(playwright.chromium, args, url, api, profile))
        for line in lines:
            print(line)
        return 0
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


if __name__ == "__main__":
    raise SystemExit(main())
