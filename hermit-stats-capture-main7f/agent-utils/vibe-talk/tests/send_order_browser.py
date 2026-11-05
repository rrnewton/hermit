#!/usr/bin/env python3
"""Send A then B through a bridge that fails A once, in real Chromium: A lands first, then B.

`#277 send-order`. The owner: "I write A then B but B lands first." A's post failed, A waited to go
again on its own, and B went at once; A's retry landed after it. The fake-DOM suite pins the page's
queue and the Rust suite the server's lanes; this runs the real page in a real browser against the
real server binary, which posts through its real provider client to a mock Google Chat bridge that
speaks the Discord-compatible bridge API on loopback:

  A is sent, and the bridge fails it at once (502 "message send failed") -> A says it goes again in
  a few seconds -> B is sent, and waits behind A, saying so; the bridge never sees it -> A goes again
  under the same nonce and lands -> B lands after it.

  C is sent and the bridge keeps failing it -> D is sent and waits -> a real reload -> C is left to
  the reader, nothing is sent on its own, and D still waits behind C -> Retry on C -> C lands, then D.

  E keeps failing -> F waits behind it -> Dismiss on E -> F lands, and E never does.

  G, H and I are written one after another while the bridge takes most of a second over each ->
  they reach it one at a time, in that order, never two at once.

  The server on its own: P is posted by a caller that hangs up while the bridge is still working on
  it, as a phone does when the page stops waiting, signs out or loses its connection -> Q, posted
  next, does not reach the bridge until P has landed.

The bridge records every post in the order it arrived, so "A then B" is what reached the chat
service, not what the page drew. It needs the release binary, which has web/ compiled into it:
`cargo build --release` first (the `send-order-browser` make target does). Pass --server-bin to use
another build, --port P to put the server on P and the bridge on P+1 (default: free ports), and
--screenshots DIR to keep a PNG of each waiting state.
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request
from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import unquote, urlsplit

CHANNEL_ID = "1110000000000000277"
CHANNEL_LABEL = "lead team"
BOT_ID = "1110000000000000009"
BOT_TOKEN = base64.urlsafe_b64encode(BOT_ID.encode()).decode().rstrip("=") + ".send-order.never-sent"
READ_TOKEN = "send-order-browser-read-token-0000000000"
WRITE_TOKEN = "send-order-browser-write-token-111111111"
FAILURE = "Google Chat message send failed"
Json = dict[str, object]


@dataclass
class Bridge:
    """A Google Chat bridge on the Discord-compatible API: posts by nonce, and fails the ones asked."""

    lock: threading.Lock = field(default_factory=threading.Lock)
    messages: list[Json] = field(default_factory=list)
    by_nonce: dict[str, Json] = field(default_factory=dict)
    # Every post as it arrived: (first word, nonce, the status it was answered with).
    arrivals: list[tuple[str, str, int]] = field(default_factory=list)
    fail_once: set[str] = field(default_factory=set)
    failing: set[str] = field(default_factory=set)
    # Seconds the bridge works on a post, by first word, before it answers: a slow bridge.
    hold: dict[str, float] = field(default_factory=dict)
    # When each post reached the bridge, and when each one landed, by first word.
    reached_at: dict[str, float] = field(default_factory=dict)
    landed_at: dict[str, float] = field(default_factory=dict)
    in_flight: int = 0
    most_in_flight: int = 0

    def post(self, body: Json) -> tuple[int, Json]:
        content = str(body.get("content") or "")
        word = content.split()[0] if content.split() else ""
        with self.lock:
            self.reached_at.setdefault(word, time.monotonic())
            self.in_flight += 1
            self.most_in_flight = max(self.most_in_flight, self.in_flight)
            delay = self.hold.get(word, 0.0)
        try:
            # Working on it, whether or not whoever asked is still waiting for the answer.
            if delay:
                time.sleep(delay)
            return self.commit(word, str(body.get("nonce") or ""), content)
        finally:
            with self.lock:
                self.in_flight -= 1

    def commit(self, word: str, nonce: str, content: str) -> tuple[int, Json]:
        with self.lock:
            if nonce and nonce in self.by_nonce:
                # The same request again: the post it already made, never a second one.
                self.arrivals.append((word, nonce, 200))
                return 200, self.by_nonce[nonce]
            if word in self.failing or word in self.fail_once:
                self.fail_once.discard(word)
                self.arrivals.append((word, nonce, 502))
                return 502, {"message": FAILURE}
            serial = len(self.messages) + 1
            message: Json = {
                "id": f"11100000000000{serial:05d}", "channel_id": CHANNEL_ID,
                "author": {"id": BOT_ID, "username": "vibe-talk", "bot": True},
                "timestamp": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
                "content": content,
            }
            self.messages.append(message)
            if nonce:
                self.by_nonce[nonce] = message
            self.arrivals.append((word, nonce, 200))
            self.landed_at.setdefault(word, time.monotonic())
            return 200, message

    def busiest(self) -> int:
        """The most posts the bridge has had in hand at once since the last `calm()`."""
        with self.lock:
            return self.most_in_flight

    def calm(self) -> None:
        with self.lock:
            self.most_in_flight = self.in_flight

    def landed(self) -> list[str]:
        """The first word of each message the chat service holds, in the order they landed."""
        with self.lock:
            return [str(message["content"]).split()[0] for message in self.messages]

    def tried(self) -> list[tuple[str, str, int]]:
        with self.lock:
            return list(self.arrivals)

    def newest_first(self) -> list[Json]:
        with self.lock:
            return list(reversed(self.messages))


def bridge_handler(bridge: Bridge) -> type[BaseHTTPRequestHandler]:
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def answer(self, status: int, payload: object) -> None:
            body = json.dumps(payload).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler's API
            path = unquote(urlsplit(self.path).path)
            if path == "/users/@me":
                self.answer(200, {"id": BOT_ID, "username": "vibe-talk", "bot": True})
            elif path == f"/channels/{CHANNEL_ID}":
                self.answer(200, {"id": CHANNEL_ID, "name": CHANNEL_LABEL, "type": 0})
            elif path == f"/channels/{CHANNEL_ID}/messages":
                self.answer(200, bridge.newest_first())
            else:
                self.answer(404, {"message": f"not served by the send-order bridge: GET {path}"})

        def do_POST(self) -> None:  # noqa: N802
            path = unquote(urlsplit(self.path).path)
            raw: object = json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"null")
            if path == f"/channels/{CHANNEL_ID}/messages" and isinstance(raw, dict):
                status, payload = bridge.post({str(key): value for key, value in raw.items()})
                self.answer(status, payload)
            else:
                self.answer(404, {"message": f"not served by the send-order bridge: POST {path}"})

        def log_message(self, _format: str, *_args: object) -> None:
            pass

    return Handler


def reply_request(port: int, text: str, key: str) -> bytes:
    """A POST to the channel's reply route, as the page makes it, as raw HTTP/1.1."""
    body = json.dumps({"text": text, "idempotency_key": key}).encode()
    head = (
        f"POST /api/v1/channels/{CHANNEL_ID}/reply HTTP/1.1\r\n"
        f"Host: 127.0.0.1:{port}\r\n"
        f"Authorization: Bearer {WRITE_TOKEN}\r\n"
        "Content-Type: application/json\r\n"
        f"Content-Length: {len(body)}\r\n"
        "\r\n"
    )
    return head.encode() + body


def post_reply(port: int, text: str, key: str) -> int:
    """POST `text` to the channel and wait for the answer; its HTTP status."""
    request = urllib.request.Request(
        f"http://127.0.0.1:{port}/api/v1/channels/{CHANNEL_ID}/reply",
        data=json.dumps({"text": text, "idempotency_key": key}).encode(),
        headers={"Authorization": f"Bearer {WRITE_TOKEN}", "Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return int(response.status)
    except urllib.error.HTTPError as error:
        return int(error.code)


def free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return int(probe.getsockname()[1])


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument(
        "--server-bin",
        type=Path,
        default=Path(__file__).resolve().parents[1] / "target" / "release" / "vibe-talk",
        help="the vibe-talk server binary to run (default: this checkout's target/release/vibe-talk)",
    )
    parser.add_argument(
        "--port",
        type=int,
        default=0,
        help="loopback port for the server; the bridge takes the next one (default: two free ports)",
    )
    parser.add_argument(
        "--browser-executable",
        default=os.environ.get("VIBE_TALK_CHROMIUM"),
        help="Chrome/Chromium executable (default: Playwright's bundled Chromium)",
    )
    parser.add_argument("--screenshots", type=Path, help="keep a PNG of each waiting state in this directory")
    return parser.parse_args()


def check(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def write_config(workdir: Path, port: int, bridge_port: int) -> Path:
    config = workdir / "vibe-talk.toml"
    config.write_text(f"""
[server]
bind = "127.0.0.1:{port}"

[discord]
provider_name = "Google Chat"
thread_api = "off"
channel_registration = true
bot_token = "{BOT_TOKEN}"
api_base = "http://127.0.0.1:{bridge_port}"
live_poll_seconds = 0
request_timeout_seconds = 10

[auth]
read_token = "{READ_TOKEN}"
write_token = "{WRITE_TOKEN}"

[[channels]]
id = "{CHANNEL_ID}"
label = "{CHANNEL_LABEL}"
writable = true

[read_aloud]
backend = "browser"

[storage]
path = "{workdir}/state/vibe-talk.sqlite3"
""")
    return config


def start_server(binary: Path, config: Path, port: int, log: Path) -> subprocess.Popen[bytes]:
    env = {key: value for key, value in os.environ.items() if not key.startswith("VIBE_TALK_")}
    env["RUST_LOG"] = "warn"
    with log.open("wb") as sink:
        server = subprocess.Popen([str(binary), "--config", str(config), "--skip-startup-probe"],
                                  cwd=config.parent, stdout=sink, stderr=subprocess.STDOUT, env=env)
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        if server.poll() is not None:
            raise RuntimeError(f"the server exited: {log.read_text(errors='replace')[-2000:]}")
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{port}/healthz", timeout=2).read()
            return server
        except (urllib.error.URLError, OSError):
            time.sleep(0.2)
    server.terminate()
    raise RuntimeError("the server never answered /healthz")


def main() -> int:
    args = arguments()
    try:
        from playwright.sync_api import Page, sync_playwright
    except ImportError as error:
        raise SystemExit(
            "Python Playwright is required: python3 -m pip install --user playwright && "
            "python3 -m playwright install chromium"
        ) from error
    binary: Path = args.server_bin
    if not binary.is_file():
        raise SystemExit(f"no server binary at {binary}: run `cargo build --release --locked` in vibe-talk first")
    shots: Path | None = args.screenshots
    if shots is not None:
        shots.mkdir(parents=True, exist_ok=True)

    bridge = Bridge()
    port = int(args.port) or free_port()
    bridge_port = port + 1 if args.port else free_port()
    httpd = ThreadingHTTPServer(("127.0.0.1", bridge_port), bridge_handler(bridge))
    httpd.daemon_threads = True
    bridge_thread = threading.Thread(target=httpd.serve_forever, daemon=True)
    bridge_thread.start()
    server: subprocess.Popen[bytes] | None = None

    def rows(page: Page) -> list[tuple[str, str]]:
        """The outbox as drawn: each row's send state and text."""
        found: object = page.evaluate(
            "() => [...document.querySelectorAll('#outgoing-log li.outgoing-message')]"
            ".map((li) => [li.getAttribute('data-send-state') || '', li.innerText])"
        )
        check(isinstance(found, list), f"the outbox could not be read: {found!r}")
        assert isinstance(found, list)
        return [(str(item[0]), str(item[1])) for item in found if isinstance(item, list)]

    def until(page: Page, what: str, done: Callable[[], bool], seconds: float = 15.0) -> None:
        """Wait, while the browser runs, until `done()` holds."""
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if done():
                return
            page.wait_for_timeout(50)
        raise AssertionError(f"{what}, within {seconds} s. Bridge: {bridge.tried()}. Outbox: {rows(page)}")

    def send(page: Page, text: str) -> None:
        page.fill("#channel-compose-text", text)
        page.click("#channel-send")

    def shoot(page: Page, name: str) -> None:
        if shots is not None:
            page.screenshot(path=str(shots / f"send-order-{name}.png"))

    try:
        with tempfile.TemporaryDirectory(prefix="vibe-talk-send-order-") as work:
            workdir = Path(work)
            (workdir / "state").mkdir()
            config = write_config(workdir, port, bridge_port)
            server = start_server(binary, config, port, workdir / "server.log")
            with sync_playwright() as playwright:
                context = playwright.chromium.launch_persistent_context(
                    str(workdir / "chrome"),
                    headless=True,
                    executable_path=args.browser_executable,
                    viewport={"width": 412, "height": 915},
                )
                page = context.pages[0]
                errors: list[str] = []
                page.on("pageerror", lambda error: errors.append(str(error)))
                page.goto(f"http://127.0.0.1:{port}/voice", wait_until="load")
                page.fill("#api-token", WRITE_TOKEN)
                page.click("#save-token")
                page.wait_for_selector("#view-switch", state="visible", timeout=10_000)
                if not page.is_visible("#channel-compose-text"):
                    page.click("#view-switch")
                page.wait_for_selector("#channel-compose-text", state="visible", timeout=10_000)

                # 1. A fails at once; B waits behind it; A goes again and lands; then B.
                bridge.fail_once.add("A")
                send(page, "A is the first thing I wrote")
                until(page, "A did not fail and wait to go again",
                      lambda: [state for state, _ in rows(page)] == ["retrying"])
                retrying = rows(page)[0][1]
                check("Google Chat could not answer: Google Chat message send failed." in retrying
                      and "Retrying in 3 s." in retrying, f"A's row does not say why it waits, or how long: {retrying!r}")
                send(page, "B is what I wrote next")
                until(page, "B was not shown waiting behind A",
                      lambda: [state for state, _ in rows(page)] == ["retrying", "queued"])
                queued = rows(page)[1][1]
                check("Waiting to send… “A is the first thing I wrote” goes first, and goes again in" in queued,
                      f"B does not say what it waits behind: {queued!r}")
                check([word for word, _, _ in bridge.tried()] == ["A"],
                      f"B reached the chat service while A waited to go again: {bridge.tried()}")
                controls: object = page.evaluate(
                    "() => [...document.querySelectorAll('#outgoing-log li.outgoing-message')[1]"
                    ".querySelectorAll('button')].map((button) => button.textContent)"
                )
                check(controls == ["Edit", "Dismiss"],
                      f"B, waiting its turn, should be able to be taken back but not sent out of turn: {controls!r}")
                shoot(page, "b-behind-a")
                until(page, "A and B did not both land", lambda: bridge.landed() == ["A", "B"])
                tried = bridge.tried()
                check([(word, status) for word, _, status in tried] == [("A", 502), ("A", 200), ("B", 200)],
                      f"the bridge was not asked for A, A again, then B: {tried}")
                check(tried[0][1] == tried[1][1] and tried[0][1] != tried[2][1],
                      f"A's retry was not the same request, or B borrowed it: {tried}")
                until(page, "the outbox did not empty", lambda: rows(page) == [])
                listed = page.inner_text("#discord-log")
                check(0 <= listed.find("A is the first") < listed.find("B is what I wrote"),
                      f"the channel does not show A before B: {listed!r}")

                # 2. C keeps failing; D waits; a real reload keeps D waiting, and sends nothing.
                bridge.failing.add("C")
                send(page, "C is stuck for now")
                until(page, "C did not fail", lambda: [state for state, _ in rows(page)] == ["retrying"])
                send(page, "D comes after C")
                until(page, "D was not shown waiting behind C",
                      lambda: [state for state, _ in rows(page)] == ["retrying", "queued"])
                page.reload(wait_until="load")
                page.wait_for_selector("#channel-compose-text", state="visible", timeout=10_000)
                until(page, "the reload did not restore C and D in order",
                      lambda: [state for state, _ in rows(page)] == ["unconfirmed", "queued"])
                held = rows(page)[1][1]
                check("“C is stuck for now” goes first, and its delivery is unconfirmed." in held,
                      f"after the reload D does not say what it waits behind: {held!r}")
                tries = len(bridge.tried())
                page.wait_for_timeout(4000)
                check(len(bridge.tried()) == tries, f"the reloaded page sent on its own: {bridge.tried()[tries:]}")
                check("D" not in [word for word, _, _ in bridge.tried()], f"D reached the bridge: {bridge.tried()}")
                shoot(page, "d-behind-c-after-reload")
                bridge.failing.discard("C")
                page.click("#outgoing-log li.outgoing-message:first-child .outgoing-retry")
                until(page, "C and D did not land after Retry", lambda: bridge.landed() == ["A", "B", "C", "D"])
                landed_words = [word for word, _, status in bridge.tried() if status == 200]
                check(landed_words[-2:] == ["C", "D"], f"D landed before C: {bridge.tried()}")
                until(page, "the outbox did not empty after the reload", lambda: rows(page) == [])

                # 3. E keeps failing; F waits behind it; Dismiss on E lets F go.
                bridge.failing.add("E")
                send(page, "E will not go through")
                until(page, "E did not fail", lambda: [state for state, _ in rows(page)] == ["retrying"])
                send(page, "F waits for E")
                until(page, "F was not shown waiting behind E",
                      lambda: [state for state, _ in rows(page)] == ["retrying", "queued"])
                page.click("#outgoing-log li.outgoing-message:first-child .outgoing-dismiss")
                until(page, "F did not land once E was dismissed", lambda: bridge.landed()[-1:] == ["F"])
                check("E" not in bridge.landed(), f"E landed: {bridge.tried()}")
                until(page, "the outbox did not empty after the Dismiss", lambda: rows(page) == [])
                bridge.failing.discard("E")

                # 4. A slow bridge: three messages written at once reach it one at a time, in order.
                for word in ("G", "H", "I"):
                    bridge.hold[word] = 0.8
                bridge.calm()
                for text in ("G goes first", "H goes second", "I goes third"):
                    send(page, text)
                until(page, "G, H and I did not all land", lambda: bridge.landed()[-3:] == ["G", "H", "I"], 20.0)
                check(bridge.busiest() == 1,
                      f"the bridge had {bridge.busiest()} posts to one channel in hand at once: {bridge.tried()}")
                until(page, "the outbox did not empty after the slow bridge", lambda: rows(page) == [])

                # 5. The server on its own: a caller that hangs up does not let the next post past.
                bridge.hold["P"] = 2.5
                bridge.calm()
                with socket.create_connection(("127.0.0.1", port), timeout=10) as caller:
                    caller.sendall(reply_request(port, "P is posted by a caller that hangs up", "send-order-p"))
                    until(page, "P never reached the bridge", lambda: "P" in bridge.reached_at)
                    time.sleep(0.2)
                # P's caller has gone; the bridge is still working on P.
                check("P" not in bridge.landed(), f"P landed before its caller hung up: {bridge.tried()}")
                status = post_reply(port, "Q follows P", "send-order-q")
                check(status == 200, f"Q was not posted: HTTP {status}")
                check(bridge.landed()[-2:] == ["P", "Q"], f"Q landed before P, whose caller had hung up: {bridge.tried()}")
                check(bridge.reached_at["Q"] >= bridge.landed_at["P"],
                      "Q reached the bridge while it was still working on P, whose caller had hung up")
                check(bridge.busiest() == 1, f"the bridge had {bridge.busiest()} posts in hand at once")
                check(not errors, f"the page threw: {errors}")
                version = context.browser.version if context.browser else "Chromium"
                context.close()
        print(f"{version} at 412x915 against the real server and a mock Google Chat bridge: A failed at"
              " once and waited 3 s, B waited behind it and the bridge never saw it, A went again under the"
              " same nonce and landed, then B; C kept failing, D waited behind it across a real reload that"
              " sent nothing, and Retry on C landed C, then D; Dismiss on a failing E let F go; G, H and I"
              " reached a slow bridge one at a time, in order; and Q did not reach the bridge until P, whose"
              " caller hung up mid-post, had landed")
        return 0
    finally:
        if server is not None:
            server.terminate()
            try:
                server.wait(timeout=10)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait()
        httpd.shutdown()
        httpd.server_close()
        bridge_thread.join()


if __name__ == "__main__":
    raise SystemExit(main())
