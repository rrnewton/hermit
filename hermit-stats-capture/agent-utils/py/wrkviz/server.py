"""Loopback-only static server for a generated timeline archive."""

from __future__ import annotations

import http.server
import threading
import webbrowser
from pathlib import Path

from wrkviz.standalone_server import TimelineRequestHandler, make_static_server


QuietHandler = TimelineRequestHandler


def make_server(
    archive: Path, host: str = "127.0.0.1", port: int = 8765
) -> http.server.ThreadingHTTPServer:
    """Create, but do not start, a server rooted at *archive*."""

    root = archive.resolve()
    if not root.is_dir():
        raise ValueError(f"archive directory does not exist: {archive}")
    if not (root / "index.html").is_file():
        raise ValueError(f"archive has no index.html; run `wrkviz build`: {root}")
    return make_static_server(root, host, port)


def serve(archive: Path, host: str, port: int, open_browser: bool) -> None:
    """Serve until interrupted, optionally opening the system browser."""

    server = make_server(archive, host, port)
    actual_port = int(server.server_address[1])
    url = f"http://{host}:{actual_port}/"
    print(f"wrkviz: serving {archive.resolve()} at {url}")
    if open_browser:
        threading.Timer(0.2, lambda: webbrowser.open(url)).start()
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nwrkviz: stopped")
    finally:
        server.server_close()
