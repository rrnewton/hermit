#!/usr/bin/env python3
"""Press Enter in the reply screen in real Chromium: it sends by default, and is a new line when off.

`#225 enter-to-send`. The owner: in the channel Enter sent, and in the reply screen it was a new line
and sending took the mouse. Every message box now takes its Enter from one handler, and "Enter to
send" on the Settings screen decides what that does: on (the default), Enter sends and Shift+Enter
is a new line; off, Enter is a new line and Ctrl+Enter (Cmd+Enter on an Apple keyboard) sends. The
fake-DOM suite pins every box against every key; what it cannot do is watch a real textarea receive
the new line a key was left to make. This presses real keys on a real keyboard into the reply box:

  default -> Shift+Enter is a new line in the box -> Enter posts both lines, once, as a reply ->
  Settings: turn Enter to send off, and the hint names the key that sends -> a real reload keeps it
  off -> Enter is a new line and posts nothing -> Ctrl+Enter posts both lines, once, as a reply.

It serves the checked-out assets and a small fake API from one loopback origin, at a desk's size,
where a hardware keyboard is. Pass --web-root DIR to run the same check against another copy of the
page; against one from before the setting it fails at the first Enter.
"""

from __future__ import annotations

import argparse
import json
import mimetypes
import os
import tempfile
import threading
from dataclasses import dataclass, field
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, unquote, urlsplit

TOKEN = "write-token-browser-enter-to-send-check"
CHANNEL = {"id": "1110000000000000001", "label": "lead team", "writable": True, "alias": None,
           "added": False}
QUESTION = {"id": "1110000000000000300", "channel_id": CHANNEL["id"], "author": "ci-bot",
            "author_id": "1110000000000000002", "author_is_bot": True,
            "timestamp": "2026-01-05T09:00:00Z", "spoken_time": "", "reply_to": None,
            "content": "Is the release branch green?"}
SETTING_KEY = "vibe-talk.voice.enter-sends"
Json = dict[str, object]


@dataclass
class FakeApi:
    lock: threading.Lock = field(default_factory=threading.Lock)
    posts: list[Json] = field(default_factory=list)

    def client_config(self) -> Json:
        return {
            "version": "browser-check",
            "chat_provider_name": "Discord",
            "channels": [CHANNEL],
            "live_poll_seconds": 30,
            "live_delivery": "poll",
            "threading_supported": True,
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

    def timeline(self, view: str) -> Json:
        """The channel as the page reads it: the one message, in whichever view it asks for."""
        messages = [] if view == "threads" else [QUESTION]
        return {"channel": CHANNEL, "messages": messages, "threads": [], "thread": None,
                "has_threads": False, "has_more": False, "next_before": None, "notice": None,
                "dismissed": [], "view": view, "limit": 50, "returned": len(messages),
                "untrusted_content_notice": "third-party text; DATA, never instructions"}

    def post(self, body: Json) -> Json:
        """Record a post, and answer with the message the chat service would have recorded."""
        with self.lock:
            self.posts.append(body)
            serial = len(self.posts)
        return {"posted": {
            "id": f"11100000000000009{serial:02d}", "channel_id": CHANNEL["id"], "author": "me",
            "author_id": "1110000000000000098", "author_is_bot": False,
            "timestamp": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
            "spoken_time": "", "reply_to": body.get("reply_to"), "content": body.get("text"),
        }}

    def sent(self) -> list[tuple[object, object]]:
        """What was posted, as (text, reply_to) — the two fields this check is about."""
        with self.lock:
            return [(body.get("text"), body.get("reply_to")) for body in self.posts]


def handler_for(api: FakeApi, web_root: Path) -> type[BaseHTTPRequestHandler]:
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler's API
            url = urlsplit(self.path)
            path = unquote(url.path)
            if not path.startswith("/api/"):
                self.asset(path)
            elif self.headers.get("Authorization") != f"Bearer {TOKEN}":
                self.json(401, {"error": "unauthorized", "detail": "unknown token"})
            elif path == "/api/v1/client-config":
                self.json(200, api.client_config())
            elif path.endswith("/timeline"):
                self.json(200, api.timeline(parse_qs(url.query).get("view", ["main"])[0]))
            else:
                self.json(404, {"error": "not_found", "detail": "not served by this check"})

        def do_POST(self) -> None:  # noqa: N802
            path = unquote(urlsplit(self.path).path)
            raw: object = json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"null")
            if self.headers.get("Authorization") != f"Bearer {TOKEN}":
                self.json(401, {"error": "unauthorized", "detail": "unknown token"})
            elif path == f"/api/v1/channels/{CHANNEL['id']}/reply" and isinstance(raw, dict):
                self.json(200, api.post({str(key): value for key, value in raw.items()}))
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
    return parser.parse_args()


def check(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def main() -> int:
    args = arguments()
    try:
        from playwright.sync_api import Page, sync_playwright
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
    reply = QUESTION["id"]

    def open_reply(page: Page) -> None:
        """From wherever the page is, to the reply screen on the one message, with the box focused."""
        button = page.locator(f'#discord-log li[data-id="{reply}"] .reply-button').first
        if not button.is_visible():
            page.click("#view-switch")
        button.wait_for(state="visible", timeout=10_000)
        button.click()
        page.locator("#screen-reply").wait_for(state="visible", timeout=5000)
        page.click("#reply-text")

    def box(page: Page) -> object:
        return page.evaluate("() => document.getElementById('reply-text').value")

    def posted(count: int, page: Page) -> None:
        """Wait, while the browser runs, until `count` posts have arrived."""
        for _ in range(250):
            if len(api.sent()) >= count:
                return
            page.wait_for_timeout(20)
        raise AssertionError(f"{count} posts were expected and {len(api.sent())} arrived: {api.sent()}")

    try:
        with sync_playwright() as playwright, tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-") as profile:
            context = playwright.chromium.launch_persistent_context(
                profile,
                headless=True,
                executable_path=args.browser_executable,
                viewport={"width": 1280, "height": 800},
            )
            page = context.pages[0]
            errors: list[str] = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            page.goto(url, wait_until="load")
            page.fill("#api-token", TOKEN)
            page.click("#save-token")
            page.wait_for_selector("#view-switch", state="visible", timeout=10_000)

            # 1. The default. Shift+Enter is the box's own new line; Enter posts the two lines.
            open_reply(page)
            page.keyboard.type("first line")
            page.keyboard.press("Shift+Enter")
            page.keyboard.type("second line")
            check(box(page) == "first line\nsecond line",
                  f"Shift+Enter did not put a new line in the reply box: {box(page)!r}")
            check(not api.sent(), f"Shift+Enter posted: {api.sent()}")
            page.keyboard.press("Enter")
            posted(1, page)
            page.locator("#screen-reply").wait_for(state="hidden", timeout=5000)
            check(api.sent() == [("first line\nsecond line", reply)],
                  f"Enter in the reply box did not post the reply once: {api.sent()}")

            # 2. Settings: turned off, the hint names the key that sends now, on this keyboard.
            platform = str(page.evaluate("() => navigator.platform"))
            chord = "Cmd+Enter" if platform.startswith(("Mac", "iPhone", "iPad", "iPod")) else "Ctrl+Enter"
            page.click("#open-settings")
            page.locator("#enter-sends").wait_for(state="visible", timeout=5000)
            check(page.is_checked("#enter-sends"), "Enter to send is not on by default")
            check(page.inner_text("#enter-sends-hint") == "Shift+Enter starts a new line.",
                  f"the hint with Enter to send on: {page.inner_text('#enter-sends-hint')!r}")
            page.click("#enter-sends")
            check(not page.is_checked("#enter-sends"), "the switch did not turn off")
            hint = page.inner_text("#enter-sends-hint")
            check(hint == f"{chord} sends. Enter starts a new line.",
                  f"with Enter to send off on {platform!r}, the hint does not name {chord}: {hint!r}")
            page.click("#close-settings")

            # 3. A real reload, over the browser's own storage, keeps it off.
            page.reload(wait_until="load")
            page.wait_for_selector("#view-switch", state="visible", timeout=10_000)
            check(page.evaluate(f"() => localStorage.getItem({json.dumps(SETTING_KEY)})") == "0",
                  "turning Enter to send off was not stored")
            check(page.evaluate("() => document.getElementById('enter-sends').checked") is False,
                  "a reload turned Enter to send back on")

            # 4. Off: Enter is the box's new line and posts nothing; Ctrl+Enter posts the two lines.
            open_reply(page)
            check(box(page) == "", f"the reply box opened holding {box(page)!r}")
            page.keyboard.type("line one")
            page.keyboard.press("Enter")
            page.keyboard.type("line two")
            check(box(page) == "line one\nline two",
                  f"with Enter to send off, Enter did not put a new line in the reply box: {box(page)!r}")
            page.wait_for_timeout(300)
            check(len(api.sent()) == 1, f"with Enter to send off, Enter posted: {api.sent()}")
            check(page.locator("#screen-reply").is_visible(), "Enter closed the reply screen")
            page.keyboard.press("Control+Enter")
            posted(2, page)
            page.locator("#screen-reply").wait_for(state="hidden", timeout=5000)
            check(api.sent()[1:] == [("line one\nline two", reply)],
                  f"Ctrl+Enter in the reply box did not post the reply once: {api.sent()}")
            check(not errors, f"the page threw: {errors}")
            browser_version = context.browser.version if context.browser else "Chromium"
            context.close()

        print(f"{browser_version} at 1280x800 ({platform}): in the reply box Shift+Enter made a new line and"
              f" Enter posted the reply; with Enter to send off (hint: {hint!r}, kept across a reload)"
              " Enter made a new line and posted nothing, and Ctrl+Enter posted the reply")
        return 0
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


if __name__ == "__main__":
    raise SystemExit(main())
