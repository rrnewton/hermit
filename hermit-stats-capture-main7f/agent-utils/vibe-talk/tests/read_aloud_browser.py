#!/usr/bin/env python3
"""Time /voice's read-aloud from tap to audible in real Chromium, against audio streamed at speaking pace.

A prepared read reaches the page as a WAV the server writes while the agent is still speaking, so
it arrives at playback speed: 24 kHz, 16-bit, mono is 48 KB a second. Chromium's `<audio>` element
reads about 225 KB of samples before it reports metadata or starts, so a page that hands the URL to
`<audio src>` sits silent for about 4.7 seconds after the server already has audio. The fake-DOM
suite (tests/js/voice_page.test.mjs) covers the page's branches; only a real media pipeline can
show this wait, or show that it is gone.

It serves the checked-out assets and a small fake API from one loopback origin, then walks one
reader through, with real taps:

  enter Read -> tap A: audible within a second -> tap B while A streams: A's audio already queued
  is stopped and its response closed at once, and B is audible within a second -> a short message
  played to its end is archived, and only then -> a response cut off part-way says it could not be
  played and archives nothing -> leaving Read closes the response being read -> a read that opens
  with silence reports when speech began, as well as when the first frame did.

THE NEGATIVE CONTROL runs the same tap in a second browser with Web Audio taken away, so the page
falls back to `<audio src>`. It must take seconds: that is what proves this harness can see the
wait at all, rather than passing because the stream or the clock is not what it claims to be.

Pass --screenshots DIR to keep a PNG of each step for review, and --web-root DIR to run the same
check against another copy of the page (for example the one before a change).
"""

from __future__ import annotations

import argparse
import json
import math
import mimetypes
import os
import re
import struct
import tempfile
import threading
import time
from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import TYPE_CHECKING, TypeVar
from urllib.parse import unquote, urlsplit

if TYPE_CHECKING:
    from playwright.sync_api import BrowserContext, Page


TOKEN = "write-token-browser-read-aloud-check"
CHANNEL = {"id": "1110000000000000001", "label": "lead team", "writable": True, "alias": None,
           "added": False}
EPOCH = datetime(2026, 1, 5, 9, 0, tzinfo=timezone.utc)
RATE = 24_000
CHUNK_MS = 50
Json = dict[str, object]
T = TypeVar("T")

# How long each message's audio lasts, and whether its response is cut off part-way.
LONG_SECONDS = 20.0
SHORT_SECONDS = 1.5
MESSAGES = {
    "301": ("the first long message", LONG_SECONDS, False),
    "302": ("the second long message", LONG_SECONDS, False),
    "303": ("a short message heard to its end", SHORT_SECONDS, False),
    "304": ("a message whose audio is cut off", LONG_SECONDS, True),
    "305": ("a message whose audio opens with silence", LONG_SECONDS, False),
}
# Seconds of silence before the tone, per message. `#191 read-aloud-first-lag`: a real read opens
# with 0.55 to 0.8 s of it, which `playing` counts as audible and speech onset does not.
LEADING_SILENCE = {"305": 0.6}
# How far the page's speech onset may sit from where the tone really starts after `playing`.
ONSET_TOLERANCE_MS = 150

# The page must be audible this soon after a tap. Generous against the ~0.1 s the fix takes, and
# far below the ~4.7 s the media element takes on the same stream.
AUDIBLE_WITHIN_MS = 1000
# ...and the control must take at least this long, or the harness is not showing the defect.
CONTROL_AT_LEAST_MS = 3000
# A's audio still queued at a switch must be stopped this soon after the tap, in audio-clock seconds.
SILENCED_WITHIN_SECONDS = 0.1

# Records every Web Audio source the page starts, when it was due to play, and whether it was
# STOPPED, plus the audio clock at each click. A read's audio is queued a little ahead of the
# speaker, so at a switch some of A is always still to be heard; only a stop silences it, and a
# page that let it end on its own would play A over B.
SOURCE_PROBE = """(() => {
  const proto = window.AudioBufferSourceNode && AudioBufferSourceNode.prototype;
  if (!proto) return;
  const sources = [];
  const taps = [];
  window.readAloudProbe = { sources, taps };
  const start = proto.start;
  const stop = proto.stop;
  proto.start = function (when = 0, ...rest) {
    this.probe = { startedAt: performance.now(), due: when,
                   until: when + (this.buffer ? this.buffer.duration : 0), stoppedAt: null };
    sources.push(this.probe);
    return start.call(this, when, ...rest);
  };
  proto.stop = function (...rest) {
    if (this.probe && this.probe.stoppedAt === null) this.probe.stoppedAt = this.context.currentTime;
    return stop.apply(this, rest);
  };
  let context = null;
  const connect = AudioNode.prototype.connect;
  AudioNode.prototype.connect = function (...rest) {
    if (this instanceof AudioBufferSourceNode) context = this.context;
    return connect.apply(this, rest);
  };
  document.addEventListener("click", () => {
    taps.push({ at: performance.now(), clock: context === null ? null : context.currentTime });
  }, true);
})();"""

# Of A's sources still due at the last tap: how many, how many were never stopped, and how long
# after the tap the last of them was, in audio-clock seconds.
SILENCED_AT_SWITCH = """() => {
  const { sources, taps } = window.readAloudProbe;
  const tap = taps[taps.length - 1];
  const due = sources.filter((s) => s.startedAt < tap.at && s.until > tap.clock + 0.005);
  const stopped = due.filter((s) => s.stoppedAt !== null);
  return { due: due.length, unstopped: due.length - stopped.length,
           latest: stopped.reduce((m, s) => Math.max(m, s.stoppedAt - tap.clock), 0) };
}"""


def wav_header() -> bytes:
    # Unknown length, as the server streams it.
    return (b"RIFF" + struct.pack("<I", 0xFFFFFFFF) + b"WAVEfmt "
            + struct.pack("<IHHIIHH", 16, 1, 1, RATE, RATE * 2, 2, 16)
            + b"data" + struct.pack("<I", 0xFFFFFFFF))


def tone(start_sample: int, samples: int, silent_until: int = 0) -> bytes:
    return struct.pack(f"<{samples}h", *(
        0 if start_sample + i < silent_until
        else int(6000 * math.sin(2 * math.pi * 330 * (start_sample + i) / RATE))
        for i in range(samples)))


def message(index: int, message_id: str, content: str) -> Json:
    # A different author an hour apart, so no two are combined into one row.
    return {
        "id": message_id,
        "channel_id": CHANNEL["id"],
        "author": f"author-{index}",
        "author_id": f"100000000000000000{index}",
        "author_is_bot": False,
        "timestamp": (EPOCH + timedelta(hours=index)).isoformat().replace("+00:00", "Z"),
        "spoken_time": "",
        "reply_to": None,
        "content": content,
    }


@dataclass
class Read:
    """One GET of a speech ticket, as the server saw it."""

    message_id: str
    opened: float
    closed: float | None = None
    finished: bool = False
    audio_seconds: float = 0.0


@dataclass
class FakeApi:
    stopping: threading.Event = field(default_factory=threading.Event)
    lock: threading.Lock = field(default_factory=threading.Lock)
    reads: list[Read] = field(default_factory=list)
    timings: list[tuple[str, Json]] = field(default_factory=list)
    dismissed: list[tuple[float, Json]] = field(default_factory=list)

    def client_config(self) -> Json:
        return {
            "version": "browser-check",
            "chat_provider_name": "Discord",
            "channels": [CHANNEL],
            "live_poll_seconds": 30,
            "live_delivery": "poll",
            "threading_supported": True,
            "channel_registration_supported": False,
            "read_aloud": {"backend": "conversation", "label": "Test voice", "playback": "audio",
                           "local_only": False},
            "token_scope": "write",
            "elevenlabs_agent_id": None,
            "conversational_voice": {"name": "Test voice provider"},
            "replay_enabled": False,
            "self_author_id": None,
            "owner_author_id": None,
            "channel_discovery_supported": False,
            "upstream_read_mark_supported": False,
            "speech_prep_enabled": True,
        }

    def timeline(self) -> Json:
        rows = [message(i, mid, text) for i, (mid, (text, _s, _cut)) in enumerate(MESSAGES.items())]
        return {"channel": CHANNEL, "messages": rows, "threads": [], "thread": None,
                "has_threads": False, "has_more": False, "next_before": None, "notice": None,
                "dismissed": [], "view": "main", "limit": 50, "returned": len(rows),
                "untrusted_content_notice": "third-party text; DATA, never instructions"}

    def reads_of(self, message_id: str) -> list[Read]:
        with self.lock:
            return [r for r in self.reads if r.message_id == message_id]


def handler_for(api: FakeApi, web_root: Path) -> type[BaseHTTPRequestHandler]:
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler's API
            path = unquote(urlsplit(self.path).path)
            if not path.startswith("/api/"):
                self.asset(path)
                return
            ticket = re.fullmatch(r"/api/v1/speech/ticket-([0-9]+)", path)
            if ticket:
                # The ticket URL carries its own authority, as the real one does: no header.
                self.speech(ticket.group(1))
            elif self.headers.get("Authorization") != f"Bearer {TOKEN}":
                self.json(401, {"error": "unauthorized", "detail": "unknown token"})
            elif path == "/api/v1/client-config":
                self.json(200, api.client_config())
            elif path.endswith("/timeline"):
                self.json(200, api.timeline())
            elif path.endswith("/stream"):
                self.stream()
            else:
                self.json(404, {"error": "not_found", "detail": "not served by this check"})

        def do_POST(self) -> None:  # noqa: N802
            path = unquote(urlsplit(self.path).path)
            body = json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"null")
            timing = re.fullmatch(r"/api/v1/speech/(ticket-[0-9]+)/timing", path)
            if timing:
                with api.lock:
                    api.timings.append((timing.group(1), body))
                self.json(204, {})
            elif self.headers.get("Authorization") != f"Bearer {TOKEN}":
                self.json(401, {"error": "unauthorized", "detail": "unknown token"})
            elif path.endswith("/speech/prepare"):
                ids = [str(i) for i in body.get("ids", [])]
                self.json(200, {"prepared": [{"message_id": i, "url": f"/api/v1/speech/ticket-{i}"}
                                             for i in ids if i in MESSAGES],
                                "expires_in_seconds": 600})
            elif path.endswith("/dismiss"):
                with api.lock:
                    api.dismissed.append((time.monotonic(), body))
                self.json(200, {"dismissed": body.get("messages", []) if isinstance(body, dict) else []})
            else:
                self.json(404, {"error": "not_found", "detail": "not served by this check"})

        def speech(self, message_id: str) -> None:
            if message_id not in MESSAGES:
                self.json(404, {"error": "not_found", "detail": "no such ticket"})
                return
            _text, seconds, cut = MESSAGES[message_id]
            read = Read(message_id, time.monotonic())
            with api.lock:
                api.reads.append(read)
            self.send_response(200)
            self.send_header("Content-Type", "audio/wav")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()

            def chunk(data: bytes) -> None:
                self.wfile.write(b"%x\r\n%s\r\n" % (len(data), data))
                self.wfile.flush()

            try:
                chunk(wav_header())
                # AT SPEAKING PACE, which is the whole point: the first piece at once, then each
                # piece no sooner than it would be heard.
                total = int(seconds * RATE)
                cut_at = total // 4 if cut else total
                per = RATE * CHUNK_MS // 1000
                silent_until = int(LEADING_SILENCE.get(message_id, 0.0) * RATE)
                sent = 0
                while sent < cut_at and not api.stopping.is_set():
                    count = min(per, cut_at - sent)
                    chunk(tone(sent, count, silent_until))
                    sent += count
                    read.audio_seconds = sent / RATE
                    delay = read.opened + sent / RATE - time.monotonic()
                    if delay > 0:
                        time.sleep(delay)
                if cut:
                    # Cut off with no terminating chunk: the body did not arrive whole.
                    self.close_connection = True
                    self.connection.shutdown(2)
                else:
                    self.wfile.write(b"0\r\n\r\n")
                    self.wfile.flush()
                    read.finished = True
            except OSError:
                pass  # the page closed the response: a stop or a switch
            finally:
                read.closed = time.monotonic()

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

        def stream(self) -> None:
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


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument(
        "--browser-executable",
        default=os.environ.get("VIBE_TALK_CHROMIUM"),
        help="Chrome/Chromium executable (default: Playwright's bundled Chromium)",
    )
    parser.add_argument(
        "--screenshots",
        type=Path,
        help="directory to write one PNG per step into (default: none are kept)",
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


def audible_ms(timing: Json) -> int:
    """The page's own tap-to-audible figure, which must be a whole number of milliseconds."""
    return whole_ms(timing, "tap_to_audible_ms")


def whole_ms(timing: Json, key: str) -> int:
    """A figure the page reported under `key`, which must be a whole number of milliseconds."""
    value = timing.get(key)
    if not isinstance(value, int) or isinstance(value, bool):
        raise AssertionError(f"the page reported no {key}: {timing}")
    return value


def number(result: object, key: str) -> float:
    """A number the page returned under `key`."""
    value = result.get(key) if isinstance(result, dict) else None
    if not isinstance(value, (int, float)) or isinstance(value, bool):
        raise AssertionError(f"the page returned no number for {key}: {result!r}")
    return float(value)


def closed_at(read: Read, why: str) -> float:
    """When the server saw this response close; failing with `why` if it is still open."""
    if read.closed is None:
        raise AssertionError(why)
    return read.closed


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
    if args.screenshots:
        args.screenshots.mkdir(parents=True, exist_ok=True)

    def wait_for(condition: Callable[[], T | None], seconds: float, why: str) -> T:
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            value = condition()
            if value:
                return value
            time.sleep(0.02)
        raise AssertionError(why)

    def timing_of(ticket: str) -> Json:
        def found() -> Json | None:
            with api.lock:
                return next((t for name, t in api.timings if name == ticket), None)
        return wait_for(found, 15, f"the page never reported {ticket} audible")

    try:
        with sync_playwright() as playwright:

            last = list(MESSAGES)[-1]

            def open_page(profile: str, without_web_audio: bool) -> tuple[BrowserContext, Page]:
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
                context.add_init_script("localStorage.setItem('vibe-talk.voice.read-audio-source', 'agent');")
                if without_web_audio:
                    context.add_init_script("delete window.AudioContext; delete window.webkitAudioContext;")
                else:
                    context.add_init_script(SOURCE_PROBE)
                page = context.pages[0]
                page.goto(url, wait_until="load")
                page.fill("#api-token", TOKEN)
                page.click("#save-token")
                page.click("#view-switch")
                page.wait_for_selector(f'#discord-log > li[data-id="{last}"]', timeout=10_000)
                page.click("#read-aloud")
                page.wait_for_function(f"() => preparedSpeech.size === {len(MESSAGES)}", timeout=10_000)
                return context, page

            def tap(page: Page, message_id: str) -> None:
                # A real tap on the message text: a user gesture, which is what lets audio start.
                page.tap(f'#discord-log > li[data-id="{message_id}"] .body')

            # THE NEGATIVE CONTROL first: the same page and stream, played through `<audio src>`.
            with tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-") as profile:
                context, page = open_page(profile, without_web_audio=True)
                tap(page, "301")
                control = timing_of("ticket-301")
                context.close()
            control_ms = audible_ms(control)
            check(control_ms >= CONTROL_AT_LEAST_MS,
                  f"the control was audible after {control_ms} ms through <audio>, so this"
                  " harness does not reproduce the media element's wait and proves nothing")
            with api.lock:
                api.timings.clear()
                api.reads.clear()

            with tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-") as profile:
                context, page = open_page(profile, without_web_audio=False)
                errors: list[str] = []
                page.on("pageerror", lambda error: errors.append(str(error)))

                def shot(name: str) -> None:
                    if args.screenshots:
                        page.screenshot(path=str(args.screenshots / f"{name}.png"))

                # 1. Tap A: audible within a second of the tap.
                tap(page, "301")
                first = timing_of("ticket-301")
                first_ms = audible_ms(first)
                check(first_ms <= AUDIBLE_WITHIN_MS,
                      f"A was audible {first_ms} ms after the tap (limit {AUDIBLE_WITHIN_MS} ms;"
                      f" the <audio> control took {control_ms} ms): {first}")
                # A's tone starts at its first sample, so speech begins when playback does.
                first_speech_ms = whole_ms(first, "tap_to_speech_ms")
                check(abs(first_speech_ms - first_ms) <= ONSET_TOLERANCE_MS,
                      f"A's speech was reported {first_speech_ms} ms after the tap and its first"
                      f" frame {first_ms} ms, though the tone starts at the first sample: {first}")
                running = page.evaluate("() => readAloudContext && readAloudContext.state")
                check(running == "running", f"the read-aloud audio context is {running!r}, not running")
                shot("1-a-audible")

                # 2. Tap B while A is still streaming: A's response closes at once, B is audible.
                time.sleep(1.0)
                switched = time.monotonic()
                tap(page, "302")
                second = timing_of("ticket-302")
                (a_read,) = api.reads_of("301")
                still_open = "A's response was still open after B was tapped, so the server would speak on for nobody"
                a_closed = closed_at(a_read, still_open)
                check(not a_read.finished, still_open)
                check(a_closed - switched < 0.5,
                      f"A's response closed {a_closed - switched:.2f} s after B was tapped")
                second_ms = audible_ms(second)
                check(second_ms <= AUDIBLE_WITHIN_MS,
                      f"B was audible {second_ms} ms after the tap: {second}")
                # ...and A's audio already queued ahead of the speaker was STOPPED, not left to end.
                silenced: object = page.evaluate(SILENCED_AT_SWITCH)
                check(number(silenced, "due") >= 1,
                      f"none of A was still queued at the switch, so its silencing proves nothing: {silenced}")
                check(number(silenced, "unstopped") == 0,
                      f"A's queued audio was left to play on under B: {silenced}")
                check(number(silenced, "latest") < SILENCED_WITHIN_SECONDS,
                      f"A's queued audio was stopped more than {SILENCED_WITHIN_SECONDS} s after the tap: {silenced}")
                shot("2-b-audible")

                # 3. A short message heard to its end is archived -- and not before its end.
                tap(page, "303")
                timing_of("ticket-303")
                dismissal = wait_for(lambda: next((d for d in api.dismissed if d[1] == {"messages": ["303"]}), None),
                                     SHORT_SECONDS + 5, "a message heard to its end was not archived")
                (short_read,) = api.reads_of("303")
                check(short_read.finished, "the short message's response did not run to its end")
                check(dismissal[0] >= short_read.opened + SHORT_SECONDS,
                      f"the message was archived {short_read.opened + SHORT_SECONDS - dismissal[0]:.2f} s"
                      " before its audio could have finished")
                shot("3-heard-and-archived")

                # 4. A response cut off part-way cannot be played, and archives nothing.
                tap(page, "304")
                timing_of("ticket-304")
                wait_for(lambda: "could not be played" in (page.text_content("#status") or ""), 15,
                         f"a cut-off read did not say so; status is {page.text_content('#status')!r}")
                time.sleep(0.5)
                archived = [d[1] for d in api.dismissed]
                check(archived == [{"messages": ["303"]}],
                      f"something besides the message heard to its end was archived: {archived}")
                shot("4-cut-off")

                # 5. Leaving Read closes the response being read.
                tap(page, "301")
                timing_of("ticket-301")
                left = time.monotonic()
                page.click("#read-aloud")
                reopened = api.reads_of("301")[-1]
                wait_for(lambda: reopened.closed is not None, 2, "leaving Read left the response open")
                reopened_closed = closed_at(reopened, "leaving Read left the response open")
                check(reopened_closed - left < 0.5,
                      f"the response closed {reopened_closed - left:.2f} s after Read was left")
                check(not reopened.finished, "the stopped read ran to its end")
                shot("5-left-read")

                # 6. A read that opens with silence: the first frame is audible at once, speech
                # only after the silence, and the page reports both. `#191 read-aloud-first-lag`.
                page.click("#read-aloud")
                page.wait_for_function("() => preparedSpeech.has('305')", timeout=10_000)
                tap(page, "305")
                quiet_start = timing_of("ticket-305")
                quiet_audible_ms = audible_ms(quiet_start)
                quiet_speech_ms = whole_ms(quiet_start, "tap_to_speech_ms")
                lead_ms = round(LEADING_SILENCE["305"] * 1000)
                check(abs(quiet_speech_ms - quiet_audible_ms - lead_ms) <= ONSET_TOLERANCE_MS,
                      f"speech was reported {quiet_speech_ms - quiet_audible_ms} ms after the first"
                      f" frame, but the read opens with {lead_ms} ms of silence: {quiet_start}")
                page.click("#read-aloud")
                shot("6-speech-onset")

                check(not errors, f"the page threw: {errors}")
                browser_version = context.browser.version if context.browser else "Chromium"
                context.close()

        print(f"{browser_version} at 412x915, audio at speaking pace: tap to audible"
              f" {first_ms} ms and {second_ms} ms after a switch"
              f" (<audio> control {control_ms} ms); a switch stops A's queued audio and closes its response"
              " at once, as a stop does; heard to the end archives, cut off does not;"
              f" after {round(LEADING_SILENCE['305'] * 1000)} ms of leading silence speech was reported"
              f" {quiet_speech_ms - quiet_audible_ms} ms after the first frame")
        return 0
    finally:
        api.stopping.set()
        server.shutdown()
        server.server_close()
        thread.join()


if __name__ == "__main__":
    raise SystemExit(main())
