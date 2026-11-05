#!/usr/bin/env python3
"""Replay the voice bridge's event order on a spoken call, in real Chromium. `#268 voice-call-drops`.

The owner's calls ended on their own, and a spoken "Yes." after an exact read-back was not acted on:
the agent said "One moment." and nothing was sent until he tapped Send. The fake-DOM suite pins each
rule; what it cannot do is run the page's real WebSocket, real timers and real audio clock against
frames in the order a bridge sends them. This does, on Chromium's fake microphone, against a
Playwright mock of the deployed `vibe-talk-v1` protocol:

  a greeting: no microphone frame reaches the server straight after its `turn_complete`, and they
  resume after the hand-off -> the owner asks, the agent proposes and reads the draft back word for
  word -> the owner's "Yes" arrives as a hypothesis, then the agent's reply "One moment." with its
  audio, then the final "Yes." -> the card is committed once, as the speaker's turn -> the page's
  notice does not close the microphone's segment while that turn is open, closes it once the hand-off
  after the turn's completion has passed, and goes as a prompt at the segment's boundary -> the server
  closes the call: the page says so in red, that the reader did not hang up, and records
  `server_closed` with code 1000 -> a second call, hung up: one `audio_end` and one `quit`, recorded
  as `hang_up`.

It serves the checked-out assets and a small fake API from one loopback origin. Pass --web-root DIR
to run it against another copy of the page; against the page before `#268` it fails at the greeting
(microphone frames straight after its `turn_complete`) or, past that, at the yes.
"""

from __future__ import annotations

import argparse
import json
import mimetypes
import os
import re
import tempfile
import threading
import time
from array import array
from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import TYPE_CHECKING
from urllib.parse import parse_qs, unquote, urlsplit

if TYPE_CHECKING:
    from playwright.sync_api import Page, WebSocketRoute

TOKEN = "write-token-browser-voice-call-drops-check"
CHANNEL = {"id": "1110000000000000001", "label": "lead team", "writable": True, "alias": None,
           "added": False}
PROPOSAL = {"serial": 7, "handle": "hAnDlE-browser-call-drops-01", "channel_id": CHANNEL["id"],
            "channel_name": "lead team", "text": "on it, rebasing now", "reply_to": None,
            "expires_in_ms": 120_000}
READ_BACK = "To lead team: on it, rebasing now. Send it?"
OUTPUT_RATE = 24_000
# The page's capture hand-off after a `turn_complete` is 750 ms; nothing may arrive well inside it.
HAND_OFF_QUIET_MS = 400
Json = dict[str, object]


def speech(seconds: float) -> bytes:
    """`seconds` of an audible square wave, as the protocol's 24 kHz 16-bit little-endian mono PCM."""
    samples = array("h", (4000 if (i // 20) % 2 == 0 else -4000 for i in range(int(OUTPUT_RATE * seconds))))
    return samples.tobytes()


@dataclass
class FakeApi:
    stopping: threading.Event = field(default_factory=threading.Event)
    changed: threading.Condition = field(default_factory=threading.Condition)
    proposal: Json | None = None
    commits: list[Json] = field(default_factory=list)
    ends: list[Json] = field(default_factory=list)

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
            if wait > 0:
                self.changed.wait_for(lambda: self.serial() != seen or self.stopping.is_set(),
                                      timeout=min(wait, 1.0))
            return {"proposal": self.proposal}

    def propose(self, proposal: Json) -> None:
        with self.changed:
            self.proposal = proposal
            self.changed.notify_all()

    def commit(self, body: Json) -> Json | None:
        """Spend the pending proposal; the message posted, or None when there was none to spend."""
        with self.changed:
            self.commits.append(body)
            proposal = self.proposal
            self.proposal = None
            self.changed.notify_all()
        if proposal is None or body.get("handle") != proposal["handle"]:
            return None
        return {"id": "1110000000000000099", "channel_id": proposal["channel_id"], "author": "bot",
                "author_id": "1110000000000000098", "author_is_bot": True,
                "timestamp": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
                "spoken_time": "", "reply_to": proposal["reply_to"], "content": proposal["text"]}

    def recorded_ends(self) -> list[Json]:
        with self.changed:
            return list(self.ends)


@dataclass
class FakeVoice:
    """The routed conversation sockets, every JSON frame the page sent on each, and when audio came."""

    routes: list[WebSocketRoute] = field(default_factory=list)
    frames: list[list[Json]] = field(default_factory=list)
    frames_at: list[list[float]] = field(default_factory=list)
    audio_at: list[list[float]] = field(default_factory=list)

    def accept(self, route: WebSocketRoute) -> None:
        index = len(self.routes)
        self.routes.append(route)
        self.frames.append([])
        self.frames_at.append([])
        self.audio_at.append([])

        def heard(message: str | bytes) -> None:
            if isinstance(message, bytes):
                self.audio_at[index].append(time.monotonic())
                return
            parsed: object = json.loads(message)
            if isinstance(parsed, dict):
                self.frames[index].append({str(key): value for key, value in parsed.items()})
                self.frames_at[index].append(time.monotonic())

        route.on_message(heard)

    def kinds(self, index: int) -> list[object]:
        return [frame.get("type") for frame in self.frames[index]]

    def first_at(self, index: int, kind: str) -> float:
        """When the first JSON frame of `kind` arrived on socket `index`, by the monotonic clock."""
        return next(at for frame, at in zip(self.frames[index], self.frames_at[index], strict=True)
                    if frame.get("type") == kind)

    def prompts(self, index: int) -> list[str]:
        return [str(frame.get("text")) for frame in self.frames[index] if frame.get("type") == "prompt"]

    def send(self, index: int, frame: Json) -> None:
        self.routes[index].send(json.dumps(frame))

    def say(self, index: int, role: str, turn: int, text: str, final: bool = True) -> None:
        self.send(index, {"type": "transcript", "role": role, "turn": turn, "text": text, "final": final})

    def audio(self, index: int, seconds: float) -> None:
        self.routes[index].send(speech(seconds))


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
                # A path, as the deployed configuration gives one: the page's own host.
                self.json(200, {"websocket_url": "/ws?mode=voice", "protocol": "vibe-talk-v1",
                                "provider": "Test voice provider", "input_sample_rate": 16000,
                                "output_sample_rate": OUTPUT_RATE, "valid_for_seconds": 900})
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
            raw: object = json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"null")
            body: Json = {str(key): value for key, value in raw.items()} if isinstance(raw, dict) else {}
            if self.headers.get("Authorization") != f"Bearer {TOKEN}":
                self.json(401, {"error": "unauthorized", "detail": "unknown token"})
            elif path == "/api/v1/post-proposals/commit":
                message = api.commit(body)
                if message is None:
                    self.json(409, {"error": "proposal_used", "detail": "already settled"})
                else:
                    self.json(200, {"parts": [message], "posted": message, "serial": PROPOSAL["serial"]})
            elif path == "/api/v1/voice-end":
                with api.changed:
                    api.ends.append(body)
                self.json(204, {})
            else:
                # Timing, health and the transcript record: accepted and dropped.
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


def live(page: Page) -> None:
    page.wait_for_function("() => document.getElementById('status-line').dataset.state === 'live'",
                           timeout=10_000)


def walk(page: Page, api: FakeApi, voice: FakeVoice) -> str:
    """The call, the yes, the service's close, and a hang-up. Answers a line for the summary."""
    page.fill("#api-token", TOKEN)
    page.click("#save-token")
    page.wait_for_selector("#talk", state="visible", timeout=10_000)

    # 1. A spoken call that greets. Microphone frames wait across the greeting's hand-off.
    page.click("#talk")
    until(page, lambda: len(voice.routes) == 1, "Talk to open the call's socket")
    live(page)
    voice.send(0, {"type": "session_started", "greeting": True})
    until(page, lambda: "audio_start" in voice.kinds(0), "the call to open its audio segment")
    voice.audio(0, 1.0)
    voice.say(0, "assistant", 0, "Hi, what can I do?")
    page.wait_for_timeout(300)
    check(not voice.audio_at[0], "the microphone streamed while the greeting was still to complete")
    voice.send(0, {"type": "turn_complete", "turn": 0})
    completed = time.monotonic()
    page.wait_for_timeout(HAND_OFF_QUIET_MS)
    early = [at for at in voice.audio_at[0] if at - completed < HAND_OFF_QUIET_MS / 1000]
    check(not early, f"{len(early)} microphone frames reached the server inside the greeting's hand-off,"
          f" the first {(early[0] - completed) * 1000:.0f} ms after its turn_complete" if early else "")
    until(page, lambda: bool(voice.audio_at[0]), "the microphone to come back after the greeting", 5_000)
    resumed_ms = (voice.audio_at[0][0] - completed) * 1000

    # 2. The owner asks; the agent proposes and reads the draft back, word for word, with its audio.
    voice.say(0, "user", 1, "tell lead team I'm on it, rebasing now")
    api.propose(PROPOSAL)
    page.locator("#post-confirm").wait_for(state="visible", timeout=10_000)
    voice.audio(0, 1.0)
    voice.say(0, "assistant", 1, READ_BACK)
    voice.send(0, {"type": "turn_complete", "turn": 1})
    # Heard to the end: a second of audio, banked a quarter of a second ahead, has played.
    page.wait_for_timeout(2500)

    # 3. The bridge's order: a hypothesis of the answer, the reply with its audio, then the final.
    voice.say(0, "user", 2, "Yes", final=False)
    voice.audio(0, 0.5)
    voice.say(0, "assistant", 2, "One moment.")
    voice.say(0, "user", 2, "Yes.")
    until(page, lambda: bool(api.commits), "the spoken yes to commit the card", 6_000)
    page.wait_for_timeout(300)
    check(api.commits == [{"handle": PROPOSAL["handle"], "channel_id": PROPOSAL["channel_id"],
                           "text": PROPOSAL["text"], "reply_to": PROPOSAL["reply_to"],
                           "confirmed_by": "speaker_turn"}],
          f"the yes did not commit the card once, as the speaker's turn: {api.commits}")
    page.wait_for_function("() => document.getElementById('post-confirm').hidden", timeout=5_000)

    # 4. The page's notice waits for the owner's turn to complete before it closes the segment, and
    # then for the hand-off after that turn_complete, as the microphone does.
    page.wait_for_timeout(300)
    check("audio_end" not in voice.kinds(0), "the notice closed the segment inside the owner's open turn")
    voice.send(0, {"type": "turn_complete", "turn": 2})
    released = time.monotonic()
    until(page, lambda: "audio_end" in voice.kinds(0), "the turn's end to close the segment for the notice")
    gap_ms = (voice.first_at(0, "audio_end") - released) * 1000
    check(gap_ms >= HAND_OFF_QUIET_MS, f"the notice closed the segment {gap_ms:.0f} ms after the turn_complete that"
          " let it go, inside the backend's hand-off")
    check(voice.prompts(0) == [], f"the notice went before the segment's boundary: {voice.prompts(0)}")
    voice.send(0, {"type": "turn_complete", "turn": 3})
    until(page, lambda: bool(voice.prompts(0)), "the notice to go as a prompt")
    check(voice.prompts(0)[0].startswith("Voice page: the message you read back was SENT to lead team."),
          f"the agent was told {voice.prompts(0)}")
    voice.say(0, "assistant", 4, "Sent.")
    voice.send(0, {"type": "turn_complete", "turn": 4})
    until(page, lambda: voice.kinds(0)[-1] == "audio_start", "the microphone to come back after the notice")
    check("quit" not in voice.kinds(0), "the healthy call said goodbye on its own")

    # 5. The server ends the call. The page says so, and that the reader did not.
    voice.routes[0].close(code=1000, reason="session expired")
    page.locator("#error").wait_for(state="visible", timeout=5_000)
    said = page.inner_text("#error")
    check(re.match(r"^The voice service ended the call \d+ s in — you did not hang up\.", said) is not None,
          f"a call the service closed said: {said!r}")
    until(page, lambda: len(api.recorded_ends()) == 1, "the call's end to be recorded")
    end = api.recorded_ends()[0]
    check((end.get("ended_by"), end.get("close_code")) == ("server_closed", 1000),
          f"the service's close was recorded as {end}")

    # 6. A second call, hung up: the reader's goodbye, once, recorded as the reader's.
    page.click("#talk")
    until(page, lambda: len(voice.routes) == 2, "Talk to open a second call")
    live(page)
    voice.send(1, {"type": "session_started", "greeting": False})
    until(page, lambda: "audio_start" in voice.kinds(1), "the second call to open its audio segment")
    page.click("#hang-up")
    until(page, lambda: len(api.recorded_ends()) == 2, "the hang-up to be recorded")
    check(voice.kinds(1).count("quit") == 1 and voice.kinds(1).count("audio_end") == 1,
          f"hanging up sent {voice.kinds(1)}")
    check(api.recorded_ends()[1].get("ended_by") == "hang_up", f"the hang-up was recorded as {api.recorded_ends()[1]}")
    check(page.locator("#error").is_hidden(), "hanging up was reported as an error")
    return (f"no microphone frame inside the greeting's hand-off (back after {resumed_ms:.0f} ms); a yes"
            " whose final transcript came after the reply's committed the card once, as the speaker's"
            f" turn; the notice waited for the owner's turn and its hand-off ({gap_ms:.0f} ms) and went"
            " at the segment's boundary; the"
            " service's close was said and recorded as server_closed 1000; a hang-up sent one goodbye"
            " and was recorded as hang_up")


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
    origin = f"http://127.0.0.1:{server.server_port}"

    try:
        with sync_playwright() as playwright, tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-drops-") as profile:
            context = playwright.chromium.launch_persistent_context(
                profile,
                headless=True,
                executable_path=args.browser_executable,
                args=["--use-fake-ui-for-media-stream", "--use-fake-device-for-media-stream",
                      "--autoplay-policy=no-user-gesture-required"],
                user_agent=(
                    "Mozilla/5.0 (Linux; Android 15; Pixel 7) AppleWebKit/537.36 "
                    "(KHTML, like Gecko) Chrome/151.0.0.0 Mobile Safari/537.36"
                ),
                viewport={"width": 412, "height": 915},
                device_scale_factor=2.625,
                is_mobile=True,
                has_touch=True,
            )
            voice = FakeVoice()
            try:
                context.grant_permissions(["microphone"], origin=origin)
                context.route_web_socket(re.compile(r"/ws\?mode=voice$"), voice.accept)
                page = context.pages[0]
                errors: list[str] = []
                page.on("pageerror", lambda error: errors.append(str(error)))
                page.goto(f"{origin}/voice", wait_until="load")
                summary = walk(page, api, voice)
                check(not errors, f"the page threw: {errors}")
                browser_version = context.browser.version if context.browser else "Chromium"
            finally:
                context.close()
        print(f"{browser_version} at 412x915: {summary}")
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
