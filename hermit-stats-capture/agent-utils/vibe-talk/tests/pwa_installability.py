#!/usr/bin/env python3
"""Ask Chromium's DevTools protocol whether /voice is installable.

By default this serves the checked-out web assets from a loopback origin, which Chromium treats as
secure. Pass --url to inspect a deployed copy instead. This is a browser-engine check, not proof
that a physical Android launcher accepted and opened the installation.
"""

from __future__ import annotations

import argparse
import json
import mimetypes
import os
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import unquote, urlsplit


WEB_ROOT = Path(__file__).resolve().parents[1] / "web"


class AssetHandler(BaseHTTPRequestHandler):
    """Serve only the public app assets, with the production cache policy."""

    def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler's API
        request_path = unquote(urlsplit(self.path).path)
        relative = "voice.html" if request_path == "/voice" else request_path.lstrip("/")
        if not relative:
            relative = "index.html"
        target = (WEB_ROOT / relative).resolve()
        if WEB_ROOT not in target.parents or not target.is_file():
            self.send_error(404)
            return

        body = target.read_bytes()
        content_type = mimetypes.guess_type(target.name)[0] or "application/octet-stream"
        if target.name == "manifest.webmanifest":
            content_type = "application/manifest+json"
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, _format: str, *_args: object) -> None:
        pass


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--url",
        help="deployed /voice URL to inspect (default: serve this checkout on loopback)",
    )
    parser.add_argument(
        "--browser-executable",
        default=os.environ.get("VIBE_TALK_CHROMIUM"),
        help="Chrome/Chromium executable (default: Playwright's bundled Chromium)",
    )
    return parser.parse_args()


def main() -> int:
    args = arguments()
    try:
        from playwright.sync_api import sync_playwright
    except ImportError as error:
        raise SystemExit(
            "Python Playwright is required: python3 -m pip install --user playwright && "
            "python3 -m playwright install chromium"
        ) from error

    server: ThreadingHTTPServer | None = None
    thread: threading.Thread | None = None
    url = args.url
    if url is None:
        server = ThreadingHTTPServer(("127.0.0.1", 0), AssetHandler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        url = f"http://127.0.0.1:{server.server_port}/voice"

    try:
        with sync_playwright() as playwright:
            with tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-") as profile:
                # `browser.new_context()` is incognito by design, and Chrome correctly reports
                # `in-incognito` as an installation blocker there. A throwaway persistent profile
                # is still isolated while exercising the ordinary installability path.
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
                page = context.pages[0]
                session = context.new_cdp_session(page)
                session.send("Page.enable")
                page.goto(url, wait_until="load")

                manifest: dict[str, object] = {}
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    manifest = session.send("Page.getAppManifest")
                    if manifest.get("url"):
                        break
                    page.wait_for_timeout(100)

                errors = manifest.get("errors", [])
                installability = session.send("Page.getInstallabilityErrors").get(
                    "installabilityErrors", []
                )
                browser_version = session.send("Browser.getVersion")["product"]
                context.close()

        if not manifest.get("url"):
            raise AssertionError(f"Chrome did not discover a web app manifest at {url}")
        if errors:
            raise AssertionError(f"Chrome reported manifest errors:\n{json.dumps(errors, indent=2)}")
        if installability:
            raise AssertionError(
                "Chrome did not consider the page installable:\n"
                + json.dumps(installability, indent=2)
            )

        parsed = json.loads(str(manifest.get("data", "{}")))
        if parsed.get("display_override") != ["standalone"]:
            raise AssertionError(f"Chrome read the wrong display_override: {parsed}")
        if parsed.get("display") != "browser":
            raise AssertionError(f"Chrome read the wrong display fallback: {parsed}")

        print(f"Chrome {browser_version}: /voice has no manifest or installability errors")
        print("display_override=[standalone], display=browser")
        print("CDP verification passed; physical Android installation was not exercised")
        return 0
    finally:
        if server is not None:
            server.shutdown()
            server.server_close()
        if thread is not None:
            thread.join()


if __name__ == "__main__":
    raise SystemExit(main())
