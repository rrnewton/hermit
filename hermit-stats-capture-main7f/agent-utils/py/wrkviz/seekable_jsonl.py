"""Chunked, seekable JSONL: a multi-member gzip file plus a codec-agnostic sidecar index.

The archive's problem, stated plainly: nothing in it is seekable. Reading three records out of
`extracted/transcripts/messages.jsonl` costs 328 MB of I/O and 950 MB of resident memory, because
filtering happens after full materialization. Everything below exists to make "the last twenty
records" and "everything between two timestamps" cost the size of the answer instead of the size
of the archive.

Container
---------
A **multi-member gzip file**. gzip's format is the concatenation of independently decodable
members, so a file that holds N members is still, to `gunzip`, `zcat`, `gzip -t`, `file(1)` and
`gzip.open`, an entirely ordinary `.gz`. Members are cut at the first LINE boundary at or after a
target of :data:`DEFAULT_TARGET_CHUNK_BYTES`, so a member always holds whole lines and no reader
ever has to reassemble a record that straddles a seek point.

The codec choice was measured on the real 101,584,237-byte `messages.jsonl` (63,844 records) and
is not a knob:

    codec     chunk   members        total   vs whole-file   index   1-chunk decompress
    gzip-6      64K     1,529   23,433,193          +8.39%   86,207B      0.1ms
    gzip-6     256K       387   22,094,090          +2.20%   22,583B      0.6ms
    gzip-6    1024K        97   21,740,324          +0.56%    5,845B      2.2ms
    gzip-6    4096K        25   21,650,576          +0.15%    1,523B      8.4ms
    zstd-19   1024K        97   17,465,144         +10.51%    5,832B      2.6ms
    xz-6      1024K        97   17,279,536          +8.18%   10,300B     10.3ms

gzip at ~1 MiB wins the argument that matters. Chunking costs it +0.56% -- effectively nothing --
because its 32 KiB window already makes its compression local, so cutting the stream throws away
almost no context it was using. The same cut costs zstd-19 10.5% and xz-6 8.2%, since those codecs
earn their smaller absolute size precisely by remembering megabytes back. Paying 24% more bytes to
keep a format every tool on every host already reads, and to keep the random-access tax near zero,
is the trade this archive wants: it is a durable record first and a transfer artifact second.

Re-measured with this implementation, on the same file as it stands today (103,895,486 bytes,
65,123 records): 99 members, 22,274,727 bytes, against 22,151,001 for the same input through
whole-file gzip-6 -- **+0.559%**, reproducing the audit's +0.56% to three digits. `gzip -t`
passes, `file(1)` reports plain "gzip compressed data", `gunzip -c` reproduces
83fae78b00f966441df58bd1d5a5b3fd82b58b19707c306b68a1a91b6403f779, which is byte for byte the
sha256 already recorded for `messages.jsonl` in `extracted/transcripts/manifest.json`. The
existing integrity contract survives chunking untouched, which is the whole reason the writer
has a lines-in/lines-out door as well as a records door.

Why a sidecar index and not something cleverer
----------------------------------------------
Three shapes were considered.

*Per-member gzip FEXTRA fields*, the BGZF trick. Rejected: Python's `gzip` module cannot write
FEXTRA, so we would be hand-rolling headers, which forfeits the one property this whole design is
built on -- that the container is produced by a boring writer and consumed by every existing tool.
And it does not even answer the question we need answered, because "which member holds t=X" would
still require walking every member header.

*An index appended after the final member.* Rejected: `gunzip` treats bytes after the last member
as trailing garbage and says so, which loses `gzip -t` cleanliness -- the exact property being
protected.

*A sidecar file.* Accepted. Its one real cost is that the pair can be separated, which is why
:class:`SeekableJsonlReader` degrades to a correct full scan when the index is absent, and why
:func:`rebuild_index` can reconstruct the index from the `.gz` alone. Losing the index costs
speed and nothing else, and that is the requirement, not an accident.

Index shape
-----------
The sidecar is itself JSONL: line 0 is a header object, lines 1..N are one object per member, in
file order. Pretty-printed whole-file JSON was rejected for the reason this module exists -- an
index you must materialize entirely before you can read a field is a small copy of the problem.
Line-per-member also keeps diffs local when an archive is rebuilt and checked in, and leaves the
door open to bisecting the index file itself the day an index stops fitting comfortably in memory.
(Today it fits easily. Run against the real `messages.jsonl`, this writer produces 99 members and
a 12,633-byte index -- 0.06% of the 22.3 MB it describes, or about 128 bytes a member, which is
what named keys and 13-digit epoch milliseconds cost. Extrapolated at 1 MiB chunks, an index over
the whole 8.79 GB archive lands near a megabyte.)

Header keys::

    format              "wrkviz/seekable-jsonl-index"
    version             1
    codec               "gzip"      -- descriptive; the member entries are codec-agnostic
    codec_level         6
    target_chunk_bytes  the writer's cut threshold
    timestamp_key       record field the t0/t1 bounds were taken from
    timestamps_sorted   whether t0/t1 are non-decreasing across members (enables bisect)
    member_count, record_count
    c_size, u_size      total compressed and uncompressed bytes
    c_sha256, u_sha256  digests of the compressed file and of the uncompressed stream
    data_file           basename of the `.gz` this index describes

Member keys, one line each::

    c_off, c_len        byte range of the member within the compressed file
    u_off, u_len        byte range of the member within the uncompressed stream
    l0, n               first line number (0-based) and line count
    t0, t1              minimum and maximum timestamp in the member, or null

Nothing in a member entry mentions gzip. Swapping the codec later -- zstd with per-frame
independence, say -- changes the header's `codec`/`codec_level` and not one member field, which is
the point: the index describes *where things are*, and the header describes *how to inflate them*.
Only `codec`, `codec_level`, `target_chunk_bytes` and `timestamp_key` are unrecoverable from the
data; every other field :func:`rebuild_index` derives by walking the file.

That door is only real if the reader walks through it deliberately, so :class:`SeekableJsonlReader`
refuses a `codec` it does not implement with :class:`UnsupportedCodecError` instead of inflating
the members as gzip and hoping. A reader that ignored the field would accept a zstd-labelled index
today -- and on the day a zstd file appeared would report "member did not inflate", whose remedy is
to replace the data. Software older than its data must say so.

Per-member digests were considered and rejected. gzip already carries a CRC-32 per member, so a
corrupt member fails to inflate rather than returning wrong bytes, and the archive's existing
integrity contract is a single sha256 over the uncompressed stream, recorded in
`extracted/transcripts/manifest.json` -- which, as recorded above, a round trip through 99 members
reproduces byte for byte. Adding per-member digests would double the index and give a third,
differently-scoped answer to a question already answered twice.

Determinism
-----------
Two writes over the same input produce byte-identical output: members carry mtime=0 and no
filename, records are encoded with sorted keys and no spaces, and cuts depend only on byte counts.
The archive's whole idempotence story -- `write_text_if_changed`, and a checkout that does not
churn on a rebuild -- depends on that. Members are compressed through
:func:`wrkviz.static_assets.deterministic_gzip` rather than
`zlib.compressobj(wbits=31)` because zlib stamps a platform-dependent OS byte into the header
(0x03 on Linux), which would make the same input produce different files on a build host and on a
laptop; Python's `gzip` module hardcodes 0xff, "unknown". Byte-identity is guaranteed for a fixed
zlib, which is what idempotence needs; a zlib upgrade may legitimately re-emit the file.

Known wart, worth knowing before someone files a bug: ``gzip -l`` reports only the LAST member's
ISIZE, so on a multi-member file it under-reports the uncompressed size. `gunzip`, `zcat`,
`gzip.open` and this module are all correct; `gzip -l` is the outlier, and always has been.
"""

from __future__ import annotations

import filecmp
import gzip
import hashlib
import json
import os
import tempfile
import zlib
from bisect import bisect_left, bisect_right
from collections import deque
from collections.abc import Callable, Iterable, Iterator, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from types import TracebackType
from typing import BinaryIO

from wrkviz.archive import (
    JsonValue,
    as_int,
    as_object,
    as_string,
    narrow_json,
    write_text_if_changed,
)
from wrkviz.static_assets import GZIP_COMPRESSION_LEVEL, deterministic_gzip


INDEX_FORMAT = "wrkviz/seekable-jsonl-index"
INDEX_VERSION = 1
INDEX_CODEC = "gzip"
INDEX_SUFFIX = ".index.jsonl"

# 1 MiB, the measured knee in the table above. Below it the index and the per-member header
# overhead start to matter (+8.4% at 64 KiB); above it a point read decompresses more than it
# needs (8.4ms at 4 MiB, against 2.2ms here) for a further saving of 0.4%.
DEFAULT_TARGET_CHUNK_BYTES = 1 << 20

# The archive's universal instant field. Every JSONL projection under `extracted/` carries it as
# integer milliseconds since the epoch, and `query.py` already reads it under this exact name.
DEFAULT_TIMESTAMP_KEY = "timestamp_ms"

# gzip framing for `zlib.decompressobj`. Only the scan path uses raw zlib, because it needs to be
# told where one member ends -- which is exactly what `unused_data` reports and what the friendly
# `gzip` module hides.
_GZIP_WBITS = 16 + zlib.MAX_WBITS

# Block size for the unindexed scan. Large enough that syscall overhead disappears against the
# inflate cost, small enough that the fallback path's peak memory stays a rounding error.
_SCAN_BLOCK_BYTES = 1 << 18

_NEWLINE = b"\n"


class SeekableJsonlError(Exception):
    """Base class for every way a chunked JSONL pair can be wrong.

    It is a promise to callers, not a taxonomy for its own sake: a reader that wants the slow
    correct answer when the sidecar is unusable writes ``except SeekableJsonlError: ...`` and
    falls back to ``use_index=False``. That only works if *nothing* here lets a raw
    ``JSONDecodeError`` or ``UnicodeDecodeError`` out of a damaged sidecar, which is why the
    parse below converts them rather than letting them propagate as the stdlib raised them.
    """


class UnsupportedCodecError(SeekableJsonlError):
    """The sidecar describes a compression codec this reader does not implement.

    Separate from :class:`TruncatedArchiveError` because it is the opposite diagnosis. The index
    schema is deliberately codec-agnostic in its member table -- ``c_off``/``c_len`` mean the same
    thing whatever produced the bytes -- so a future zstd writer is a legal producer of this
    format, and a gzip-only reader meeting its output has found software older than its data, not
    damaged data. Without this class that case surfaces as "member at 0 did not inflate", whose
    remedy ("replace or re-derive the file") would destroy a perfectly good archive.
    """


class TruncatedArchiveError(SeekableJsonlError):
    """The compressed bytes are not a member this reader can inflate.

    The named case is the one that actually happens -- a write interrupted partway leaves a final
    member with no trailer -- but the class also carries a member that fails its CRC and bytes
    after the last member that are not a member at all. They are one error because they have one
    remedy: the data file is damaged and must be replaced or re-derived, and no amount of index
    work will help. Contrast :class:`IndexMismatchError`, where the data is fine and the sidecar
    is what needs rebuilding.
    """


class IndexMismatchError(SeekableJsonlError):
    """The sidecar index describes a file other than the one on disk.

    Raised rather than silently ignored. A stale index does not produce a slow answer, it produces
    a confidently wrong one -- records attributed to the wrong timestamps, a `tail` that returns
    the middle of the file -- and there is no way for a caller to notice. Callers who would rather
    have the slow correct answer pass ``use_index=False``, or delete the sidecar and let the
    documented fallback take over.
    """


@dataclass(frozen=True)
class ChunkIndexEntry:
    """One member's location, in both coordinate systems, plus what is inside it."""

    c_off: int
    c_len: int
    u_off: int
    u_len: int
    l0: int
    n: int
    t0: int | None
    t1: int | None

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Render this entry as one index line, in the schema's declared key order."""

        return {
            "c_off": self.c_off,
            "c_len": self.c_len,
            "u_off": self.u_off,
            "u_len": self.u_len,
            "l0": self.l0,
            "n": self.n,
            "t0": self.t0,
            "t1": self.t1,
        }

    def overlaps(self, start_ms: int | None, end_ms: int | None) -> bool:
        """Whether this member can hold a record in the half-open range ``[start, end)``."""

        if self.t0 is None or self.t1 is None:
            # No timestamped record in the member, so no time range can select from it.
            return False
        if start_ms is not None and self.t1 < start_ms:
            return False
        if end_ms is not None and self.t0 >= end_ms:
            return False
        return True


@dataclass(frozen=True)
class ChunkIndex:
    """A parsed sidecar: the header fields plus the member table, in file order."""

    version: int
    codec: str
    codec_level: int
    target_chunk_bytes: int
    timestamp_key: str
    timestamps_sorted: bool
    record_count: int
    c_size: int
    u_size: int
    c_sha256: str
    u_sha256: str
    data_file: str
    members: tuple[ChunkIndexEntry, ...]

    def to_text(self) -> str:
        """Render the sidecar: header line, then one line per member."""

        header: dict[str, JsonValue] = {
            "format": INDEX_FORMAT,
            "version": self.version,
            "codec": self.codec,
            "codec_level": self.codec_level,
            "target_chunk_bytes": self.target_chunk_bytes,
            "timestamp_key": self.timestamp_key,
            "timestamps_sorted": self.timestamps_sorted,
            "member_count": len(self.members),
            "record_count": self.record_count,
            "c_size": self.c_size,
            "u_size": self.u_size,
            "c_sha256": self.c_sha256,
            "u_sha256": self.u_sha256,
            "data_file": self.data_file,
        }
        lines = [_json_line(header)]
        lines.extend(_json_line(member.to_json_obj()) for member in self.members)
        return "".join(line + "\n" for line in lines)

    @classmethod
    def from_text(cls, text: str, where: str) -> ChunkIndex:
        """Parse a sidecar, reporting every way it can be malformed as one error class.

        The narrowing helpers this borrows from :mod:`wrkviz.archive` raise
        ``ValueError`` for a field of the wrong type, which is right for their own callers and
        wrong here: a caller of this module degrades to a full scan on
        :class:`SeekableJsonlError` and would otherwise crash on a sidecar with a string where an
        integer belongs. The translation is one place rather than thirty call sites.
        """

        try:
            return cls._parse(text, where)
        except ValueError as exc:
            raise IndexMismatchError(f"{where}: malformed index: {exc}") from exc

    @classmethod
    def _parse(cls, text: str, where: str) -> ChunkIndex:
        """Parse a sidecar and check that it is internally consistent.

        Internal consistency is checked here, at parse time, and not at the point of use: the
        member table's contiguity is the invariant every read below assumes, and an invariant
        checked once against an in-memory structure costs nothing while an invariant checked
        per-read is both slower and easier to forget in one branch.

        Split on ``"\\n"`` and not with ``str.splitlines()``. The index is written with
        ``ensure_ascii=False``, so a ``data_file`` or ``timestamp_key`` carrying U+2028, U+2029 or
        U+0085 -- none of which JSON escapes and all of which ``splitlines`` treats as line
        terminators -- would be written as one line and read back as two. Blank lines are dropped
        rather than refused, because a trailing newline appended by an editor or a ``>>`` is not a
        record and turning it into a parse failure would make the sidecar unreadable for a reason
        that carries no information.
        """

        raw_lines = [line for line in text.split("\n") if line]
        if not raw_lines:
            raise IndexMismatchError(f"{where}: index is empty")
        header = _json_object(raw_lines[0], f"{where} header")
        fmt = as_string(header.get("format"), f"{where} header.format")
        if fmt != INDEX_FORMAT:
            raise IndexMismatchError(f"{where}: unknown index format {fmt!r}")
        version = as_int(header.get("version"), f"{where} header.version")
        if version != INDEX_VERSION:
            raise IndexMismatchError(f"{where}: unsupported index version {version}")
        members: list[ChunkIndexEntry] = []
        c_off = 0
        u_off = 0
        line_no = 0
        for offset, raw in enumerate(raw_lines[1:], start=1):
            spot = f"{where} member {offset - 1}"
            obj = _json_object(raw, spot)
            entry = ChunkIndexEntry(
                c_off=as_int(obj.get("c_off"), f"{spot}.c_off"),
                c_len=as_int(obj.get("c_len"), f"{spot}.c_len"),
                u_off=as_int(obj.get("u_off"), f"{spot}.u_off"),
                u_len=as_int(obj.get("u_len"), f"{spot}.u_len"),
                l0=as_int(obj.get("l0"), f"{spot}.l0"),
                n=as_int(obj.get("n"), f"{spot}.n"),
                t0=_optional_int(obj.get("t0"), f"{spot}.t0"),
                t1=_optional_int(obj.get("t1"), f"{spot}.t1"),
            )
            if entry.c_off != c_off or entry.u_off != u_off or entry.l0 != line_no:
                raise IndexMismatchError(
                    f"{spot}: not contiguous with the preceding members "
                    f"(expected c_off={c_off} u_off={u_off} l0={line_no}, "
                    f"found c_off={entry.c_off} u_off={entry.u_off} l0={entry.l0})"
                )
            if entry.c_len <= 0 or entry.u_len < 0 or entry.n < 0:
                raise IndexMismatchError(f"{spot}: non-positive extent")
            members.append(entry)
            c_off += entry.c_len
            u_off += entry.u_len
            line_no += entry.n
        if not members:
            raise IndexMismatchError(f"{where}: index describes no members")
        index = cls(
            version=version,
            codec=as_string(header.get("codec"), f"{where} header.codec"),
            codec_level=as_int(header.get("codec_level"), f"{where} header.codec_level"),
            target_chunk_bytes=as_int(
                header.get("target_chunk_bytes"), f"{where} header.target_chunk_bytes"
            ),
            timestamp_key=as_string(
                header.get("timestamp_key"), f"{where} header.timestamp_key"
            ),
            # The header's claim is *confirmed*, never taken on trust, and a claim the table does
            # not support is quietly downgraded rather than refused. Downgrading is right here and
            # nowhere else in this parse: every other disagreement makes reads wrong, while this
            # one only makes them linear over an in-memory table -- which is the documented
            # fallback posture, not a degraded one. And it must be checked, because the bisect key
            # is a plain tuple handed to `bisect_left`: a single member with no timestamps puts a
            # -1 in the middle of it, and a binary search over a non-monotonic sequence does not
            # fail, it returns a cursor past real data and silently drops every record before it.
            # A writer cannot produce that, but two writer outputs concatenated and re-indexed can
            # -- which is exactly the workflow the module docstring advertises.
            timestamps_sorted=(
                _boolean(header.get("timestamps_sorted"), f"{where} header.timestamps_sorted")
                and _bisectable(members)
            ),
            record_count=as_int(header.get("record_count"), f"{where} header.record_count"),
            c_size=as_int(header.get("c_size"), f"{where} header.c_size"),
            u_size=as_int(header.get("u_size"), f"{where} header.u_size"),
            c_sha256=as_string(header.get("c_sha256"), f"{where} header.c_sha256"),
            u_sha256=as_string(header.get("u_sha256"), f"{where} header.u_sha256"),
            data_file=as_string(header.get("data_file"), f"{where} header.data_file"),
            members=tuple(members),
        )
        member_count = as_int(header.get("member_count"), f"{where} header.member_count")
        if member_count != len(members):
            raise IndexMismatchError(
                f"{where}: header claims {member_count} members, index lists {len(members)}"
            )
        if index.c_size != c_off or index.u_size != u_off or index.record_count != line_no:
            raise IndexMismatchError(
                f"{where}: header totals disagree with the member table "
                f"(header c_size={index.c_size} u_size={index.u_size} "
                f"record_count={index.record_count}; members sum to "
                f"c_size={c_off} u_size={u_off} record_count={line_no})"
            )
        return index


def _bisectable(members: Sequence[ChunkIndexEntry]) -> bool:
    """Whether :meth:`SeekableJsonlReader._candidates` may binary-search this member table.

    The precondition the search actually needs, stated once so that the writer, the rebuilder and
    the parser all answer the question the same way. It is *member*-level monotonicity -- each
    member's earliest instant at or after the previous member's latest -- and not the
    record-level "the stream never went backwards" the first draft of this module tested. The two
    differ in both directions and the difference matters:

    * A member with no timestamps at all has no position on the axis. Its ``t1`` becomes a -1
      sentinel in the bisect key, and one of those anywhere but the tail makes the key
      non-monotonic. The writer only ever emits such a member for empty input, but
      :func:`rebuild_index` over a concatenation of two archives can meet one in the middle.
    * Records out of order *within* one member are harmless, because the member is scanned whole
      once the search has selected it. Declaring the file unsorted for that would give up the
      bisect for nothing.
    """

    previous: int | None = None
    for entry in members:
        if entry.t0 is None or entry.t1 is None:
            return False
        if previous is not None and entry.t0 < previous:
            return False
        previous = entry.t1
    return True


@dataclass(frozen=True)
class WriteReport:
    """What :func:`write_seekable_jsonl` did, in enough detail to log or assert on."""

    path: Path
    index_path: Path
    index: ChunkIndex
    data_changed: bool
    index_changed: bool

    @property
    def member_count(self) -> int:
        """How many members the file was cut into."""

        return len(self.index.members)

    @property
    def record_count(self) -> int:
        """How many records were written, across all members."""

        return self.index.record_count


def index_path_for(path: Path) -> Path:
    """Return the conventional sidecar path for a chunked JSONL file.

    The suffix is appended rather than substituted -- `messages.jsonl.gz` gains
    `messages.jsonl.gz.index.jsonl` -- so the pair sorts adjacent in a directory listing and so
    the index cannot collide with the data file of a differently-compressed sibling.
    """

    return path.with_name(path.name + INDEX_SUFFIX)


def write_seekable_jsonl(
    path: Path,
    records: Iterable[Mapping[str, JsonValue]],
    *,
    target_chunk_bytes: int = DEFAULT_TARGET_CHUNK_BYTES,
    timestamp_key: str = DEFAULT_TIMESTAMP_KEY,
    index_path: Path | None = None,
) -> WriteReport:
    """Write records as chunked JSONL, encoding each one canonically."""

    return _write(
        path,
        (_encode_record(record, timestamp_key, index) for index, record in enumerate(records)),
        target_chunk_bytes=target_chunk_bytes,
        timestamp_key=timestamp_key,
        index_path=index_path,
    )


def write_seekable_jsonl_lines(
    path: Path,
    lines: Iterable[bytes],
    *,
    target_chunk_bytes: int = DEFAULT_TARGET_CHUNK_BYTES,
    timestamp_key: str = DEFAULT_TIMESTAMP_KEY,
    index_path: Path | None = None,
) -> WriteReport:
    """Write already-encoded JSON lines verbatim, without a newline on any of them.

    This door exists for the one job :func:`write_seekable_jsonl` cannot do: re-chunking an
    existing `.jsonl` without changing a byte of it. The archive's integrity contract is a sha256
    over the uncompressed stream, so a migration that re-encoded records -- reordering keys,
    renormalizing floats, changing `ensure_ascii` -- would break the digest recorded in
    `manifest.json` even though every record survived. Here the caller's bytes go through
    untouched and the digest still matches; the price is that each line is parsed once purely to
    read its timestamp, which is a one-time migration cost paid for provable losslessness.
    """

    return _write(
        path,
        (_encode_line(line, timestamp_key, index) for index, line in enumerate(lines)),
        target_chunk_bytes=target_chunk_bytes,
        timestamp_key=timestamp_key,
        index_path=index_path,
    )


def _write(
    path: Path,
    encoded: Iterable[tuple[bytes, int | None]],
    *,
    target_chunk_bytes: int,
    timestamp_key: str,
    index_path: Path | None,
) -> WriteReport:
    if target_chunk_bytes <= 0:
        raise ValueError("target_chunk_bytes must be positive")
    sidecar = index_path if index_path is not None else index_path_for(path)
    path.parent.mkdir(parents=True, exist_ok=True)

    members: list[ChunkIndexEntry] = []
    c_hash = hashlib.sha256()
    u_hash = hashlib.sha256()
    c_off = 0
    u_off = 0
    line_no = 0
    # Members are staged in memory one at a time. Peak memory is therefore one target chunk plus
    # one oversize record, not one file -- the property that makes this usable against the 5.83 GB
    # of source snapshots without the 950 MB RSS that motivated the whole exercise.
    pending: list[bytes] = []
    pending_bytes = 0
    pending_lines = 0
    pending_t0: int | None = None
    pending_t1: int | None = None

    fd, raw_tmp = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    tmp = Path(raw_tmp)
    try:
        with os.fdopen(fd, "wb") as handle:

            def flush() -> None:
                nonlocal c_off, u_off, line_no, pending, pending_bytes, pending_lines
                nonlocal pending_t0, pending_t1
                blob = b"".join(pending)
                member = deterministic_gzip(blob)
                handle.write(member)
                c_hash.update(member)
                u_hash.update(blob)
                members.append(
                    ChunkIndexEntry(
                        c_off=c_off,
                        c_len=len(member),
                        u_off=u_off,
                        u_len=len(blob),
                        l0=line_no,
                        n=pending_lines,
                        t0=pending_t0,
                        t1=pending_t1,
                    )
                )
                c_off += len(member)
                u_off += len(blob)
                line_no += pending_lines
                pending = []
                pending_bytes = 0
                pending_lines = 0
                pending_t0 = None
                pending_t1 = None

            for line, timestamp in encoded:
                pending.append(line)
                pending.append(_NEWLINE)
                pending_bytes += len(line) + 1
                pending_lines += 1
                if timestamp is not None:
                    pending_t0 = timestamp if pending_t0 is None else min(pending_t0, timestamp)
                    pending_t1 = timestamp if pending_t1 is None else max(pending_t1, timestamp)
                # Cut AFTER appending: the member closes at the first line boundary at or AFTER
                # the target, so it overshoots rather than stopping short, and a record larger
                # than the whole target simply closes the member it landed in instead of being
                # split. The other arrangement -- flush first when this record would overflow --
                # undershoots instead, and its member count is unbounded from above: a stream of
                # records just under the target would produce one small member each. Overshooting
                # bounds it at u_size/target + 1 members, which is what keeps the index at the
                # measured 0.06% of the data instead of at whatever the record sizes decide.
                if pending_bytes >= target_chunk_bytes:
                    flush()
            # An empty input still gets one empty member. A zero-byte file is not a gzip file,
            # and "the .gz alone is always valid" is the invariant everything else here rests on;
            # an empty member costs 20 bytes and keeps the writer's output uniformly openable.
            if pending_lines > 0 or not members:
                flush()
            handle.flush()
            os.fsync(handle.fileno())

        index = ChunkIndex(
            version=INDEX_VERSION,
            codec=INDEX_CODEC,
            codec_level=GZIP_COMPRESSION_LEVEL,
            target_chunk_bytes=target_chunk_bytes,
            timestamp_key=timestamp_key,
            # Declared by the one predicate the reader's search is defined against, rather than by
            # a second, independently-maintained rule about the record stream. See `_bisectable`:
            # when it is false the reader filters the (tiny, in-memory) member table linearly and
            # reads exactly the same bytes from the `.gz`. Correct beats clever on a durable
            # format, and one definition beats two that are meant to agree.
            timestamps_sorted=_bisectable(members),
            record_count=line_no,
            c_size=c_off,
            u_size=u_off,
            c_sha256=c_hash.hexdigest(),
            u_sha256=u_hash.hexdigest(),
            data_file=path.name,
            members=tuple(members),
        )
        data_changed = _replace_if_changed(tmp, path, sidecar)
    finally:
        if tmp.exists():
            tmp.unlink()
    index_changed = write_text_if_changed(sidecar, index.to_text())
    return WriteReport(
        path=path,
        index_path=sidecar,
        index=index,
        data_changed=data_changed,
        index_changed=index_changed,
    )


def _replace_if_changed(tmp: Path, path: Path, sidecar: Path) -> bool:
    """Publish *tmp* as *path*, unless the bytes already there are identical.

    `archive.write_text_if_changed` cannot be reused here: it reads and compares as UTF-8 text,
    and these are compressed bytes. The rule it enforces is the same one, for the same reason --
    an identical rebuild must not churn the mtime of a file that is checked in, or every rebuild
    looks like a change to everything downstream of it.

    The cost is one extra sequential read of the file already on disk, on top of the pass that
    produced the replacement. That is the cheap side of the trade: the comparison is streamed by
    `filecmp` and short-circuits on a size mismatch, whereas an unnecessary `os.replace` of a
    21 MB blob is a new inode, a new mtime, and a diff for whoever is watching the archive.

    **The sidecar is removed first, and only when the data is really being replaced.** Data and
    index are two files and cannot be published in one atomic step, so a crash, an ENOSPC or a
    kill between them leaves *some* pair on disk and the only choice is which pair. Unlinking
    first leaves new data with no index, which is the documented fallback: correct answers,
    linear reads, and `rebuild_index` restores the speed. The other order leaves new data beside
    an index describing the old, and the open-time check is a single comparison of total
    compressed size, so a change that does not move the size -- shifting every timestamp by one
    millisecond does not -- opens clean and then fails inside a member as a
    :class:`TruncatedArchiveError`, telling the operator to replace data that is perfectly good.
    `static_assets.write_text_with_gzip_invalidation` already reaches this conclusion for the
    site's assets, for the same reason and in the same direction.
    """

    if path.is_file() and filecmp.cmp(tmp, path, shallow=False):
        return False
    sidecar.unlink(missing_ok=True)
    os.replace(tmp, path)
    return True


@dataclass(frozen=True)
class _RawMember:
    """One member as found by a scan: where it starts, its bytes, and its contents."""

    c_off: int
    raw: bytes
    data: bytes


def _iter_members(path: Path, note: Callable[[int], None]) -> Iterator[_RawMember]:
    """Walk a multi-member gzip file from the start, yielding one member at a time.

    This is the fallback that makes the sidecar optional, and it is also what
    :func:`rebuild_index` is built on. `zlib.decompressobj` is used rather than `gzip.open` for
    one reason: when a member ends it reports the leftover bytes in `unused_data`, which is
    precisely the member boundary the friendly wrapper hides.

    **It opens its own handle rather than borrowing one, and that is a correctness property.**
    A generator that seeks and reads a handle it shares is safe only while it is the sole user of
    that handle's cursor -- and the obvious sharer is *itself*: two live walks over one reader
    interleave their `read`s, each resuming where the other left off, and the first one to notice
    raises "byte N is not part of a gzip member". A caller bug would be reported as a damaged
    archive, and it would only appear on files larger than one `_SCAN_BLOCK_BYTES`, so it would
    pass every small test and surface at archive scale. A file descriptor per walk is the cheapest
    possible price for that not being true. The compressed bytes are still handed back alongside
    the inflated ones, because the caller would otherwise have to re-read them at ``c_off``.
    """

    where = str(path)
    with path.open("rb") as handle:
        c_off = 0
        consumed = 0
        raw: list[bytes] = []
        out: list[bytes] = []
        engine = zlib.decompressobj(_GZIP_WBITS)
        pending = b""
        while True:
            if not pending:
                pending = handle.read(_SCAN_BLOCK_BYTES)
                if not pending:
                    break
                note(len(pending))
            try:
                out.append(engine.decompress(pending))
            except zlib.error as exc:
                raise TruncatedArchiveError(
                    f"{where}: byte {c_off + consumed} is not part of a gzip member: {exc}"
                ) from exc
            if engine.eof:
                unused = engine.unused_data
                used = len(pending) - len(unused)
                raw.append(pending[:used])
                consumed += used
                yield _RawMember(c_off=c_off, raw=b"".join(raw), data=b"".join(out))
                c_off += consumed
                consumed = 0
                raw = []
                out = []
                engine = zlib.decompressobj(_GZIP_WBITS)
                pending = unused
            else:
                raw.append(pending)
                consumed += len(pending)
                pending = b""
        if consumed:
            raise TruncatedArchiveError(
                f"{where}: file ends inside the member starting at {c_off} "
                f"({consumed} bytes read, no gzip trailer)"
            )


class SeekableJsonlReader:
    """Read a chunked JSONL file, touching only the members an answer needs.

    Every method is O(result) in bytes read from the data file when an index is present, and
    correct-but-O(file) when it is absent. :attr:`data_bytes_read` counts physical reads from the
    `.gz` so that claim can be asserted on rather than timed; :attr:`index_bytes_read` counts the
    one-time sidecar read separately, because the index is a roughly fixed 0.06% of the data and
    folding it into the same counter would obscure exactly the number under test.
    """

    def __init__(
        self,
        path: Path,
        *,
        index_path: Path | None = None,
        use_index: bool = True,
        timestamp_key: str | None = None,
        cache_members: bool = False,
    ) -> None:
        self._path = path
        self._index_path = index_path if index_path is not None else index_path_for(path)
        self._data_bytes_read = 0
        self._index_bytes_read = 0
        self._index: ChunkIndex | None = None
        self._member_ends: tuple[int, ...] | None = None
        self._member_starts: tuple[int, ...] | None = None
        # Which field holds the instant. `None` -- the normal case -- means "whatever the index
        # says, and `DEFAULT_TIMESTAMP_KEY` if there is no index". A caller that names a key is
        # making an assertion, not a suggestion, and it is checked below rather than ignored: the
        # index's t0/t1 bounds were computed from the header's field, so quietly honouring a
        # different one would filter records against one field while selecting members by another,
        # and the same query would return different answers depending only on whether the sidecar
        # happened to be on disk. That would falsify the whole "losing the index costs speed and
        # nothing else" contract in the one direction nobody would think to test.
        self._fallback_timestamp_key = (
            DEFAULT_TIMESTAMP_KEY if timestamp_key is None else timestamp_key
        )
        self._handle: BinaryIO | None = None
        # Off by default, because the general case is a reader used once over a file far larger
        # than memory, and a cache there would turn a streaming read into an out-of-memory error.
        # A caller opts in when it will ask several questions of one file whose answers live in
        # overlapping members -- reading three separate line ranges out of the same member, say.
        # Without it each question re-inflates the member and `data_bytes_read` honestly reports
        # the repetition; with it the counter reports what a reader that keeps what it has already
        # paid for actually costs. `query._ChunkedJsonlReader`, the standalone copy of this class,
        # carries the same option under the same name, and it is used there for exactly that case.
        self._members: dict[int, bytes] | None = {} if cache_members else None
        if use_index and self._index_path.is_file():
            raw_sidecar = self._index_path.read_bytes()
            self._index_bytes_read += len(raw_sidecar)
            try:
                sidecar = raw_sidecar.decode("utf-8")
            except UnicodeDecodeError as exc:
                raise IndexMismatchError(
                    f"{self._index_path}: the sidecar is not UTF-8 text: {exc}"
                ) from exc
            index = ChunkIndex.from_text(sidecar, str(self._index_path))
            # Three O(1) agreement checks, done once at open, before any read trusts the table.
            # The expensive digest comparison waits for an explicit `verify(deep=True)`; these are
            # the ones cheap enough to be unconditional.
            if index.codec != INDEX_CODEC:
                raise UnsupportedCodecError(
                    f"{self._index_path}: index describes codec {index.codec!r} at level "
                    f"{index.codec_level}; this reader implements {INDEX_CODEC!r} only"
                )
            # The header names the file it describes, and until this check that field was written,
            # parsed, type-checked and never once compared -- while `IndexMismatchError` advertised
            # exactly this condition. An explicit `index_path=` is a supported argument on both the
            # reader and the writer, so pairing an index with a foreign data file of the same size
            # is a thing a caller can do by accident, and its symptom is confidently wrong
            # timestamps rather than an error.
            if index.data_file != path.name:
                raise IndexMismatchError(
                    f"{self._index_path}: index describes {index.data_file!r}, not "
                    f"{path.name!r}; pass the matching sidecar or run `rebuild_index`"
                )
            size = path.stat().st_size
            if size != index.c_size:
                raise IndexMismatchError(
                    f"{self._index_path}: index describes {index.c_size} compressed bytes "
                    f"but {path} holds {size}"
                )
            if timestamp_key is not None and timestamp_key != index.timestamp_key:
                raise IndexMismatchError(
                    f"{self._index_path}: index was built over {index.timestamp_key!r} but this "
                    f"reader was asked for {timestamp_key!r}; rebuild the index over that field "
                    "or pass use_index=False to scan"
                )
            self._index = index

    def __enter__(self) -> SeekableJsonlReader:
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        self.close()

    def close(self) -> None:
        """Release the file handle; safe to call more than once."""

        if self._handle is not None:
            self._handle.close()
            self._handle = None

    @property
    def path(self) -> Path:
        """The compressed file being read."""

        return self._path

    @property
    def has_index(self) -> bool:
        """Whether a sidecar was found and accepted; false means reads are full scans."""

        return self._index is not None

    @property
    def index(self) -> ChunkIndex:
        """The loaded sidecar, or an error if this reader is scanning without one."""

        if self._index is None:
            raise SeekableJsonlError(f"{self._path}: no index is loaded")
        return self._index

    @property
    def timestamp_key(self) -> str:
        """The record field range queries read, as decided by the index when there is one."""

        if self._index is None:
            return self._fallback_timestamp_key
        return self._index.timestamp_key

    @property
    def data_bytes_read(self) -> int:
        """Bytes physically read from the compressed file since the last counter reset."""

        return self._data_bytes_read

    @property
    def index_bytes_read(self) -> int:
        """Bytes physically read from the sidecar since the last counter reset."""

        return self._index_bytes_read

    def reset_counters(self) -> None:
        """Zero the byte counters, so one reader can measure several operations."""

        self._data_bytes_read = 0
        self._index_bytes_read = 0

    def iter_lines(self) -> Iterator[bytes]:
        """Yield every record's bytes, newline stripped, in file order."""

        if self._index is None:
            for member in self._scan_members():
                yield from _split_member(member.data, str(self._path))
            return
        for entry in self._index.members:
            yield from _split_member(self._member_bytes(entry), str(self._path))

    def iter_records(self) -> Iterator[dict[str, JsonValue]]:
        """Yield every record, parsed, in file order."""

        for number, line in enumerate(self.iter_lines()):
            yield _record(line, f"{self._path} line {number}")

    def read_lines(self, first: int, last: int) -> Iterator[dict[str, JsonValue]]:
        """Yield records whose 0-based line number is in the half-open range ``[first, last)``."""

        if first < 0 or last < first:
            raise ValueError(f"invalid line range [{first}, {last})")
        if first == last:
            return
        if self._index is None:
            for number, line in enumerate(self.iter_lines()):
                if number >= last:
                    return
                if number >= first:
                    yield _record(line, f"{self._path} line {number}")
            return
        members = self._index.members
        if self._member_starts is None:
            self._member_starts = tuple(entry.l0 for entry in members)
        # `bisect_right - 1` finds the member whose line span contains `first`: starts are strictly
        # increasing except for the degenerate empty member, and the -1 steps back from "first
        # member starting after `first`" to "member `first` is inside".
        cursor = max(bisect_right(self._member_starts, first) - 1, 0)
        for entry in members[cursor:]:
            if entry.l0 >= last:
                return
            if entry.n == 0:
                continue
            lines = _split_member(self._member_bytes(entry), str(self._path))
            lo = max(first - entry.l0, 0)
            hi = min(last - entry.l0, entry.n)
            for offset in range(lo, hi):
                yield _record(lines[offset], f"{self._path} line {entry.l0 + offset}")

    def read_range(
        self, start_ms: int | None, end_ms: int | None
    ) -> Iterator[dict[str, JsonValue]]:
        """Yield records whose timestamp falls in the half-open range ``[start_ms, end_ms)``.

        ``None`` on either side means unbounded. Records with no timestamp are never selected: a
        record without a position on the time axis is not "before everything", it is absent from
        the axis, and returning it would make two adjacent range queries overlap.
        """

        if start_ms is not None and end_ms is not None and end_ms < start_ms:
            raise ValueError(f"invalid time range [{start_ms}, {end_ms})")
        key = self.timestamp_key
        if self._index is None:
            for number, line in enumerate(self.iter_lines()):
                where = f"{self._path} line {number}"
                record = _record(line, where)
                if _within(_timestamp_of(record, key, where), start_ms, end_ms):
                    yield record
            return
        for entry in self._candidates(start_ms, end_ms):
            lines = _split_member(self._member_bytes(entry), str(self._path))
            for offset, line in enumerate(lines):
                where = f"{self._path} line {entry.l0 + offset}"
                record = _record(line, where)
                if _within(_timestamp_of(record, key, where), start_ms, end_ms):
                    yield record

    def tail(self, count: int) -> list[dict[str, JsonValue]]:
        """Return the last *count* records, opening only the final member(s)."""

        if count < 0:
            raise ValueError("count must be non-negative")
        if count == 0:
            return []
        if self._index is None:
            recent: deque[tuple[int, bytes]] = deque(maxlen=count)
            for number, line in enumerate(self.iter_lines()):
                recent.append((number, line))
            return [_record(line, f"{self._path} line {number}") for number, line in recent]
        collected: list[bytes] = []
        first_line = self._index.record_count
        for entry in reversed(self._index.members):
            if entry.n == 0:
                continue
            collected = _split_member(self._member_bytes(entry), str(self._path)) + collected
            first_line = entry.l0
            if len(collected) >= count:
                break
        wanted = collected[-count:] if count < len(collected) else collected
        base = first_line + len(collected) - len(wanted)
        return [
            _record(line, f"{self._path} line {base + offset}")
            for offset, line in enumerate(wanted)
        ]

    def verify(self, *, deep: bool = False) -> None:
        """Raise if the pair is inconsistent; return silently if it is sound.

        The shallow form is what the constructor already ran. The deep form inflates every member
        and reproduces both digests, which is a full pass over the file and is therefore something
        a caller asks for, not something a read pays for.
        """

        if self._index is None:
            # Not an error: the fallback is a documented mode, not a degraded one. Walk the file
            # so a truncated member or trailing garbage is still reported.
            for _member in self._scan_members():
                pass
            return
        index = self._index
        if not deep:
            size = self._path.stat().st_size
            if size != index.c_size:
                raise IndexMismatchError(
                    f"{self._path}: {size} bytes on disk, index describes {index.c_size}"
                )
            return
        c_hash = hashlib.sha256()
        u_hash = hashlib.sha256()
        for entry in index.members:
            raw = self._raw_member(entry)
            c_hash.update(raw)
            u_hash.update(self._inflate(raw, entry))
        if c_hash.hexdigest() != index.c_sha256:
            raise IndexMismatchError(f"{self._path}: compressed digest disagrees with the index")
        if u_hash.hexdigest() != index.u_sha256:
            raise IndexMismatchError(f"{self._path}: uncompressed digest disagrees with the index")

    def _candidates(
        self, start_ms: int | None, end_ms: int | None
    ) -> Iterator[ChunkIndexEntry]:
        """Members that can contain a record in ``[start_ms, end_ms)``, in file order."""

        index = self.index
        members = index.members
        if not index.timestamps_sorted:
            # Linear over the member table, which lives in memory and is four orders of magnitude
            # smaller than the data. The number under test -- bytes read from the `.gz` -- is
            # identical to the bisect path; only the index walk is O(members) rather than O(log).
            for entry in members:
                if entry.overlaps(start_ms, end_ms):
                    yield entry
            return
        cursor = 0
        if start_ms is not None:
            # Members are sorted and non-overlapping in time, so the first member that can hold a
            # record at or after `start_ms` is the first whose t1 reaches it. A member with no
            # timestamps cannot exist here: `timestamps_sorted` is false whenever one does. The
            # ends table is built once per reader rather than per query, so the search really is
            # O(log members) and not a linear pass wearing a bisect for decoration.
            if self._member_ends is None:
                self._member_ends = tuple(
                    entry.t1 if entry.t1 is not None else -1 for entry in members
                )
            cursor = bisect_left(self._member_ends, start_ms)
        for entry in members[cursor:]:
            if end_ms is not None and entry.t0 is not None and entry.t0 >= end_ms:
                return
            if entry.overlaps(start_ms, end_ms):
                yield entry

    def _open(self) -> BinaryIO:
        if self._handle is None:
            self._handle = self._path.open("rb")
        return self._handle

    def _raw_member(self, entry: ChunkIndexEntry) -> bytes:
        """Read one member's compressed bytes -- the only place this class touches the file."""

        handle = self._open()
        handle.seek(entry.c_off)
        raw = handle.read(entry.c_len)
        self._data_bytes_read += len(raw)
        if len(raw) != entry.c_len:
            raise TruncatedArchiveError(
                f"{self._path}: member at {entry.c_off} is {len(raw)} bytes, "
                f"index says {entry.c_len}"
            )
        return raw

    def _member_bytes(self, entry: ChunkIndexEntry) -> bytes:
        if self._members is None:
            return self._inflate(self._raw_member(entry), entry)
        cached = self._members.get(entry.c_off)
        if cached is None:
            cached = self._inflate(self._raw_member(entry), entry)
            self._members[entry.c_off] = cached
        return cached

    def _inflate(self, raw: bytes, entry: ChunkIndexEntry) -> bytes:
        try:
            data = gzip.decompress(raw)
        except (EOFError, OSError, zlib.error) as exc:
            # Named as data damage because that is what it is on the scan path, but the message
            # has to admit the other possibility: reading through an index means the offsets came
            # from the sidecar, and a sidecar the open-time size check happened to agree with can
            # still point into the middle of a member. Telling an operator to re-derive the data
            # when the remedy is `rebuild_index` is the expensive direction to be wrong in.
            raise TruncatedArchiveError(
                f"{self._path}: member at {entry.c_off} did not inflate: {exc}. Either the data "
                "is damaged or the sidecar points into the wrong place; `rebuild_index` tells "
                "them apart without touching the data."
            ) from exc
        # Checked on every member read, unlike the digests. It is a length comparison against a
        # number gzip already had to compute, so it is free, and it turns "the index is stale in a
        # way the size check missed" from a wrong answer into an error at the exact member.
        if len(data) != entry.u_len:
            raise IndexMismatchError(
                f"{self._path}: member at {entry.c_off} inflates to {len(data)} bytes, "
                f"index says {entry.u_len}"
            )
        return data

    def _scan_members(self) -> Iterator[_RawMember]:
        """Walk the whole file member by member, counting the bytes it costs."""

        def note(count: int) -> None:
            self._data_bytes_read += count

        yield from _iter_members(self._path, note)


def rebuild_index(
    path: Path,
    *,
    target_chunk_bytes: int = DEFAULT_TARGET_CHUNK_BYTES,
    timestamp_key: str = DEFAULT_TIMESTAMP_KEY,
    codec_level: int = GZIP_COMPRESSION_LEVEL,
) -> ChunkIndex:
    """Reconstruct the sidecar index by walking *path*, without rewriting the data.

    This is the other half of "losing the index costs speed and nothing else": the index is not
    merely optional at read time, it is recoverable. The three keyword arguments are exactly the
    header fields that are *not* recoverable from the bytes -- how the members were produced, and
    which field the timestamps came from -- and everything else, including member boundaries and
    both digests, is derived. Given the same arguments the writer used, the result equals the
    written index exactly; the tests pin that, because it is what makes the claim checkable rather
    than aspirational.
    """

    members: list[ChunkIndexEntry] = []
    c_hash = hashlib.sha256()
    u_hash = hashlib.sha256()
    line_no = 0
    u_off = 0
    for member in _iter_members(path, _ignore_bytes):
        c_hash.update(member.raw)
        u_hash.update(member.data)
        lines = _split_member(member.data, str(path))
        t0: int | None = None
        t1: int | None = None
        for offset, line in enumerate(lines):
            where = f"{path} line {line_no + offset}"
            at = _timestamp_of(_record(line, where), timestamp_key, where)
            if at is None:
                continue
            t0 = at if t0 is None else min(t0, at)
            t1 = at if t1 is None else max(t1, at)
        members.append(
            ChunkIndexEntry(
                c_off=member.c_off,
                c_len=len(member.raw),
                u_off=u_off,
                u_len=len(member.data),
                l0=line_no,
                n=len(lines),
                t0=t0,
                t1=t1,
            )
        )
        line_no += len(lines)
        u_off += len(member.data)
    if not members:
        raise TruncatedArchiveError(f"{path}: not a gzip file (no members)")
    return ChunkIndex(
        version=INDEX_VERSION,
        codec=INDEX_CODEC,
        codec_level=codec_level,
        target_chunk_bytes=target_chunk_bytes,
        timestamp_key=timestamp_key,
        timestamps_sorted=_bisectable(members),
        record_count=line_no,
        c_size=sum(entry.c_len for entry in members),
        u_size=u_off,
        c_sha256=c_hash.hexdigest(),
        u_sha256=u_hash.hexdigest(),
        data_file=path.name,
        members=tuple(members),
    )


def write_rebuilt_index(
    path: Path,
    *,
    target_chunk_bytes: int = DEFAULT_TARGET_CHUNK_BYTES,
    timestamp_key: str = DEFAULT_TIMESTAMP_KEY,
    index_path: Path | None = None,
) -> ChunkIndex:
    """Rebuild the index for *path* and write it to the sidecar."""

    index = rebuild_index(
        path, target_chunk_bytes=target_chunk_bytes, timestamp_key=timestamp_key
    )
    sidecar = index_path if index_path is not None else index_path_for(path)
    write_text_if_changed(sidecar, index.to_text())
    return index


def _encode_record(
    record: Mapping[str, JsonValue], timestamp_key: str, number: int
) -> tuple[bytes, int | None]:
    where = f"record {number}"
    # Compact separators and sorted keys, matching the archive's existing JSONL projections
    # byte for byte. `allow_nan=False` refuses rather than emitting the bare `NaN` token, which
    # Python reads back happily and every other JSON parser rejects -- a lossless format cannot
    # ship a value only its own writer can read.
    line = json.dumps(
        dict(record),
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
        allow_nan=False,
    ).encode("utf-8")
    if _NEWLINE in line:
        raise ValueError(f"{where}: encoded record contains a newline")
    return line, _timestamp_of(record, timestamp_key, where)


def _encode_line(line: bytes, timestamp_key: str, number: int) -> tuple[bytes, int | None]:
    where = f"line {number}"
    if _NEWLINE in line:
        raise ValueError(f"{where}: pre-encoded line contains a newline")
    return line, _timestamp_of(_record(line, where), timestamp_key, where)


def _timestamp_of(record: Mapping[str, JsonValue], key: str, where: str) -> int | None:
    value = record.get(key)
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, int):
        raise ValueError(f"{where}: {key} is not an integer")
    return value


def _within(at: int | None, start_ms: int | None, end_ms: int | None) -> bool:
    if at is None:
        return False
    if start_ms is not None and at < start_ms:
        return False
    if end_ms is not None and at >= end_ms:
        return False
    return True


def _ignore_bytes(count: int) -> None:
    """Byte-counting sink for callers that are not measuring, such as an index rebuild."""


def _split_member(data: bytes, where: str) -> list[bytes]:
    if not data:
        return []
    if not data.endswith(_NEWLINE):
        raise TruncatedArchiveError(f"{where}: member does not end at a line boundary")
    return data[:-1].split(_NEWLINE)


def _record(line: bytes, where: str) -> dict[str, JsonValue]:
    """Parse one data line, reporting a non-record as an archive fault rather than a stdlib one.

    Every failure this module can produce is a :class:`SeekableJsonlError`, so that a caller can
    write one `except` and fall back to a scan. A member that inflates cleanly and does not hold
    JSON objects is not a chunked JSONL file, which is the same remedy as any other damage to the
    data: replace or re-derive it.
    """

    try:
        raw: object = json.loads(line.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise TruncatedArchiveError(f"{where}: not a JSON record: {exc}") from exc
    try:
        return as_object(narrow_json(raw, where), where)
    except ValueError as exc:
        raise TruncatedArchiveError(f"{where}: not a JSON record: {exc}") from exc


def _json_object(text: str, where: str) -> dict[str, JsonValue]:
    """Parse one index line, reporting a malformed sidecar as the sidecar's fault.

    `json.JSONDecodeError` escaping here would mean a caller's ``except SeekableJsonlError`` --
    the documented way to degrade to a full scan when the index is unusable -- did not catch the
    most ordinary way for an index to be unusable.
    """

    try:
        raw: object = json.loads(text)
    except json.JSONDecodeError as exc:
        raise IndexMismatchError(f"{where}: not a JSON object: {exc}") from exc
    try:
        return as_object(narrow_json(raw, where), where)
    except ValueError as exc:
        raise IndexMismatchError(f"{where}: not a JSON object: {exc}") from exc


def _json_line(value: Mapping[str, JsonValue]) -> str:
    """Encode one index line in the writer's declared key order.

    Unlike the records themselves, index lines are *not* sorted by key. Determinism comes from the
    dict literals that build them, which is just as strong a guarantee, and the declared order buys
    two things alphabetical order destroys: the header opens with `format` and `version`, so
    `head -c 64` identifies the file, and each member line reads in the order the schema documents
    -- compressed extent, uncompressed extent, lines, timestamps -- instead of interleaving the
    three coordinate systems as `c_len, c_off, l0, n, t0, t1, u_len, u_off`.
    """

    return json.dumps(
        dict(value),
        ensure_ascii=False,
        separators=(",", ":"),
        allow_nan=False,
    )


def _optional_int(value: JsonValue, where: str) -> int | None:
    if value is None:
        return None
    return as_int(value, where)


def _boolean(value: JsonValue, where: str) -> bool:
    if not isinstance(value, bool):
        raise IndexMismatchError(f"{where}: expected a boolean")
    return value


__all__: Sequence[str] = [
    "ChunkIndex",
    "ChunkIndexEntry",
    "DEFAULT_TARGET_CHUNK_BYTES",
    "DEFAULT_TIMESTAMP_KEY",
    "INDEX_CODEC",
    "INDEX_FORMAT",
    "INDEX_SUFFIX",
    "INDEX_VERSION",
    "IndexMismatchError",
    "SeekableJsonlError",
    "SeekableJsonlReader",
    "TruncatedArchiveError",
    "UnsupportedCodecError",
    "WriteReport",
    "index_path_for",
    "rebuild_index",
    "write_rebuilt_index",
    "write_seekable_jsonl",
    "write_seekable_jsonl_lines",
]
