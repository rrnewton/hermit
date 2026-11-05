from __future__ import annotations

import gzip
import http.client
import threading
from pathlib import Path

from wrkviz.server import make_server
from wrkviz.standalone_server import (
    accepts_gzip,
    cache_control_for_path,
    parse_byte_range,
)
from wrkviz.static_assets import (
    deterministic_gzip,
    gzip_sidecar_path,
    sync_gzip_sidecar,
    write_text_with_gzip_invalidation,
)


def test_deterministic_gzip_has_fixed_header_and_round_trips() -> None:
    payload = (b"timeline payload\n" * 1_000) + bytes(range(256))

    first = deterministic_gzip(payload)
    second = deterministic_gzip(payload)

    assert first == second
    assert first[4:8] == b"\x00\x00\x00\x00"
    assert first[9] == 255
    assert gzip.decompress(first) == payload


def test_gzip_sidecar_sync_is_idempotent_and_removes_tiny_stale_file(
    tmp_path: Path,
) -> None:
    source = tmp_path / "timeline.json"
    source.write_bytes(b"x" * 4_096)

    assert sync_gzip_sidecar(source) is True
    sidecar = gzip_sidecar_path(source)
    first = sidecar.read_bytes()
    assert gzip.decompress(first) == source.read_bytes()
    assert sync_gzip_sidecar(source) is False
    assert sidecar.read_bytes() == first

    source.write_bytes(b"{}\n")
    assert sync_gzip_sidecar(source) is True
    assert not sidecar.exists()


def test_content_change_invalidates_sidecar_before_identity_replacement(
    tmp_path: Path,
) -> None:
    source = tmp_path / "timeline.json"
    source.write_text("old generation\n" * 1_000, encoding="utf-8")
    assert sync_gzip_sidecar(source) is True
    sidecar = gzip_sidecar_path(source)
    assert sidecar.is_file()

    changed = write_text_with_gzip_invalidation(
        source, "new generation\n" * 1_000
    )

    assert changed == 2
    assert not sidecar.exists()
    assert source.read_text(encoding="utf-8").startswith("new generation")
    assert sync_gzip_sidecar(source) is True
    refreshed = sidecar.read_bytes()
    assert gzip.decompress(refreshed) == source.read_bytes()
    assert write_text_with_gzip_invalidation(
        source, "new generation\n" * 1_000
    ) == 0
    assert sidecar.read_bytes() == refreshed


def test_accept_encoding_respects_explicit_gzip_exclusion() -> None:
    assert accepts_gzip("br, gzip") is True
    assert accepts_gzip("*") is True
    assert accepts_gzip("gzip;q=0, *;q=1") is False
    assert accepts_gzip("br") is False


def _request(
    port: int,
    method: str,
    path: str,
    headers: dict[str, str] | None = None,
) -> tuple[int, dict[str, str], bytes]:
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    try:
        connection.request(method, path, headers=headers or {})
        response = connection.getresponse()
        response_headers = {key.lower(): value for key, value in response.getheaders()}
        return response.status, response_headers, response.read()
    finally:
        connection.close()


def test_server_negotiates_sidecar_and_revalidates_with_strong_etag(
    tmp_path: Path,
) -> None:
    (tmp_path / "index.html").write_text("ok", encoding="utf-8")
    data_root = tmp_path / "data"
    data_root.mkdir()
    timeline = data_root / "timeline.json"
    identity = (b'{"event":"repeated timeline data"}\n' * 8_000)
    timeline.write_bytes(identity)
    assert sync_gzip_sidecar(timeline) is True

    content_hash_path = data_root / (("a" * 64) + ".json")
    content_hash_path.write_bytes(b"immutable\n" * 1_000)
    assert sync_gzip_sidecar(content_hash_path) is True

    server = make_server(tmp_path, port=0)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        port = int(server.server_address[1])
        status, headers, body = _request(
            port, "GET", "/data/timeline.json", {"Accept-Encoding": "br, gzip"}
        )
        assert status == 200
        assert headers["content-encoding"] == "gzip"
        assert headers["vary"] == "Accept-Encoding"
        assert headers["content-length"] == str(len(body))
        assert headers["cache-control"] == "public, no-cache"
        assert headers["etag"].startswith('"sha256-')
        assert not headers["etag"].startswith("W/")
        assert gzip.decompress(body) == identity

        status, validated_headers, validated_body = _request(
            port,
            "GET",
            "/data/timeline.json",
            {"Accept-Encoding": "gzip", "If-None-Match": headers["etag"]},
        )
        assert status == 304
        assert validated_body == b""
        assert validated_headers["etag"] == headers["etag"]
        assert validated_headers["vary"] == "Accept-Encoding"
        assert validated_headers["content-encoding"] == "gzip"

        status, identity_headers, identity_body = _request(
            port, "GET", "/data/timeline.json", {"Accept-Encoding": "gzip;q=0"}
        )
        assert status == 200
        assert "content-encoding" not in identity_headers
        assert identity_headers["vary"] == "Accept-Encoding"
        assert identity_headers["etag"] != headers["etag"]
        assert identity_body == identity

        status, head_headers, head_body = _request(
            port, "HEAD", "/data/timeline.json", {"Accept-Encoding": "gzip"}
        )
        assert status == 200
        assert head_body == b""
        assert head_headers["content-encoding"] == "gzip"
        assert head_headers["content-length"] == str(
            gzip_sidecar_path(timeline).stat().st_size
        )

        status, immutable_headers, _ = _request(
            port,
            "GET",
            "/data/" + content_hash_path.name,
            {"Accept-Encoding": "gzip"},
        )
        assert status == 200
        assert immutable_headers["cache-control"] == (
            "public, max-age=31536000, immutable"
        )

        status, _, listing_body = _request(port, "GET", "/data/")
        assert status == 404
        assert b"timeline.json" not in listing_body
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def test_only_complete_content_digest_filenames_are_immutable(tmp_path: Path) -> None:
    assert cache_control_for_path(tmp_path / (("f" * 64) + ".json")) == (
        "public, max-age=31536000, immutable"
    )
    assert cache_control_for_path(tmp_path / "manifest.json") == "public, no-cache"
    assert cache_control_for_path(tmp_path / "timeline.json") == "public, no-cache"
    assert cache_control_for_path(tmp_path / "phase-0123456789abcdef.json") == (
        "public, no-cache"
    )


def test_parse_byte_range_covers_the_forms_a_shard_reader_uses() -> None:
    """Only what a sidecar-driven reader asks for, and nothing that needs a MIME body."""

    assert parse_byte_range("bytes=0-99", 1_000) == (0, 99)
    assert parse_byte_range("bytes=100-", 1_000) == (100, 999)
    # A tail range is how a reader finds the last member with no sidecar to consult.
    assert parse_byte_range("bytes=-500", 1_000) == (500, 999)
    assert parse_byte_range("bytes=-5000", 1_000) == (0, 999)
    # Clamped rather than refused: a client may legitimately not know the length.
    assert parse_byte_range("bytes=900-99999", 1_000) == (900, 999)
    # `first >= size` is unsatisfiable and comes back inverted, which is how the caller tells it
    # apart from "no range here" and answers 416 instead of 200.
    unsatisfiable = parse_byte_range("bytes=1000-1099", 1_000)
    assert unsatisfiable is not None and unsatisfiable[1] < unsatisfiable[0]
    # Everything this server declines to interpret returns None, and a None means 200.
    for header in ("bytes=0-99,200-299", "items=0-99", "bytes=-", "bytes=abc", "bytes=-0", ""):
        assert parse_byte_range(header, 1_000) is None


def test_server_answers_a_byte_range_without_sending_the_whole_object(
    tmp_path: Path,
) -> None:
    """A schema-3 reader must be able to fetch one member by the offset its sidecar published.

    The archive shipped its own `serve.py` with no range support at all, so the layout designed
    around byte offsets could not be read by the server published beside it. The assertions are
    on the status, the `Content-Range` and the body length, because those are what a reader acts
    on -- and on `Accept-Ranges`, because a capability nothing advertises is a capability nothing
    uses.
    """

    payload = bytes(index % 251 for index in range(20_000))
    (tmp_path / "shard.bin").write_bytes(payload)
    (tmp_path / "index.html").write_text("ok", encoding="utf-8")
    server = make_server(tmp_path, port=0)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        port = int(server.server_address[1])
        status, headers, body = _request(port, "GET", "/shard.bin")
        assert status == 200
        assert headers["accept-ranges"] == "bytes"
        etag = headers["etag"]

        status, headers, body = _request(
            port, "GET", "/shard.bin", {"Range": "bytes=1000-1099"}
        )
        assert status == 206
        assert headers["content-range"] == "bytes 1000-1099/20000"
        assert headers["content-length"] == "100"
        assert body == payload[1000:1100]

        status, headers, body = _request(port, "GET", "/shard.bin", {"Range": "bytes=-50"})
        assert status == 206
        assert headers["content-range"] == "bytes 19950-19999/20000"
        assert body == payload[-50:]

        # Unsatisfiable: the client's idea of the file is stale and should be told so.
        status, headers, body = _request(
            port, "GET", "/shard.bin", {"Range": "bytes=20000-20099"}
        )
        assert status == 416
        assert headers["content-range"] == "bytes */20000"
        assert body == b""

        # `If-Range` against the current validator is honoured; against a stale one it is not,
        # and the whole representation is the only safe answer.
        status, headers, body = _request(
            port, "GET", "/shard.bin", {"Range": "bytes=0-9", "If-Range": etag}
        )
        assert status == 206
        assert body == payload[:10]
        status, headers, body = _request(
            port,
            "GET",
            "/shard.bin",
            {"Range": "bytes=0-9", "If-Range": '"sha256-' + ("0" * 64) + '"'},
        )
        assert status == 200
        assert body == payload

        # A multi-range request is answered whole rather than with a multipart body.
        status, headers, body = _request(
            port, "GET", "/shard.bin", {"Range": "bytes=0-9,20-29"}
        )
        assert status == 200
        assert body == payload
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def test_a_range_request_is_never_answered_with_a_content_coding(tmp_path: Path) -> None:
    """Byte offsets address the named file, so negotiation is off when a range is asked for.

    Half a gzip stream is not half a document. Returning a slice of the `.gz` companion under
    `Content-Encoding: gzip` would hand a client bytes it cannot decode and cannot tell apart
    from the ones it asked for.
    """

    identity = b'{"event":"repeated timeline data"}\n' * 2_000
    target = tmp_path / "data.json"
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_bytes(identity)
    (tmp_path / "index.html").write_text("ok", encoding="utf-8")
    assert sync_gzip_sidecar(target) is True
    server = make_server(tmp_path, port=0)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        port = int(server.server_address[1])
        status, headers, body = _request(
            port,
            "GET",
            "/data.json",
            {"Accept-Encoding": "gzip", "Range": "bytes=0-33"},
        )
        assert status == 206
        assert "content-encoding" not in headers
        assert body == identity[:34]
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)
