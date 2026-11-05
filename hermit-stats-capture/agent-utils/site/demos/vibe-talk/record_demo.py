#!/usr/bin/env python3
"""Record short vibe-talk demo clips against a mock Google Chat space, and encode them as GIFs.

Everything runs on loopback and nothing contacts a chat or model vendor:

  * gchat_bridge_mock.py (beside this file) serves a fictional Google Chat space through the
    Discord-compatible bridge contract vibe-talk reads, plus a canned Messages API for summaries;
  * the real vibe-talk server (a release build; web/ is compiled into it) is started with a
    throwaway configuration, vibe-talk.toml.template filled in, pointing at both;
  * Playwright drives Chromium through each scripted clip while Chrome DevTools' screencast
    captures every frame the compositor draws, with its timestamp;
  * ffmpeg turns each clip into an animated GIF (palettegen/paletteuse) and a poster PNG.

Usage, from the repository root:
    cargo build --release --manifest-path vibe-talk/Cargo.toml
    python3 site/demos/vibe-talk/record_demo.py [--only NAME,NAME] [--publish]

Output goes to --out (default: $XDG_CACHE_HOME/agent-utils/vibe-talk-demo, else
~/.cache/agent-utils/vibe-talk-demo), never into the checkout: the raw frames run to about 100 MB.
--publish then copies each GIF and poster into site/media/vibe-talk/, where the website uses them.

Ports (all 127.0.0.1): vibe-talk on --port-base (default 19310), the bridge on +1, the mock
Messages API on +2. They are checked free before anything starts. Every process this script starts
is stopped by PID when it ends, including on failure.

Needs: python3 with the `playwright` package and its Chromium (`python3 -m playwright install
chromium`), and ffmpeg on PATH (or --ffmpeg). No network and no credentials.
"""

from __future__ import annotations

import argparse
import asyncio
import base64
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import time
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING, Awaitable, Callable

if TYPE_CHECKING:
    from playwright.async_api import BrowserContext, CDPSession, Page

HERE = Path(__file__).resolve().parent
# site/demos/vibe-talk/ -> the repository root.
REPO_ROOT = HERE.parents[2]
DEFAULT_BINARY = REPO_ROOT / "vibe-talk" / "target" / "release" / "vibe-talk"
SITE_MEDIA = REPO_ROOT / "site" / "media" / "vibe-talk"
CONFIG_TEMPLATE = HERE / "vibe-talk.toml.template"
CHANNEL_ID = "1300000000000000001"
READ_TOKEN = "demo-read-token-0123456789abcdef"
WRITE_TOKEN = "demo-write-token-0123456789abcdef"
INGEST_TOKEN = "demo-ingest-token-0123456789abcdef"
# base64url("1500000000000000001"): the owner's account id, which vibe-talk reads out of the
# token's first segment as the account it posts as. Not a secret; the bridge accepts anything.
BRIDGE_TOKEN = "MTUwMDAwMDAwMDAwMDAwMDAwMQ.demo.bridge-token-not-secret"
OWNER_ID = "1500000000000000001"
ZONE = "America/New_York"


@dataclass(frozen=True)
class Profile:
    """A viewport to record at. The screencast delivers CSS-pixel frames scaled down from the
    device-pixel rendering, so `scale` 2 gives smoother text than rendering at 1."""

    width: int
    height: int
    scale: int
    mobile: bool


PHONE = Profile(width=412, height=915, scale=2, mobile=True)
DESKTOP = Profile(width=1000, height=625, scale=2, mobile=False)


# Drawn into the page by the recorder only: a ripple where a finger lands, and on the desktop an
# arrow that follows the mouse. Built with the CSSOM and the Web Animations API, so the page's own
# markup and stylesheets are untouched.
OVERLAY_JS = """
(() => {
  const desktop = __DESKTOP__;
  const install = () => {
    const root = document.documentElement;
    let cursor = null;
    if (desktop) {
      cursor = document.createElement('div');
      cursor.innerHTML = '<svg width="22" height="30" viewBox="0 0 22 30"><path d="M2 2 L2 24 L8 18 L12 28 L16 26 L12 16 L20 16 Z" fill="#fff" stroke="#111" stroke-width="1.6" stroke-linejoin="round"/></svg>';
      Object.assign(cursor.style, {position: 'fixed', left: '0', top: '0', width: '22px',
        height: '30px', pointerEvents: 'none', zIndex: '2147483647', display: 'none',
        filter: 'drop-shadow(0 1px 2px rgba(0,0,0,.5))'});
      root.appendChild(cursor);
      window.addEventListener('pointermove', (e) => {
        cursor.style.display = 'block';
        cursor.style.transform = `translate(${e.clientX - 2}px, ${e.clientY - 2}px)`;
      }, true);
    }
    window.addEventListener('pointerdown', (e) => {
      const dot = document.createElement('div');
      const size = desktop ? 30 : 46;
      Object.assign(dot.style, {position: 'fixed', left: `${e.clientX - size / 2}px`,
        top: `${e.clientY - size / 2}px`, width: `${size}px`, height: `${size}px`,
        borderRadius: '50%', background: 'rgba(255,255,255,0.30)',
        border: '2px solid rgba(255,255,255,0.75)', pointerEvents: 'none',
        zIndex: '2147483646', boxSizing: 'border-box'});
      root.appendChild(dot);
      dot.animate([{transform: 'scale(0.55)', opacity: 1}, {transform: 'scale(1.35)', opacity: 0}],
        {duration: 650, easing: 'ease-out', fill: 'forwards'});
      setTimeout(() => dot.remove(), 750);
    }, true);
    // Smooth, eased scrolling of one element by `dy` pixels over `ms`, resolved when done.
    window.__demoScroll = (selector, dy, ms) => new Promise((resolve) => {
      const node = document.querySelector(selector);
      if (!node) return resolve(false);
      const start = node.scrollTop;
      const target = Math.max(0, Math.min(node.scrollHeight - node.clientHeight, start + dy));
      const began = performance.now();
      const step = (now) => {
        const t = Math.min(1, (now - began) / ms);
        const eased = t < 0.5 ? 2 * t * t : 1 - Math.pow(-2 * t + 2, 2) / 2;
        node.scrollTop = start + (target - start) * eased;
        if (t < 1) requestAnimationFrame(step); else resolve(true);
      };
      requestAnimationFrame(step);
    });
  };
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', install);
  } else {
    install();
  }
})();
"""


def log(message: str) -> None:
    print(f"[record] {message}", file=sys.stderr, flush=True)


# -- processes ------------------------------------------------------------------------------------


def port_free(port: int) -> bool:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            probe.bind(("127.0.0.1", port))
        except OSError:
            return False
    return True


def http(method: str, url: str, body: object | None = None, token: str | None = None) -> object:
    data = None if body is None else json.dumps(body).encode()
    request = urllib.request.Request(url, data=data, method=method)
    if data is not None:
        request.add_header("Content-Type", "application/json")
    if token:
        request.add_header("Authorization", f"Bearer {token}")
    with urllib.request.urlopen(request, timeout=20) as response:
        raw = response.read()
    value: object = json.loads(raw) if raw else None
    return value


@dataclass
class Services:
    vibe_talk: Path
    run_dir: Path
    port_base: int
    procs: list[subprocess.Popen[bytes]] = field(default_factory=list)

    @property
    def vibe_url(self) -> str:
        return f"http://127.0.0.1:{self.port_base}"

    @property
    def bridge_url(self) -> str:
        return f"http://127.0.0.1:{self.port_base + 1}"

    def start(self) -> None:
        ports = [self.port_base, self.port_base + 1, self.port_base + 2]
        busy = [port for port in ports if not port_free(port)]
        if busy:
            raise SystemExit(f"loopback port(s) {busy} are in use; pick another --port-base")
        if self.run_dir.exists():
            shutil.rmtree(self.run_dir)
        self.run_dir.mkdir(parents=True)
        os.chmod(self.run_dir, 0o700)
        for name, value in (("llm-key", "demo-not-a-real-key"), ("ingest-token", INGEST_TOKEN)):
            path = self.run_dir / name
            path.write_text(value + "\n", encoding="utf-8")
            os.chmod(path, 0o600)
        config = self.run_dir / "vibe-talk.toml"
        template = CONFIG_TEMPLATE.read_text(encoding="utf-8")
        config.write_text(template.format(
            vibe_port=ports[0], bridge_port=ports[1], llm_port=ports[2], zone=ZONE,
            bridge_token=BRIDGE_TOKEN, owner_id=OWNER_ID, read_token=READ_TOKEN,
            write_token=WRITE_TOKEN, ingest_token=INGEST_TOKEN, channel_id=CHANNEL_ID,
            run_dir=self.run_dir), encoding="utf-8")
        bridge_log = open(self.run_dir / "bridge.log", "w", encoding="utf-8")
        self.procs.append(subprocess.Popen(
            [sys.executable, str(HERE / "gchat_bridge_mock.py"), "--bridge-port", str(ports[1]),
             "--llm-port", str(ports[2]), "--hint-url", f"{self.vibe_url}/api/v1/live/hints",
             "--ingest-token-file", str(self.run_dir / "ingest-token")],
            stdout=bridge_log, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL))
        log(f"bridge started, pid {self.procs[-1].pid}")
        self.wait_for(f"{self.bridge_url}/_demo/state", "the bridge")
        # A clean environment: every VIBE_TALK_* variable would override the file.
        env = {"HOME": os.environ.get("HOME", "/tmp"), "PATH": "/usr/bin:/bin", "RUST_LOG": "info"}
        server_log = open(self.run_dir / "vibe-talk.log", "w", encoding="utf-8")
        self.procs.append(subprocess.Popen(
            [str(self.vibe_talk), "--config", str(config)], env=env, stdout=server_log,
            stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL))
        log(f"vibe-talk started, pid {self.procs[-1].pid}")
        self.wait_for(f"{self.vibe_url}/healthz", "vibe-talk")

    def wait_for(self, url: str, what: str) -> None:
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            for proc in self.procs:
                if proc.poll() is not None:
                    raise SystemExit(f"{what} exited early (code {proc.returncode}); see "
                                     f"{self.run_dir}")
            try:
                urllib.request.urlopen(url, timeout=2).read()
                return
            except OSError:
                time.sleep(0.2)
        raise SystemExit(f"{what} did not answer {url} within 30 s; see {self.run_dir}")

    def stop(self) -> None:
        for proc in reversed(self.procs):
            if proc.poll() is None:
                proc.terminate()
        for proc in reversed(self.procs):
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5)
            log(f"stopped pid {proc.pid}")
        self.procs.clear()

    # -- the script's levers --------------------------------------------------------------------

    def reset(self) -> None:
        http("POST", f"{self.bridge_url}/_demo/reset", {})

    def cue(self, name: str) -> None:
        http("POST", f"{self.bridge_url}/_demo/cue/{name}", {})

    def warm_thread_names(self) -> None:
        """Thread names are made in the background after a write-token read of the thread list."""
        url = f"{self.vibe_url}/api/v1/channels/{CHANNEL_ID}/timeline?view=threads&limit=50"
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            page = http("GET", url, token=WRITE_TOKEN)
            threads = page.get("threads") if isinstance(page, dict) else None
            if isinstance(threads, list) and threads and all(
                    isinstance(t, dict) and t.get("display_name") and t.get("summary")
                    for t in threads):
                return
            time.sleep(0.5)
        raise SystemExit("thread names did not arrive from the mock Messages API")


# -- recording ------------------------------------------------------------------------------------


class Recorder:
    """Every compositor frame of one page, with its timestamp, via the DevTools screencast."""

    def __init__(self, context: BrowserContext, page: Page, out: Path, profile: Profile) -> None:
        self.context = context
        self.page = page
        self.out = out
        self.profile = profile
        self.frames: list[tuple[float, str]] = []
        self.marks: dict[str, float] = {}
        self.cdp: CDPSession | None = None
        self.started = 0.0
        self.stopped = 0.0

    async def start(self) -> None:
        if self.out.exists():
            shutil.rmtree(self.out)
        self.out.mkdir(parents=True)
        cdp = await self.context.new_cdp_session(self.page)
        self.cdp = cdp

        def on_frame(params: dict[str, object]) -> None:
            metadata = params.get("metadata")
            stamp = metadata.get("timestamp") if isinstance(metadata, dict) else None
            data = params.get("data")
            if not isinstance(data, str):
                return
            name = f"{len(self.frames):05d}.png"
            (self.out / name).write_bytes(base64.b64decode(data))
            self.frames.append((float(stamp) if isinstance(stamp, (int, float)) else time.time(),
                                name))
            asyncio.ensure_future(
                cdp.send("Page.screencastFrameAck", {"sessionId": params.get("sessionId")}))

        cdp.on("Page.screencastFrame", on_frame)
        self.started = time.time()
        await cdp.send("Page.startScreencast", {"format": "png", "everyNthFrame": 1})
        # The first frame arrives on start; wait for it so the clip never opens on nothing.
        for _ in range(100):
            if self.frames:
                break
            await asyncio.sleep(0.02)

    def mark(self, label: str) -> None:
        self.marks[label] = time.time()

    async def stop(self) -> None:
        self.stopped = time.time()
        if self.cdp is not None:
            await self.cdp.send("Page.stopScreencast")
        await asyncio.sleep(0.2)
        timeline = {
            "started": self.started, "stopped": self.stopped, "marks": self.marks,
            "frames": [[stamp, name] for stamp, name in self.frames],
            "viewport": [self.profile.width, self.profile.height],
        }
        (self.out / "timeline.json").write_text(json.dumps(timeline, indent=1), encoding="utf-8")
        log(f"{self.out.name}: {len(self.frames)} frames over {self.stopped - self.started:.1f} s")


@dataclass
class Stage:
    """What a clip script can do: the page, the services, and the recorder."""

    page: Page
    services: Services
    recorder: Recorder
    desktop: bool
    mouse: list[float] = field(default_factory=lambda: [0.0, 0.0])

    async def pause(self, seconds: float) -> None:
        await asyncio.sleep(seconds)

    async def js(self, expression: str) -> object:
        value: object = await self.page.evaluate(f"() => ({expression})")
        return value

    async def wait(self, predicate: str, timeout: float = 15) -> None:
        await self.page.wait_for_function(f"() => ({predicate})", timeout=timeout * 1000)

    async def tap(self, selector: str) -> None:
        locator = self.page.locator(selector).first
        await locator.scroll_into_view_if_needed()
        if not self.desktop:
            await locator.tap()
            return
        box = await locator.bounding_box()
        if box is None:
            raise RuntimeError(f"{selector} has no box to click")
        await self.glide(box["x"] + box["width"] / 2, box["y"] + box["height"] / 2)
        await asyncio.sleep(0.15)
        await self.page.mouse.down()
        await asyncio.sleep(0.08)
        await self.page.mouse.up()

    async def glide(self, x: float, y: float, seconds: float = 0.7) -> None:
        """Move the (drawn) desktop pointer to x, y in a visible, eased glide."""
        steps = max(8, int(seconds * 40))
        x0, y0 = self.mouse
        for index in range(1, steps + 1):
            t = index / steps
            eased = t * t * (3 - 2 * t)
            await self.page.mouse.move(x0 + (x - x0) * eased, y0 + (y - y0) * eased)
            await asyncio.sleep(seconds / steps)
        self.mouse = [x, y]

    async def type(self, text: str, delay_ms: int = 70) -> None:
        await self.page.keyboard.type(text, delay=delay_ms)

    async def scroll(self, selector: str, dy: int, ms: int) -> None:
        await self.page.evaluate(
            "([s, d, m]) => window.__demoScroll(s, d, m)", [selector, dy, ms])

    async def dismiss_status(self) -> None:
        if await self.page.is_visible("#dismiss-status"):
            await self.page.click("#dismiss-status")


async def open_channel(page: Page, base_url: str) -> None:
    """Load the app and switch it from the voice view to the channel, before recording starts."""
    await page.goto(f"{base_url}/voice", wait_until="load")
    await page.wait_for_function("() => document.getElementById('view-switch')", timeout=15_000)
    await page.click("#view-switch")
    await page.wait_for_function(
        "() => document.querySelectorAll('#discord-log > li').length > 5 && "
        "!document.getElementById('channel-loading').checkVisibility()", timeout=20_000)
    await page.wait_for_timeout(600)
    if await page.is_visible("#dismiss-status"):
        await page.click("#dismiss-status")
    await page.wait_for_timeout(300)


def row(phrase: str) -> str:
    """The channel row whose text holds `phrase` (lower case), in the main list."""
    return f'#discord-log > li.discord-message[data-search*="{phrase}"]'


SCROLLER = "#scroll-area"


# -- the clips ------------------------------------------------------------------------------------
#
# Each clip has a `prepare` (run before the screencast starts: loading, switching views, scrolling
# into place) and a `play` (what is filmed). Pauses are deliberate: long enough to read.


async def prepare_summaries(stage: Stage) -> None:
    await stage.js(f"document.querySelector('{SCROLLER}').scrollTop = 0")
    await stage.pause(0.8)


async def play_summaries(stage: Stage) -> None:
    await stage.pause(1.6)
    await stage.tap("#summarise")
    await stage.wait(f"document.querySelector('{row('plan for today')}')"
                     ".getAttribute('data-summarised') === 'true'")
    await stage.pause(2.4)
    await stage.scroll(SCROLLER, 700, 1300)
    await stage.wait(f"document.querySelector('{row('csv export report')}')"
                     ".getAttribute('data-summarised') === 'true'")
    await stage.pause(2.6)
    stage.recorder.mark("poster")
    await stage.scroll(SCROLLER, 2000, 1400)
    await stage.wait(f"document.querySelector('{row('approval needed')}')"
                     ".getAttribute('data-summarised') === 'true'")
    await stage.pause(2.6)


async def prepare_report(stage: Stage) -> None:
    # Summaries are asked for only for rows near the screen, so put the report there first.
    place = (f"(() => {{ const r = document.querySelector('{row('csv export report')}');"
             f" const s = document.querySelector('{SCROLLER}');"
             " s.scrollTop += r.getBoundingClientRect().top - 330; })()")
    await stage.js(place)
    await stage.pause(0.4)
    await stage.page.click("#summarise")
    await stage.wait(f"document.querySelector('{row('csv export report')}')"
                     ".getAttribute('data-summarised') === 'true'")
    await stage.js(place)
    await stage.pause(0.8)


async def play_report(stage: Stage) -> None:
    await stage.pause(2.2)
    await stage.tap(f"{row('csv export report')} .summary")
    await stage.wait(f"document.querySelector('{row('csv export report')}')"
                     ".getAttribute('data-collapsed') === 'false'")
    await stage.pause(1.4)
    # Bring the opened report up under the floating pill: it then fills the screen.
    delta = await stage.js(f"document.querySelector('{row('csv export report')}')"
                           ".getBoundingClientRect().top - 62")
    await stage.scroll(SCROLLER, int(delta) if isinstance(delta, (int, float)) else 0, 1300)
    stage.recorder.mark("poster")
    await stage.pause(4.6)


async def prepare_live(stage: Stage) -> None:
    await stage.js(f"document.querySelector('{SCROLLER}').scrollTop = 1e7")
    await stage.pause(0.8)


async def play_live(stage: Stage) -> None:
    await stage.pause(2.0)
    stage.services.cue("worker-report")
    await stage.wait(f"document.querySelector('{row('regression test added')}')")
    await stage.wait(f"document.querySelector('{row('back in the pool')}')", timeout=20)
    await stage.pause(1.6)
    if await stage.page.is_visible("#jump-newest"):
        await stage.tap("#jump-newest")
    await stage.pause(1.2)
    stage.recorder.mark("poster")
    await stage.pause(1.4)


async def prepare_reply(stage: Stage) -> None:
    await stage.js(f"document.querySelector('{SCROLLER}').scrollTop = 1e7")
    await stage.pause(0.8)


async def play_reply(stage: Stage) -> None:
    await stage.pause(1.6)
    await stage.tap(f"{row('approval needed')} .reply-button")
    await stage.wait("document.getElementById('reply-text').checkVisibility()")
    await stage.pause(1.4)
    await stage.tap("#reply-text")
    await stage.pause(0.3)
    await stage.type("Yes, land it. Thanks!")
    await stage.pause(0.9)
    await stage.tap("#reply-send")
    await stage.wait(f"document.querySelector('{row('yes, land it')}')")
    await stage.pause(1.4)
    await stage.dismiss_status()
    await stage.wait(f"document.querySelector('{row('toolchain bump is on main')}')", timeout=20)
    await stage.pause(1.2)
    await stage.wait(f"document.querySelector('{row('all three of today')}')", timeout=20)
    await stage.pause(1.0)
    if await stage.page.is_visible("#jump-newest"):
        await stage.tap("#jump-newest")
    await stage.pause(1.2)
    stage.recorder.mark("poster")
    await stage.pause(1.6)


async def prepare_desktop(stage: Stage) -> None:
    await stage.page.click("#column-toggle")
    await stage.wait("[...document.querySelectorAll('#side-cards .side-card-text')]"
                     ".some((n) => n.textContent.startsWith('csv-export-review'))")
    await stage.page.mouse.move(760, 520)
    stage.mouse = [760.0, 520.0]
    await stage.pause(0.8)


async def play_desktop(stage: Stage) -> None:
    await stage.pause(1.8)
    # A click on a card only highlights it and shows it whole; its replies button opens the thread.
    row = "#side-cards .side-card-row:has-text('csv-export-review')"
    await stage.tap(f"{row} .side-card")
    await stage.pause(1.0)
    await stage.tap(f"{row} .side-card-open")
    await stage.wait("document.getElementById('side-compose-text').checkVisibility()")
    await stage.pause(2.0)
    await stage.scroll("#side-scroll", 2000, 1300)
    await stage.pause(0.8)
    await stage.tap("#side-compose-text")
    await stage.type("Ship it with tomorrow's release.", delay_ms=55)
    await stage.pause(0.6)
    await stage.tap("#side-send")
    await stage.pause(0.4)
    await stage.glide(960, 330, 0.6)
    await stage.pause(0.4)
    await stage.dismiss_status()
    await stage.wait("[...document.querySelectorAll('#side-log li')]"
                     ".some((n) => (n.dataset.search || '').includes('tagged for tomorrow'))",
                     timeout=20)
    await stage.pause(0.5)
    await stage.scroll("#side-scroll", 2000, 900)
    stage.recorder.mark("poster")
    await stage.pause(2.6)


@dataclass
class Clip:
    name: str
    profile: Profile
    prepare: Callable[[Stage], Awaitable[None]]
    play: Callable[[Stage], Awaitable[None]]
    needs_thread_names: bool = False


CLIPS = [
    Clip("channel-summaries", PHONE, prepare_summaries, play_summaries),
    Clip("long-report", PHONE, prepare_report, play_report),
    Clip("live-message", PHONE, prepare_live, play_live),
    Clip("reply-from-phone", PHONE, prepare_reply, play_reply),
    Clip("desktop-threads", DESKTOP, prepare_desktop, play_desktop, needs_thread_names=True),
]


async def record(services: Services, clips: list[Clip], frames_dir: Path, headed: bool) -> None:
    from playwright.async_api import async_playwright

    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch(headless=not headed)
        try:
            for clip in clips:
                log(f"clip {clip.name}")
                services.reset()
                if clip.needs_thread_names:
                    services.warm_thread_names()
                profile = clip.profile
                context = await browser.new_context(
                    viewport={"width": profile.width, "height": profile.height},
                    device_scale_factor=profile.scale, is_mobile=profile.mobile,
                    has_touch=profile.mobile, color_scheme="dark", timezone_id=ZONE,
                    locale="en-US")
                page = await context.new_page()
                errors: list[str] = []
                page.on("pageerror", lambda error: errors.append(str(error)))
                await page.add_init_script(
                    f"try {{ localStorage.setItem('vibe-talk.token', {json.dumps(WRITE_TOKEN)}); }}"
                    " catch (e) {}")
                await page.add_init_script(
                    OVERLAY_JS.replace("__DESKTOP__", "false" if profile.mobile else "true"))
                await open_channel(page, services.vibe_url)
                recorder = Recorder(context, page, frames_dir / clip.name, profile)
                stage = Stage(page, services, recorder, desktop=not profile.mobile)
                await clip.prepare(stage)
                await recorder.start()
                try:
                    await clip.play(stage)
                finally:
                    await recorder.stop()
                    await context.close()
                if errors:
                    raise SystemExit(f"{clip.name}: the page threw: {errors[:3]}")
        finally:
            await browser.close()


# -- encoding -------------------------------------------------------------------------------------


@dataclass
class Timeline:
    """What the recorder wrote beside a clip's frames."""

    started: float
    stopped: float
    marks: dict[str, float]
    frames: list[tuple[float, str]]

    @staticmethod
    def load(path: Path) -> Timeline:
        raw: object = json.loads(path.read_text(encoding="utf-8"))
        if not isinstance(raw, dict):
            raise SystemExit(f"{path} is not a timeline")

        def number(value: object) -> float:
            if not isinstance(value, (int, float)):
                raise SystemExit(f"{path} holds a non-number where a time belongs")
            return float(value)

        marks = raw.get("marks")
        frames = raw.get("frames")
        return Timeline(
            started=number(raw.get("started")),
            stopped=number(raw.get("stopped")),
            marks={str(k): number(v) for k, v in marks.items()} if isinstance(marks, dict) else {},
            frames=[(number(f[0]), str(f[1])) for f in frames
                    if isinstance(f, list) and len(f) == 2] if isinstance(frames, list) else [],
        )


@dataclass
class Encoding:
    ffmpeg: str
    fps: int
    phone_width: int
    desktop_width: int


def encode(clip: Clip, frames_dir: Path, out_dir: Path, enc: Encoding) -> dict[str, object]:
    source = frames_dir / clip.name
    timeline = Timeline.load(source / "timeline.json")
    frames = timeline.frames
    if not frames:
        raise SystemExit(f"{clip.name}: no frames were captured")
    # Frame stamps are the compositor's; the end of the clip is the recorder's stop, converted
    # onto the same clock through the first frame (both clocks are wall time in practice).
    offset = frames[0][0] - timeline.started if abs(frames[0][0] - timeline.started) > 5 else 0.0
    end = timeline.stopped + offset
    # Resample the compositor's irregular frames (as fast as 60 a second while something moves,
    # none while nothing does) onto a constant clock: at each tick, whatever frame was on screen.
    # A constant rate maps exactly onto a GIF's centisecond delays; 10 frames a second is 10 cs.
    sequence = source / "sequence"
    if sequence.exists():
        shutil.rmtree(sequence)
    sequence.mkdir()
    first = frames[0][0]
    ticks = max(1, int(round((end - first) * enc.fps)))
    shown = 0
    for tick in range(ticks):
        at = first + tick / enc.fps
        while shown + 1 < len(frames) and frames[shown + 1][0] <= at:
            shown += 1
        os.symlink(source / frames[shown][1], sequence / f"{tick:05d}.png")
    width = enc.phone_width if clip.profile.mobile else enc.desktop_width
    gif = out_dir / f"{clip.name}.gif"
    # One palette for the whole clip (stats_mode=full) and no dithering: the interface is flat
    # colour and antialiased text, which 256 colours hold cleanly, and an undithered frame differs
    # from the next only where the page changed, which is what keeps a GIF small.
    graph = (f"scale={width}:-1:flags=lanczos,split[a][b];"
             "[a]palettegen=max_colors=256:stats_mode=full[p];"
             "[b][p]paletteuse=dither=none:diff_mode=rectangle")
    subprocess.run([enc.ffmpeg, "-loglevel", "error", "-y", "-framerate", str(enc.fps),
                    "-i", str(sequence / "%05d.png"), "-filter_complex", graph, "-loop", "0",
                    str(gif)], check=True)
    # The poster: the frame on screen at the clip's "poster" mark (or two thirds in).
    at = timeline.marks.get(
        "poster", timeline.started + (timeline.stopped - timeline.started) * 2 / 3) + offset
    chosen = frames[0][1]
    for stamp, name in frames:
        if stamp <= at:
            chosen = name
    poster = out_dir / f"{clip.name}-poster.png"
    subprocess.run([enc.ffmpeg, "-loglevel", "error", "-y", "-i", str(source / chosen),
                    "-vf", f"scale={width}:-1:flags=lanczos", str(poster)], check=True)
    probe = subprocess.run([enc.ffmpeg, "-hide_banner", "-i", str(gif)], capture_output=True,
                           text=True)
    duration = round(ticks / enc.fps, 1)
    size = gif.stat().st_size
    height = _height(probe.stderr)
    log(f"{gif.name}: {size / 1e6:.2f} MB, {width}x{height}, {duration} s")
    return {"gif": str(gif), "bytes": size, "poster": str(poster), "duration_s": duration,
            "width": width, "height": height, "frames_captured": len(frames)}


def _height(ffmpeg_banner: str) -> int:
    match = re.search(r"Video: gif.*?, (\d+)x(\d+)", ffmpeg_banner)
    return int(match.group(2)) if match else 0


# -- main -----------------------------------------------------------------------------------------


def default_out() -> Path:
    """Outside the checkout: a full run leaves about 100 MB of raw frames behind."""
    cache = os.environ.get("XDG_CACHE_HOME") or str(Path.home() / ".cache")
    return Path(cache) / "agent-utils" / "vibe-talk-demo"


def publish(clips: list[Clip], out: Path) -> None:
    """Copy each clip's GIF and poster to where the website uses them."""
    SITE_MEDIA.mkdir(parents=True, exist_ok=True)
    for clip in clips:
        for name in (f"{clip.name}.gif", f"{clip.name}-poster.png"):
            shutil.copyfile(out / name, SITE_MEDIA / name)
            log(f"published {SITE_MEDIA / name}")
    log("now run `python3 scripts/check_site.py`, and check each clip's width and height in "
        "site/vibe-talk.html if --phone-width or --desktop-width changed")


def main() -> int:
    parser = argparse.ArgumentParser(
        description=(__doc__ or "").split("\n\n")[0],
        epilog="Example, from the repository root: python3 site/demos/vibe-talk/record_demo.py "
               "--only reply-from-phone --publish")
    default_binary = os.environ.get("VIBE_TALK_BIN")
    if not default_binary and DEFAULT_BINARY.is_file():
        default_binary = str(DEFAULT_BINARY)
    parser.add_argument("--vibe-talk", type=Path, default=default_binary,
                        help="the vibe-talk release binary (default: $VIBE_TALK_BIN, else "
                             "vibe-talk/target/release/vibe-talk in this repository). Build it "
                             "first with `cargo build --release`: web/ is compiled into it")
    parser.add_argument("--out", type=Path, default=default_out(),
                        help="where GIFs, posters, frames and the run's logs go (default: "
                             "$XDG_CACHE_HOME/agent-utils/vibe-talk-demo, else "
                             "~/.cache/agent-utils/vibe-talk-demo)")
    parser.add_argument("--publish", action="store_true",
                        help="after encoding, copy each encoded clip's GIF and poster into "
                             "site/media/vibe-talk/ in this repository")
    parser.add_argument("--only", default="",
                        help="comma-separated clip names to record (default: all): "
                             + ", ".join(clip.name for clip in CLIPS))
    parser.add_argument("--port-base", type=int, default=19310,
                        help="vibe-talk's loopback port; the bridge and the mock Messages API "
                             "take the next two (default 19310)")
    parser.add_argument("--fps", type=int, default=10, help="GIF frame rate (default 10)")
    parser.add_argument("--phone-width", type=int, default=412,
                        help="GIF width for phone clips, in pixels (default 412)")
    parser.add_argument("--desktop-width", type=int, default=800,
                        help="GIF width for desktop clips, in pixels (default 800)")
    parser.add_argument("--encode-only", action="store_true",
                        help="re-encode GIFs from frames already recorded under --out")
    parser.add_argument("--headed", action="store_true", help="show the browser while recording")
    parser.add_argument("--ffmpeg", default=shutil.which("ffmpeg") or "ffmpeg",
                        help="the ffmpeg binary (default: ffmpeg on PATH)")
    args = parser.parse_args()

    wanted = [name for name in args.only.split(",") if name]
    unknown = sorted(set(wanted) - {clip.name for clip in CLIPS})
    if unknown:
        parser.error(f"unknown clip(s): {', '.join(unknown)}")
    clips = [clip for clip in CLIPS if not wanted or clip.name in wanted]
    out = args.out.resolve()
    frames_dir = out / "frames"
    out.mkdir(parents=True, exist_ok=True)

    if not shutil.which(args.ffmpeg) and not Path(args.ffmpeg).is_file():
        parser.error(f"ffmpeg not found ({args.ffmpeg}); install it or pass --ffmpeg")
    if not args.encode_only:
        if not args.vibe_talk or not Path(args.vibe_talk).is_file():
            parser.error("--vibe-talk (or $VIBE_TALK_BIN) must name the built vibe-talk binary")
        try:
            import playwright.async_api  # noqa: F401
        except ImportError:
            parser.error("the python3 'playwright' package is missing: pip install playwright && "
                         "python3 -m playwright install chromium")
        fonts = HERE / "fonts.conf"
        if fonts.is_file():
            # Chromium resolves system-ui through fontconfig; see fonts.conf.
            os.environ["FONTCONFIG_FILE"] = str(fonts)
        services = Services(Path(args.vibe_talk).resolve(), out / "run", args.port_base)
        try:
            services.start()
            asyncio.run(record(services, clips, frames_dir, args.headed))
        finally:
            services.stop()

    enc = Encoding(args.ffmpeg, args.fps, args.phone_width, args.desktop_width)
    listing = out / "clips.json"
    # Keep what an earlier run said about clips not encoded this time.
    known: dict[str, dict[str, object]] = {}
    previous: object = json.loads(listing.read_text(encoding="utf-8")) if listing.is_file() else []
    for entry in previous if isinstance(previous, list) else []:
        gif = entry.get("gif") if isinstance(entry, dict) else None
        if isinstance(entry, dict) and isinstance(gif, str):
            known[Path(gif).stem] = {str(key): value for key, value in entry.items()}
    for clip in clips:
        known[clip.name] = encode(clip, frames_dir, out, enc)
    results = [known[clip.name] for clip in CLIPS if clip.name in known]
    listing.write_text(json.dumps(results, indent=1), encoding="utf-8")
    total = sum(size for result in results if isinstance(size := result.get("bytes"), int))
    log(f"total {total / 1e6:.2f} MB in {len(results)} GIF(s), listed in {listing}")
    if args.publish:
        publish(clips, out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
