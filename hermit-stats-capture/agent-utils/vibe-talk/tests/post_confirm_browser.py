#!/usr/bin/env python3
"""Tap Send on a post-confirmation card the moment it appears, in real Chromium, and post nothing.

`#49 post-confirmation-card-hold`. During a write-scope call the agent may propose a post, and the
page shows the server's copy on a card whose Send is the owner's confirmation. A card appears over
whatever was there, so a tap already on its way to a row or the transcript can land on Send before
the owner has read a word. Send is held for the page's POST_CHANGE_HOLD_MS when a card appears, as
it already was when one replaced another. The fake-DOM suite pins the branch; this proves a real
touch on the real button, laid out and enabled by a real engine, sends nothing during the hold.

It serves the checked-out assets and a small fake API from one loopback origin, routes the voice
WebSocket to an in-browser mock, opens a typed call (no microphone), proposes a post, and taps:

  card appears -> tap Send at once: nothing is committed -> Send enables by itself within the
  hold -> tap Send: the proposal is committed once, as the UI.

Pass --web-root DIR to run the same check against another copy of the page; against one without
the hold it fails at the first tap.
"""

from __future__ import annotations

import argparse
import json
import mimetypes
import os
import tempfile
import threading
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, unquote, urlsplit

TOKEN = "write-token-browser-post-confirm-check"
CHANNEL = {"id": "1110000000000000001", "label": "lead team", "writable": True, "alias": None,
           "added": False}
PROPOSAL = {"serial": 7, "handle": "hAnDlE-browser-check-0001", "channel_id": CHANNEL["id"],
            "channel_name": "lead team", "text": "on it, back in ten", "reply_to": None,
            "expires_in_ms": 120_000}
Json = dict[str, object]

# The page's own hold; the tap must land well inside it for its miss to mean anything.
HOLD_MS = 2000
TAP_WITHIN_MS = 1000


@dataclass
class FakeApi:
    stopping: threading.Event = field(default_factory=threading.Event)
    changed: threading.Condition = field(default_factory=threading.Condition)
    proposal: Json | None = None
    watches: int = 0
    commits: list[tuple[float, Json]] = field(default_factory=list)

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

    def serial(self) -> object:
        return None if self.proposal is None else self.proposal["serial"]

    def pending(self, wait: float, seen: int | None) -> Json:
        """The long poll: answer once the pending proposal is not the one `seen`, or after `wait` s."""
        with self.changed:
            self.watches += 1
            if wait > 0:
                self.changed.wait_for(lambda: self.serial() != seen or self.stopping.is_set(),
                                      timeout=min(wait, 1.0))
            return {"proposal": self.proposal}

    def propose(self, proposal: Json) -> None:
        with self.changed:
            self.proposal = proposal
            self.changed.notify_all()


def posted(proposal: Json) -> Json:
    return {"id": "1110000000000000099", "channel_id": proposal["channel_id"], "author": "bot",
            "author_id": "1110000000000000098", "author_is_bot": True,
            "timestamp": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
            "spoken_time": "", "reply_to": proposal["reply_to"], "content": proposal["text"]}


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
            elif path == "/api/v1/voice-session":
                host = self.headers.get("Host")
                self.json(200, {"websocket_url": f"ws://{host}/voice-socket", "protocol": "vibe-talk-v1",
                                "provider": "Test voice provider", "input_sample_rate": 16000,
                                "output_sample_rate": 24000, "valid_for_seconds": 900})
            elif path == "/api/v1/post-proposals":
                query = parse_qs(url.query)
                seen = query.get("seen")
                self.json(200, api.pending(float(query.get("wait", ["0"])[0]), int(seen[0]) if seen else None))
            elif path.endswith("/timeline"):
                self.json(200, api.timeline())
            else:
                self.json(404, {"error": "not_found", "detail": "not served by this check"})

        def do_POST(self) -> None:  # noqa: N802
            path = unquote(urlsplit(self.path).path)
            body = json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"null")
            if self.headers.get("Authorization") != f"Bearer {TOKEN}":
                self.json(401, {"error": "unauthorized", "detail": "unknown token"})
            elif path == "/api/v1/post-proposals/commit":
                with api.changed:
                    api.commits.append((time.monotonic(), body))
                    proposal = api.proposal
                    api.proposal = None
                    api.changed.notify_all()
                if proposal is None or body.get("handle") != proposal["handle"]:
                    self.json(409, {"error": "proposal_used", "detail": "already settled"})
                else:
                    message = posted(proposal)
                    self.json(200, {"parts": [message], "posted": message, "serial": proposal["serial"]})
            elif path == "/api/v1/voice-timing":
                self.json(204, {})
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
    return parser.parse_args()


def check(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


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
        with sync_playwright() as playwright, tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-") as profile:
            context = playwright.chromium.launch_persistent_context(
                profile,
                headless=True,
                executable_path=args.browser_executable,
                user_agent=(
                    "Mozilla/5.0 (Linux; Android 15; Pixel 7) AppleWebKit/537.36 "
                    "(KHTML, like Gecko) Chrome/151.0.0.0 Mobile Safari/537.36"
                ),
                viewport={"width": 412, "height": 915},
                device_scale_factor=2.625,
                is_mobile=True,
                has_touch=True,
            )
            # Never connected onward: the socket opens in the browser and stays quiet, which is all
            # a typed call needs to start watching for proposals.
            context.route_web_socket("**/voice-socket", lambda _socket: None)
            page = context.pages[0]
            errors: list[str] = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            page.goto(url, wait_until="load")
            page.fill("#api-token", TOKEN)
            page.click("#save-token")
            # A typed call, opened as the page opens one since `#241 voice-composer`: the first
            # message sent from the call's box with nothing open.
            page.fill("#compose-text", "anything for me to post?")
            page.click("#send-text")
            deadline = time.monotonic() + 10
            while api.watches < 2:
                check(time.monotonic() < deadline, "the typed call never started watching for proposals")
                # Not `time.sleep`: the socket route is served only while Playwright is dispatching.
                page.wait_for_timeout(20)

            api.propose(PROPOSAL)
            send = page.locator("#post-confirm-send")
            send.wait_for(state="visible", timeout=10_000)
            appeared = time.monotonic()
            box = send.bounding_box()
            check(box is not None, "Send has no box on screen")
            assert box is not None
            x, y = box["x"] + box["width"] / 2, box["y"] + box["height"] / 2
            # A touch that misses Send also posts nothing, which would make the check below vacuous.
            on_send = page.evaluate("([x, y]) => document.getElementById('post-confirm-send')"
                                    ".contains(document.elementFromPoint(x, y))", [x, y])
            check(on_send is True, f"the point tapped is not on Send: ({x:.0f}, {y:.0f})")
            # A real touch at Send's centre, not a locator click: that would wait for Send to enable.
            page.touchscreen.tap(x, y)
            tapped_ms = (time.monotonic() - appeared) * 1000
            check(tapped_ms < TAP_WITHIN_MS,
                  f"the first tap landed {tapped_ms:.0f} ms after the card appeared, too late to test the hold")
            page.wait_for_timeout(HOLD_MS // 2)
            check(not api.commits,
                  f"a tap {tapped_ms:.0f} ms after the card appeared posted text the owner had not read: "
                  f"{api.commits[0][1] if api.commits else None}")

            page.wait_for_function("() => !document.getElementById('post-confirm-send').disabled",
                                   timeout=HOLD_MS + 1000)
            enabled_ms = (time.monotonic() - appeared) * 1000
            # Loose: measured from this side seeing the card. The page suite pins the exact hold.
            check(enabled_ms >= HOLD_MS / 2, f"Send enabled {enabled_ms:.0f} ms after the card appeared")
            send.tap()
            page.locator("#post-confirm").wait_for(state="hidden", timeout=5000)
            commits = [body for _at, body in api.commits]
            check(commits == [{"handle": PROPOSAL["handle"], "channel_id": PROPOSAL["channel_id"],
                               "text": PROPOSAL["text"], "reply_to": PROPOSAL["reply_to"],
                               "confirmed_by": "ui"}],
                  f"Send after the hold did not commit the card once, as the UI: {commits}")
            check(not errors, f"the page threw: {errors}")
            browser_version = context.browser.version if context.browser else "Chromium"
            context.close()

        print(f"{browser_version} at 412x915: a tap {tapped_ms:.0f} ms after a first card appeared posted"
              f" nothing; Send enabled by itself at {enabled_ms:.0f} ms and then committed the card once")
        return 0
    finally:
        api.stopping.set()
        with api.changed:
            api.changed.notify_all()
        server.shutdown()
        server.server_close()
        thread.join()


if __name__ == "__main__":
    raise SystemExit(main())
