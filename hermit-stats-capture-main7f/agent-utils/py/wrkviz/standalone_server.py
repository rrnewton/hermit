#!/usr/bin/env python3
"""Dependency-free static timeline server, copied verbatim into generated archives."""

from __future__ import annotations

import argparse
import functools
import gzip
import hashlib
import http.server
import io
import os
import re
import threading
import urllib.parse
import webbrowser
import zlib
from email.utils import formatdate
from pathlib import Path
from typing import BinaryIO


_CONTENT_HASHED_NAME = re.compile(r"^[0-9a-f]{64}(?:\.[A-Za-z0-9][A-Za-z0-9._-]*)?$")
_REVALIDATE = "public, no-cache"
_IMMUTABLE = "public, max-age=31536000, immutable"


def accepts_gzip(header: str) -> bool:
    """Return whether an HTTP ``Accept-Encoding`` value permits gzip."""

    qualities: dict[str, float] = {}
    for raw_item in header.split(","):
        parts = [part.strip() for part in raw_item.split(";")]
        coding = parts[0].lower()
        if not coding:
            continue
        quality = 1.0
        for parameter in parts[1:]:
            name, separator, raw_value = parameter.partition("=")
            if separator and name.strip().lower() == "q":
                try:
                    quality = float(raw_value.strip())
                except ValueError:
                    quality = 0.0
        qualities[coding] = quality if 0.0 <= quality <= 1.0 else 0.0
    if "gzip" in qualities:
        return qualities["gzip"] > 0.0
    return qualities.get("*", 0.0) > 0.0


def cache_control_for_path(path: Path) -> str:
    """Choose conservative caching unless the filename is a complete content digest."""

    return _IMMUTABLE if _CONTENT_HASHED_NAME.fullmatch(path.name) else _REVALIDATE


#: Digests already computed, keyed by identity *and* by the two pieces of metadata that change
#: when a file is rewritten. A rebuild replaces a file by `os.replace`, which gives it a new inode
#: and a new mtime, so a stale entry cannot be hit: the key stops matching before the content does.
_ETAG_CACHE: dict[tuple[str, int, int, int], str] = {}
_ETAG_CACHE_LOCK = threading.Lock()

#: Bounded because this process outlives any one page load. Small: the entry point, the bootstrap
#: and every shard a session touches fit easily, and the cost of a miss is one re-read.
_ETAG_CACHE_LIMIT = 512

#: The largest range this server will answer with a 206. Above it the whole representation is
#: sent instead, which is a legal answer to any range request -- a server is always permitted to
#: ignore ``Range``.
#:
#: The bound exists because the selected bytes are buffered rather than streamed. Streaming them
#: would mean interposing a length-limited file object between the open handle and the base
#: class's copy loop, and every way to spell that in this file's typing contract needs either a
#: Liskov-violating override of ``copyfile`` or a suppression, and this package has neither
#: anywhere. Buffering costs one allocation the size of the answer, and the answer this exists to
#: serve is one gzip member of a schema-3 shard: the codec targets about a mebibyte per member and
#: the largest ever measured was 796,663 bytes, so the ceiling is more than an order of magnitude
#: above the case and far below the 246,973,399-byte monolith that made an unbounded buffer worth
#: worrying about.
_MAX_RANGE_BYTES = 16 << 20

_RANGE = re.compile(r"^bytes=(\d*)-(\d*)$")

#: Ceiling on inflating a `.gz`-only resource for a client that refused gzip. Generous, because
#: the point is to bound a memory bomb rather than to ration the fallback: the largest such
#: resource measured is the 49,264,239-byte artifact catalogue, and the fallback is only reached
#: by a client that sent `Accept-Encoding: identity` -- which no browser does.
_MAX_INFLATE_BYTES = 512 << 20

#: Path components this server will not serve from, at any depth, under any name.
#:
#: `source_snapshots` is the vendor material ingestion reads -- 6,524,876,923 bytes across 995
#: files on the measured archive, 67.5% of everything under the root. A new archive keeps it
#: outside the served tree entirely, in the store beside the archive, so for those this rule
#: never fires. It exists for the archive nobody has migrated yet, where the bytes are still
#: physically inside `--output` and are therefore fetchable by anyone who can guess a filename.
#:
#: A refusal by *component name*, anywhere beneath the root, rather than by matching the exact
#: `teams/<slug>/source_snapshots/` shape. This file is copied verbatim into every archive and has
#: no notion of teams, and the loose rule is the safe direction to be wrong in: the only thing it
#: can over-refuse is a file the archive has no reason to contain, while the strict rule would
#: under-refuse the moment somebody copied or symlinked a store somewhere unexpected.
#:
#: Note what this does *not* claim to be. It is a serving policy, not a security boundary: the
#: files are still on the operator's disk with their ordinary permissions, and the server is
#: loopback-only. The claim is narrower and is the one that was asked for -- nothing under the
#: snapshot root is reachable over HTTP.
_UNSERVABLE_COMPONENTS = frozenset({"source_snapshots"})


def _strong_etag(path: Path, stat_result: os.stat_result) -> str:
    """The file's SHA-256, computed at most once per distinct version of it.

    The digest is kept, rather than the validator weakened, because a strong validator is the
    thing that makes a range request safe: a client that fetches member 3 of a shard on Tuesday
    and member 7 on Wednesday needs to know it is reading one file, and `mtime` alone cannot say
    so at one-second resolution. What is removed is re-deriving it. Hashing on every request made
    a 100-byte range over the 246,973,399-byte schema-1 monolith cost a full read before the first
    byte went out -- and made a 304, whose entire purpose is to send nothing, cost exactly as much
    as a 200.
    """

    key = (str(path), stat_result.st_dev, stat_result.st_ino, stat_result.st_mtime_ns)
    with _ETAG_CACHE_LOCK:
        cached = _ETAG_CACHE.get(key)
    if cached is not None:
        return cached
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while True:
            chunk = handle.read(1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
    etag = '"sha256-' + digest.hexdigest() + '"'
    with _ETAG_CACHE_LOCK:
        if len(_ETAG_CACHE) >= _ETAG_CACHE_LIMIT:
            # Insertion-ordered, so this drops the least recently *added* entry. Not an LRU, and
            # deliberately not: an LRU here would be more code guarding a cost that is one file
            # read, on a loopback server for one reader.
            _ETAG_CACHE.pop(next(iter(_ETAG_CACHE)))
        _ETAG_CACHE[key] = etag
    return etag


def parse_byte_range(header: str, size: int) -> tuple[int, int] | None:
    """Resolve one ``Range`` header against a *size*-byte representation.

    Returns the inclusive ``(first, last)`` byte offsets, or ``None`` when the header names no
    range this server will honour -- in which case the caller sends the whole thing, which is
    always a correct answer to a range request.

    **Only a single range is honoured.** A comma-separated list would mean a
    ``multipart/byteranges`` body, and the one client this exists for -- a reader fetching one
    gzip member of a shard by the offset its sidecar published -- asks for exactly one range and
    would have to grow a MIME parser to read the reply. Refusing by falling back to 200 is a
    correct response to a request this cannot improve on, whereas implementing multipart is a
    second body format to get wrong.

    A suffix range (``bytes=-500``) is honoured, because "the last N bytes" is how a reader
    recovers a shard's final member when it has no sidecar at all.
    """

    match = _RANGE.fullmatch(header.strip())
    if match is None:
        return None
    raw_first, raw_last = match.group(1), match.group(2)
    if not raw_first and not raw_last:
        return None
    if not raw_first:
        length = int(raw_last)
        if length == 0:
            return None
        return (max(size - length, 0), size - 1)
    first = int(raw_first)
    if first >= size:
        # Unsatisfiable, and distinct from unparseable: the caller answers 416 rather than 200,
        # because a client asking past the end has a stale idea of the file and should be told.
        return (size, size - 1)
    last = size - 1 if not raw_last else min(int(raw_last), size - 1)
    if last < first:
        return None
    return (first, last)


def _etag_matches(raw_header: str, etag: str) -> bool:
    for raw_value in raw_header.split(","):
        value = raw_value.strip()
        if value == "*" or value == etag or value.removeprefix("W/") == etag:
            return True
    return False


def _inflate(path: Path) -> bytes | None:
    """Return the decompressed contents of *path*, or ``None`` if it is not usable as one."""

    try:
        with gzip.open(path, "rb") as compressed:
            body = compressed.read(_MAX_INFLATE_BYTES + 1)
    except (OSError, EOFError, gzip.BadGzipFile, zlib.error):
        return None
    return None if len(body) > _MAX_INFLATE_BYTES else body


class TimelineRequestHandler(http.server.SimpleHTTPRequestHandler):
    """Serve a resource from its identity bytes or from a ``.gz`` that stands in for them.

    A ``.gz`` companion is a *complete* representation here, not a transfer-time optimisation
    layered over a mandatory plain twin. When ``foo.json`` is absent and ``foo.json.gz`` is
    present, the compressed member is what the archive stores and this server answers
    ``GET /foo.json`` from it -- sending the stored bytes under ``Content-Encoding: gzip`` to the
    ordinary client, and inflating them only for a client that explicitly refused gzip or asked
    for a byte range.

    That is what lets the archive stop writing the twin. Both files existed because the identity
    copy was the resource and the ``.gz`` was an accelerator, so the plain bytes had to be on disk
    even though no browser ever received them: on the measured archive that was 203.4 MiB of
    per-phase detail and a 47.0 MiB artifact catalogue kept for a fallback nobody took. Inverting
    which one is mandatory costs one inflate on the fallback path and removes both.
    """

    _cache_control_sent = False

    def list_directory(self, path: str | os.PathLike[str]) -> io.BytesIO | None:
        """Never expose generated-object or transcript filenames by directory listing."""

        del path
        self.send_error(404, "File not found")
        return None

    def end_headers(self) -> None:
        """Add browser-safety headers and a fallback revalidation policy."""

        if not self._cache_control_sent:
            self.send_header("Cache-Control", _REVALIDATE)
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Referrer-Policy", "no-referrer")
        super().end_headers()
        self._cache_control_sent = False

    def _send_cache_control(self, source_path: Path) -> None:
        self.send_header("Cache-Control", cache_control_for_path(source_path))
        self._cache_control_sent = True

    def _is_unservable(self, path: Path) -> bool:
        """Whether *path* lies in a tree this server refuses to publish.

        Checked on the path `translate_path` produced, after its own normalization, so a request
        spelled with `%2e%2e`, doubled slashes or percent-encoded components is judged by where it
        actually lands rather than by how it was written.
        """

        return any(part in _UNSERVABLE_COMPONENTS for part in path.parts)

    def _selected_source_path(self) -> Path | None:
        source_path = Path(self.translate_path(self.path))
        if not source_path.is_dir():
            return source_path
        request_path = urllib.parse.urlsplit(self.path).path
        if not request_path.endswith("/"):
            return None
        for index_name in ("index.html", "index.htm"):
            candidate = source_path / index_name
            if candidate.is_file():
                return candidate
        return None

    def send_head(self) -> BinaryIO | None:
        """Open the selected representation and emit content-negotiated headers."""

        # First, before the file is opened, before content negotiation, and before the base class
        # can be reached: a refusal that ran later could be bypassed by any request shape that
        # falls through to `super().send_head()`.
        #
        # 404 rather than 403, matching `list_directory` directly above, which has answered 404
        # for a real directory since it was written. The reason is the same: this server's error
        # responses should not be a discovery oracle. "Forbidden" tells a client the path exists.
        if self._is_unservable(Path(self.translate_path(self.path))):
            self.send_error(404, "File not found")
            return None
        source_path = self._selected_source_path()
        if source_path is None:
            return super().send_head()
        sidecar_path = source_path.with_name(source_path.name + ".gz")
        source_ok = source_path.is_file() and not source_path.is_symlink()
        sidecar_ok = sidecar_path.is_file() and not sidecar_path.is_symlink()
        if not source_ok and not sidecar_ok:
            return super().send_head()
        # True when the `.gz` is the only thing on disk, so it *is* the resource rather than a
        # companion to one. The identity representation still exists as far as HTTP is concerned;
        # it is simply materialised on demand instead of stored.
        gz_only = sidecar_ok and not source_ok
        vary = sidecar_ok
        raw_range = self.headers.get("Range", "")
        # A range is served from the identity representation, always. A byte range over a `.gz`
        # companion would be a range over bytes the client never asked for by name and cannot
        # decode in isolation -- half a gzip stream is not half a document -- so content coding
        # and byte ranges are not combined. Nothing is lost for the case this exists to serve: a
        # schema-3 shard *is* the `.gz`, it is requested under its own `.gz` name, and a range
        # over it addresses exactly the bytes its sidecar index named.
        use_gzip = (
            sidecar_ok
            and not raw_range
            and accepts_gzip(self.headers.get("Accept-Encoding", ""))
        )
        handle: BinaryIO | None = None
        inflated: bytes | None = None
        if gz_only and not use_gzip:
            inflated = _inflate(sidecar_path)
            if inflated is None:
                self.send_error(404, "File not found")
                return None
        try:
            if inflated is not None:
                selected_stat = sidecar_path.stat()
                handle = io.BytesIO(inflated)
            else:
                handle = (sidecar_path if use_gzip else source_path).open("rb")
                selected_stat = os.fstat(handle.fileno())
            source_stat = (sidecar_path if gz_only else source_path).stat()
        except OSError:
            if handle is not None:
                handle.close()
            self.send_error(404, "File not found")
            return None

        if inflated is not None:
            # A distinct validator, because these are distinct bytes. Deriving it from the stored
            # member's digest keeps it strong and free, and the suffix keeps a client that holds
            # the inflated form from ever matching it against the compressed one.
            etag = _strong_etag(sidecar_path, selected_stat)[:-1] + '-identity"'
        else:
            etag = _strong_etag(sidecar_path if use_gzip else source_path, selected_stat)
        if _etag_matches(self.headers.get("If-None-Match", ""), etag):
            handle.close()
            self.send_response(304)
            self.send_header("ETag", etag)
            self.send_header("Last-Modified", formatdate(source_stat.st_mtime, usegmt=True))
            self._send_cache_control(source_path)
            if vary:
                self.send_header("Vary", "Accept-Encoding")
            if use_gzip:
                self.send_header("Content-Encoding", "gzip")
            self.end_headers()
            return None

        # The size of the *selected representation*, which for a materialised identity body is
        # what was inflated, not what the stored member weighs. Taking `st_size` here would
        # advertise the compressed length against uncompressed bytes and resolve every range
        # against the wrong extent.
        size = len(inflated) if inflated is not None else selected_stat.st_size
        # `If-Range` makes a range conditional on the representation being the one the client
        # already has part of. If it does not match, the correct answer is the whole thing, not a
        # slice of a different file stitched onto the client's stale prefix.
        if_range = self.headers.get("If-Range", "")
        selection = (
            parse_byte_range(raw_range, size)
            if raw_range and (not if_range or _etag_matches(if_range, etag))
            else None
        )
        if selection is not None and selection[1] >= selection[0] and (
            selection[1] - selection[0] + 1 > _MAX_RANGE_BYTES
        ):
            selection = None
        if selection is not None and selection[1] < selection[0]:
            handle.close()
            self.send_response(416)
            self.send_header("Content-Range", f"bytes */{size}")
            self.send_header("Content-Length", "0")
            self.send_header("ETag", etag)
            self.send_header("Accept-Ranges", "bytes")
            self._send_cache_control(source_path)
            self.end_headers()
            return None

        self.send_response(206 if selection is not None else 200)
        self.send_header("Content-Type", self.guess_type(str(source_path)))
        if selection is None:
            self.send_header("Content-Length", str(size))
        else:
            first, last = selection
            handle.seek(first)
            body = handle.read(last - first + 1)
            handle.close()
            handle = io.BytesIO(body)
            self.send_header("Content-Range", f"bytes {first}-{last}/{size}")
            self.send_header("Content-Length", str(len(body)))
        self.send_header("Last-Modified", formatdate(source_stat.st_mtime, usegmt=True))
        self.send_header("ETag", etag)
        # Advertised on every response, including the ones with no range, because a client that
        # cannot see the capability will not use it -- and the schema-3 layout exists precisely
        # so that a reader can ask for one member instead of a shard.
        self.send_header("Accept-Ranges", "bytes")
        self._send_cache_control(source_path)
        if vary:
            self.send_header("Vary", "Accept-Encoding")
        if use_gzip:
            self.send_header("Content-Encoding", "gzip")
        self.end_headers()
        return handle

    def log_message(self, format: str, *args: object) -> None:
        """Suppress routine index requests while retaining other request logs."""

        if self.command != "GET" or self.path not in ("/", "/index.html"):
            super().log_message(format, *args)


def make_static_server(
    root: Path, host: str = "127.0.0.1", port: int = 8765
) -> http.server.ThreadingHTTPServer:
    """Create a threaded static server rooted at *root*."""

    handler = functools.partial(TimelineRequestHandler, directory=str(root.resolve()))
    return http.server.ThreadingHTTPServer((host, port), handler)


def _main() -> None:
    """Serve the generated archive containing this script."""

    parser = argparse.ArgumentParser(
        description="Serve this timeline on loopback and optionally open it in a browser."
    )
    parser.add_argument(
        "--port", type=int, default=8765, help="loopback port (default: %(default)s)"
    )
    parser.add_argument("--open", action="store_true", help="open the timeline in a browser")
    args = parser.parse_args()
    root = Path(__file__).resolve().parent
    server = make_static_server(root, port=args.port)
    url = f"http://127.0.0.1:{server.server_address[1]}/"
    print(f"Serving {root} at {url}")
    if args.open:
        threading.Timer(0.2, lambda: webbrowser.open(url)).start()
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == "__main__":
    _main()
