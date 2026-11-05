#!/usr/bin/env python3
"""Read-only, machine-friendly navigation over a built timeline archive."""

from __future__ import annotations

import argparse
import base64
import gzip
import hashlib
import json
import os
import re
import sys
import zlib
from bisect import bisect_left, bisect_right
from collections.abc import Callable, Iterable, Iterator, Sequence
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path, PurePosixPath
from typing import BinaryIO, Protocol

QUERY_SCHEMA_VERSION = 1
TIMELINE_SCHEMA_VERSION = 1
TRANSCRIPT_EXPORT_SCHEMA_VERSION = 1
_WHITESPACE = re.compile(
    "[\u0009-\u000d\u001c-\u0020\u0085\u00a0\u1680"
    "\u2000-\u200a\u2028\u2029\u202f\u205f\u3000]+"
)
_TRIGRAM_BLOOM_ALGORITHM = "ascii-lower-utf8-trigram-fnv1a32-double-v1"
_TRIGRAM_BLOOM_HASH_COUNT = 7
_TRIGRAM_BLOOM_FNV_OFFSET = 2_166_136_261
_TRIGRAM_BLOOM_FNV_PRIME = 16_777_619
_TRIGRAM_BLOOM_SECOND_SEED = 0x9E37_79B9
_UINT32_MASK = (1 << 32) - 1
_RFC3339_INSTANT = re.compile(
    r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}"
    r"(?:\.[0-9]{1,6})?(?:Z|[+-][0-9]{2}:[0-9]{2})$"
)
PROMPT_SELECTIONS = ("human", "bot", "all")
_HUMAN_AUTHOR_KINDS = frozenset(("owner_human", "other_human"))
_BOT_AUTHOR_KINDS = frozenset(("agent", "system"))
SEARCH_CORPORA = (
    "owner-prompts",
    "agent-responses",
    "all-transcript",
)
SEARCH_MATCH_MODES = ("smart", "phrase", "literal")
SEARCH_SORTS = ("relevance", "newest", "oldest")
SEARCH_PROMPT_AUTHORS = ("any", "owner", "agent", "unclassified")
SEARCH_LINKAGES = ("any", "linked", "unlinked")
SEARCH_ROLES = (
    "user",
    "assistant",
    "agent",
    "system",
    "external",
    "goal",
    "tool",
    "event",
)

JsonScalar = str | int | float | bool | None
JsonValue = JsonScalar | list["JsonValue"] | dict[str, "JsonValue"]


class TimeWindow(Protocol):
    """The interval operations query filtering needs, plus the bounds a seek needs.

    ``contains`` and ``overlaps`` are enough to *filter* records one at a time, which is all
    this protocol used to promise, and it is exactly why every windowed query used to read
    the whole archive: a predicate can reject a record but it cannot say where to start
    reading. The bounds are declared here so a window can be turned into a byte offset. They
    are read-only properties rather than plain attributes so that the frozen dataclasses that
    implement this protocol satisfy it without being made mutable.
    """

    @property
    def start_ms(self) -> int | None:
        """Inclusive lower bound in epoch milliseconds, or ``None`` for unbounded."""

    @property
    def end_ms(self) -> int | None:
        """Exclusive upper bound in epoch milliseconds, or ``None`` for unbounded."""

    def contains(self, timestamp_ms: int) -> bool:
        """Return whether a point falls inside this half-open window."""

    def overlaps(self, start_ms: int, end_ms: int | None) -> bool:
        """Return whether an interval overlaps this half-open window."""


def _narrow_json(raw: object, where: str) -> JsonValue:
    if raw is None or isinstance(raw, (str, bool, int, float)):
        return raw
    if isinstance(raw, list):
        return [_narrow_json(item, where) for item in raw]
    if isinstance(raw, dict):
        result: dict[str, JsonValue] = {}
        for key, value in raw.items():
            if not isinstance(key, str):
                raise ValueError(f"{where}: object key is not a string")
            result[key] = _narrow_json(value, where)
        return result
    raise ValueError(f"{where}: unsupported JSON value {type(raw).__name__}")


def read_json(path: Path) -> JsonValue:
    """Read and recursively narrow a JSON document."""

    raw: object = json.loads(path.read_text(encoding="utf-8"))
    return _narrow_json(raw, str(path))


def as_object(value: JsonValue, where: str) -> dict[str, JsonValue]:
    """Require one JSON object and retain its recursively narrowed type."""

    if not isinstance(value, dict):
        raise ValueError(f"{where}: expected an object")
    return value


def as_array(value: JsonValue, where: str) -> list[JsonValue]:
    """Require one JSON array and retain its recursively narrowed type."""

    if not isinstance(value, list):
        raise ValueError(f"{where}: expected an array")
    return value


def as_string(value: JsonValue, where: str) -> str:
    """Require one JSON string with a location-aware validation error."""

    if not isinstance(value, str):
        raise ValueError(f"{where}: expected a string")
    return value


def as_int(value: JsonValue, where: str) -> int:
    """Require one JSON integer while rejecting booleans."""

    if not isinstance(value, int) or isinstance(value, bool):
        raise ValueError(f"{where}: expected an integer")
    return value


def canonical_json(value: JsonValue) -> str:
    """Serialize a narrowed JSON value deterministically for CLI output."""

    return json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n"


@dataclass(frozen=True)
class QueryFilters:
    """Provider-neutral filters shared by list and search operations."""

    teams: tuple[str, ...] = ()
    window: TimeWindow | None = None
    rollup_kinds: tuple[str, ...] = ()
    agent_ref: str | None = None


@dataclass(frozen=True)
class SearchResults:
    """One ranked search page plus complete result-set metadata."""

    items: tuple[dict[str, JsonValue], ...]
    total_matches: int
    offset: int
    limit: int
    corpus: str
    match_mode: str
    sort: str

    @property
    def truncated(self) -> bool:
        """Return whether matches exist outside this page."""

        return self.offset > 0 or self.offset + len(self.items) < self.total_matches


@dataclass(frozen=True)
class _TextMatch:
    compact: str
    ranges: tuple[tuple[int, int], ...]
    score: int


@dataclass(frozen=True)
class _SearchMatcher:
    compact_needle: str
    patterns: tuple[re.Pattern[str], ...]
    whole_words: tuple[bool, ...]
    bloom_terms: tuple[str, ...]
    case_sensitive: bool

    def match(self, text: str) -> _TextMatch | None:
        """Return match ranges and a deterministic relevance score."""

        compact = _compact_text(text)
        collected: list[tuple[int, int]] = []
        for pattern, whole_word in zip(self.patterns, self.whole_words, strict=True):
            candidate = _pattern_ranges(pattern, compact, whole_word=whole_word)
            if not candidate:
                return None
            collected.extend(candidate)
        ranges = tuple(sorted(set(collected)))[:64]
        first = min(start for start, _ in ranges)
        last = max(end for _, end in ranges)
        span = max(1, last - first)
        comparable = compact if self.case_sensitive else _ascii_lower_text(compact)
        comparable_needle = (
            self.compact_needle
            if self.case_sensitive
            else _ascii_lower_text(self.compact_needle)
        )
        exact = comparable == comparable_needle
        score = (
            (100_000 if exact else 0)
            + max(0, 20_000 - span)
            + len(ranges) * 100
            - min(first, 10_000)
        )
        return _TextMatch(compact, ranges, score)


@dataclass(frozen=True)
class _SearchLinkContext:
    """Relationship facts retained independently of text-shard Bloom pruning."""

    prompt_excerpts: dict[str, str]
    response_counts: dict[str, int]


@dataclass(frozen=True)
class _IndexEntry:
    kind: str
    record: dict[str, JsonValue]


@dataclass(frozen=True)
class OrdinalRange:
    """One-based inclusive prompt ordinal bounds."""

    first: int
    last: int

    def contains(self, ordinal: int) -> bool:
        """Return whether *ordinal* lies inside this inclusive range."""

        return self.first <= ordinal <= self.last


@dataclass(frozen=True)
class TextTotals:
    """Additive counts over record text, excluding serialization framing."""

    records: int = 0
    words: int = 0
    utf8_bytes: int = 0

    @classmethod
    def from_texts(cls, texts: Iterable[str]) -> TextTotals:
        """Count records, whitespace-delimited words, and UTF-8 text bytes."""

        records = 0
        words = 0
        utf8_bytes = 0
        for text in texts:
            records += 1
            words += len(text.split())
            utf8_bytes += len(text.encode("utf-8"))
        return cls(records, words, utf8_bytes)

    def __add__(self, other: TextTotals) -> TextTotals:
        return TextTotals(
            self.records + other.records,
            self.words + other.words,
            self.utf8_bytes + other.utf8_bytes,
        )

    def to_mapping(self) -> dict[str, JsonValue]:
        """Return these totals as a JSON-compatible object."""

        return {
            "records": self.records,
            "words": self.words,
            "utf8_bytes": self.utf8_bytes,
        }


@dataclass(frozen=True)
class SummaryKindStats:
    """Availability and generated-text totals for one summary surface."""

    available: int
    unavailable: int
    content: TextTotals

    def to_mapping(self) -> dict[str, JsonValue]:
        """Return coverage and content totals as a JSON-compatible object."""

        return {
            "available": self.available,
            "unavailable": self.unavailable,
            "total": self.available + self.unavailable,
            "content": self.content.to_mapping(),
        }


def _check_page(limit: int | None, tail: int | None) -> None:
    """Validate the two paging flags, and refuse the combination that has no meaning.

    ``--limit`` takes the first N of the selection and ``--tail`` the last N. Together they
    would have to mean one of two different things -- the first N of the last M, or the last
    N of the first M -- and a reader cannot tell which from the command line. Refusing is
    cheaper than picking, and far cheaper than picking silently.
    """

    if limit is not None and limit < 1:
        raise ValueError("--limit must be at least 1")
    if tail is not None and tail < 1:
        raise ValueError("--tail must be at least 1")
    if limit is not None and tail is not None:
        raise ValueError("--limit and --tail select opposite ends; choose one")


def parse_ordinal_range(raw: str) -> OrdinalRange:
    """Parse ``N`` or inclusive ``N-M`` prompt ordinals."""

    match = re.fullmatch(r"([1-9][0-9]*)(?:-([1-9][0-9]*))?", raw)
    if match is None:
        raise ValueError("--range must be a positive ordinal or inclusive N-M range")
    first = int(match.group(1))
    last = int(match.group(2) or match.group(1))
    if first > last:
        raise ValueError("--range first ordinal must not exceed its last ordinal")
    return OrdinalRange(first, last)


# =====================================================================================
# Reading a slice instead of a file
# =====================================================================================
#
# Everything between here and `TranscriptQuery` exists to make an answer cost the size of
# the *span the answer lives in* rather than the size of the file. That is deliberately a
# weaker claim than "the size of the answer", because one shape does not reach it and saying
# so here is cheaper than a reader discovering it with a stopwatch: `list_messages` can bound
# where a response scan *starts* (no response precedes the prompt it replies to) and cannot
# bound where it *ends* (a response may arrive arbitrarily later), so selecting the first few
# prompts of an archive and asking for their replies still walks the message projection to
# EOF unless a `--limit`, a `--tail` or a window end stops it. See `list_messages` for why no
# sound upper bound exists rather than merely not having been written yet.
#
# Two physical formats need slicing, and they get two readers rather than one, because they
# are genuinely different problems:
#
# `_ChunkedJsonlReader` reads a schema-3 shard -- a multi-member gzip file with the sidecar
# index written by `wrkviz.seekable_jsonl`. It is a *reimplementation* of that
# module's reader, and the duplication is deliberate and load-bearing. This file is copied
# verbatim into every generated archive as the standalone `./timeline` executable, which is
# pinned by `test_bundled_query_source_runs_without_the_installed_package`: it runs with no
# `wrkviz` package on the path, so it may import nothing but the standard
# library. The alternatives were to make the launcher a multi-file bundle (losing the single
# executable an archive owner can copy anywhere) or to concatenate modules at render time
# (a build step whose output nobody reads until it is broken). Instead the copy is pinned by
# a differential: `test_query_chunk_reader_matches_the_package_reader` asserts that this
# reader and `seekable_jsonl.SeekableJsonlReader` return the same records *and* read the same
# number of bytes from the same shard. A comment saying "keep these in sync" would not have
# survived the first divergence; a failing test will.
#
# `_SchemaThreeArchive`, below it, is what calls it: every list, show, search and stats path in
# this file now reads `data/timeline-v3.json` and its spine shards when a complete schema-3
# generation is present, and falls back to schema 2 and then schema 1 when it is not. See that
# class for the completeness rule and for which stream answers which question.
#
# `_SeekableJsonlText` reads the *uncompressed* transcript projections under
# `extracted/transcripts/`. Those have no sidecar and deliberately gain none: adding a
# chunked `.gz` twin beside them would recreate exactly the duplication schema 3 exists to
# remove (the archive already carries 2.42 GB of `.json` beside 0.19 GB of `.gz`), and it is
# unnecessary, because an uncompressed file sorted on its key can be binary-searched in
# place. The two invariants that makes sound are writer invariants, not lucky properties of
# today's data: `transcript_export` emits both files through `sorted(..., key=_record_sort_key)`
# whose first component is `timestamp_ms`, and assigns `ordinal` by `enumerate(prompts, 1)`
# over that same sorted list, so in `prompts.jsonl` the ordinal is exactly the line number
# plus one. Both are re-checked at the records a seek actually lands on, so a projection that
# ever stops honouring them fails loudly instead of silently returning the wrong window.


class ArchiveReadError(ValueError):
    """A managed file this reader cannot navigate.

    A subclass of ``ValueError`` because every caller in this file already funnels
    ``ValueError`` into the CLI's exit-2 path with the message attached; the distinct class
    is for the reader that wants to tell "this archive is damaged or stale" apart from "this
    request was malformed" without matching on prose.
    """


_CHUNK_INDEX_FORMAT = "wrkviz/seekable-jsonl-index"
_CHUNK_INDEX_VERSION = 1
_CHUNK_INDEX_CODEC = "gzip"
_CHUNK_INDEX_SUFFIX = ".index.jsonl"

# Read granularity when a reader is deliberately walking a whole file: large enough that
# syscall overhead disappears against parsing.
_SCAN_BLOCK_BYTES = 1 << 18

# Read granularity for a bisect probe, which wants one record and nothing else. Deliberately
# far smaller than `_SCAN_BLOCK_BYTES`: a bisect over the 105 MB message projection takes
# about twenty-seven probes, so reading a scan-sized block at each one would turn a seek that
# should cost a few hundred kilobytes into seven megabytes. The mean transcript record is
# about 1.6 KB, so 4 KiB answers most probes in one read; a longer record is caught by
# doubling rather than by a second guess at the right constant, which bounds the wasted read
# at one record's length however long that record turns out to be.
_PROBE_BLOCK_BYTES = 1 << 12

# The first backwards read of a tail. Twenty records at the measured mean fit inside it, so
# the common tail costs one read; the window doubles rather than guessing again.
_TAIL_BLOCK_BYTES = 1 << 16

_GZIP_WBITS = 16 + zlib.MAX_WBITS


@dataclass(frozen=True)
class _ChunkMember:
    """One gzip member's extent, in all three coordinate systems the index publishes."""

    c_off: int
    c_len: int
    u_off: int
    u_len: int
    l0: int
    n: int
    t0: int | None
    t1: int | None

    def overlaps(self, start_ms: int | None, end_ms: int | None) -> bool:
        """Whether this member can hold a record in the half-open range ``[start, end)``."""

        if self.t0 is None or self.t1 is None:
            return False
        if start_ms is not None and self.t1 < start_ms:
            return False
        return end_ms is None or self.t0 < end_ms


@dataclass(frozen=True)
class _ChunkIndex:
    """A parsed sidecar: the header fields a reader acts on, plus the member table."""

    timestamp_key: str
    timestamps_sorted: bool
    record_count: int
    c_size: int
    u_size: int
    c_sha256: str
    u_sha256: str
    data_file: str
    members: tuple[_ChunkMember, ...]


def _optional_json_int(value: JsonValue, where: str) -> int | None:
    return None if value is None else as_int(value, where)


def _parse_chunk_index(text: str, where: str) -> _ChunkIndex:
    """Parse a sidecar and check that its member table is contiguous.

    Contiguity is the invariant every read below assumes, so it is checked once here rather
    than re-derived per read. Split on ``"\\n"`` rather than ``str.splitlines()``: the sidecar
    is written with ``ensure_ascii=False``, so a ``data_file`` carrying U+2028 -- which JSON
    does not escape and ``splitlines`` does treat as a terminator -- would be written as one
    line and read back as two.
    """

    lines = [line for line in text.split("\n") if line]
    if not lines:
        raise ArchiveReadError(f"{where}: index is empty")
    header = as_object(_narrow_json(json.loads(lines[0]), where), f"{where} header")
    if as_string(header.get("format"), f"{where} header.format") != _CHUNK_INDEX_FORMAT:
        raise ArchiveReadError(f"{where}: not a chunked-JSONL index")
    version = as_int(header.get("version"), f"{where} header.version")
    if version != _CHUNK_INDEX_VERSION:
        raise ArchiveReadError(f"{where}: unsupported index version {version}")
    codec = as_string(header.get("codec"), f"{where} header.codec")
    if codec != _CHUNK_INDEX_CODEC:
        raise ArchiveReadError(
            f"{where}: index describes codec {codec!r}; this reader implements "
            f"{_CHUNK_INDEX_CODEC!r} only"
        )
    members: list[_ChunkMember] = []
    c_off = 0
    u_off = 0
    line_no = 0
    for offset, raw_line in enumerate(lines[1:]):
        spot = f"{where} member {offset}"
        obj = as_object(_narrow_json(json.loads(raw_line), spot), spot)
        member = _ChunkMember(
            c_off=as_int(obj.get("c_off"), f"{spot}.c_off"),
            c_len=as_int(obj.get("c_len"), f"{spot}.c_len"),
            u_off=as_int(obj.get("u_off"), f"{spot}.u_off"),
            u_len=as_int(obj.get("u_len"), f"{spot}.u_len"),
            l0=as_int(obj.get("l0"), f"{spot}.l0"),
            n=as_int(obj.get("n"), f"{spot}.n"),
            t0=_optional_json_int(obj.get("t0"), f"{spot}.t0"),
            t1=_optional_json_int(obj.get("t1"), f"{spot}.t1"),
        )
        if member.c_off != c_off or member.u_off != u_off or member.l0 != line_no:
            raise ArchiveReadError(f"{spot}: not contiguous with the preceding members")
        if member.c_len <= 0 or member.u_len < 0 or member.n < 0:
            raise ArchiveReadError(f"{spot}: non-positive extent")
        members.append(member)
        c_off += member.c_len
        u_off += member.u_len
        line_no += member.n
    if not members:
        raise ArchiveReadError(f"{where}: index describes no members")
    if as_int(header.get("member_count"), f"{where} header.member_count") != len(members):
        raise ArchiveReadError(f"{where}: header member count disagrees with the table")
    record_count = as_int(header.get("record_count"), f"{where} header.record_count")
    c_size = as_int(header.get("c_size"), f"{where} header.c_size")
    u_size = as_int(header.get("u_size"), f"{where} header.u_size")
    if record_count != line_no or c_size != c_off or u_size != u_off:
        raise ArchiveReadError(f"{where}: header totals disagree with the member table")
    sorted_claim = header.get("timestamps_sorted")
    if not isinstance(sorted_claim, bool):
        raise ArchiveReadError(f"{where} header.timestamps_sorted: expected a boolean")
    return _ChunkIndex(
        timestamp_key=as_string(
            header.get("timestamp_key"), f"{where} header.timestamp_key"
        ),
        # The header's claim is confirmed against the table rather than trusted. A member
        # with no timestamps has no position on the axis, and one of those anywhere but the
        # tail makes the bisect key non-monotonic -- and a binary search over a
        # non-monotonic sequence does not fail, it silently returns a cursor past real data.
        timestamps_sorted=sorted_claim and _members_are_bisectable(members),
        record_count=record_count,
        c_size=c_size,
        u_size=u_size,
        c_sha256=as_string(header.get("c_sha256"), f"{where} header.c_sha256"),
        u_sha256=as_string(header.get("u_sha256"), f"{where} header.u_sha256"),
        data_file=as_string(header.get("data_file"), f"{where} header.data_file"),
        members=tuple(members),
    )


def _members_are_bisectable(members: Sequence[_ChunkMember]) -> bool:
    """Whether the member table's time bounds are non-decreasing, so a bisect is sound."""

    previous: int | None = None
    for member in members:
        if member.t0 is None or member.t1 is None:
            return False
        if previous is not None and member.t0 < previous:
            return False
        previous = member.t1
    return True


def _split_member(data: bytes, where: str) -> list[bytes]:
    if not data:
        return []
    if not data.endswith(b"\n"):
        raise ArchiveReadError(f"{where}: member does not end at a line boundary")
    return data[:-1].split(b"\n")


def _decode_record(line: bytes, where: str) -> dict[str, JsonValue]:
    try:
        raw: object = json.loads(line.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ArchiveReadError(f"{where}: not a JSON record: {error}") from error
    return as_object(_narrow_json(raw, where), where)


class _ChunkedJsonlReader:
    """Read a schema-3 shard, touching only the members an answer needs.

    Every method is O(result) in bytes read from the shard when the sidecar is present, and
    correct-but-O(file) when it is absent -- losing the index costs speed and nothing else.
    :attr:`data_bytes_read` counts physical reads from the ``.gz`` so that claim can be
    asserted on rather than timed; :attr:`index_bytes_read` counts the one-time sidecar read
    separately, because the sidecar is a roughly fixed 0.17% of the shard set and folding it
    into the same counter would obscure exactly the number under test.
    """

    def __init__(
        self,
        path: Path,
        *,
        index_path: Path | None = None,
        cache_members: bool = False,
    ) -> None:
        self.path = path
        self.data_bytes_read = 0
        self.index_bytes_read = 0
        self._index: _ChunkIndex | None = None
        self._member_starts: tuple[int, ...] = ()
        self._member_ends: tuple[int, ...] = ()
        # Off by default, because the general case is a reader used once over a shard far larger
        # than memory. A caller opts in when it will ask several questions of one shard whose
        # answers live in overlapping members -- a schema-3 spine, where the agents, the phase
        # cards and the rollups of one team are three line ranges inside the same ~1 MiB member.
        # Without it, "the agents, then the phase cards, then the rollups" inflates that member
        # three times and `data_bytes_read` honestly reports three times the bytes; with it, the
        # counter reports what a reader that keeps what it has already paid for actually costs.
        self._members: dict[int, list[bytes]] | None = {} if cache_members else None
        sidecar = (
            index_path
            if index_path is not None
            else path.with_name(path.name + _CHUNK_INDEX_SUFFIX)
        )
        if path.is_symlink() or not path.is_file():
            raise ArchiveReadError(f"archive shard is missing or unsafe: {path}")
        if sidecar.is_symlink() or not sidecar.is_file():
            return
        raw = sidecar.read_bytes()
        self.index_bytes_read += len(raw)
        try:
            index = _parse_chunk_index(raw.decode("utf-8"), str(sidecar))
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise ArchiveReadError(f"{sidecar}: malformed index: {error}") from error
        # Three O(1) agreement checks before any read trusts the table. A stale sidecar does
        # not produce a slow answer, it produces a confidently wrong one -- records
        # attributed to the wrong timestamps, a tail that returns the middle of the file.
        if index.data_file != path.name:
            raise ArchiveReadError(
                f"{sidecar}: index describes {index.data_file!r}, not {path.name!r}"
            )
        size = path.stat().st_size
        if size != index.c_size:
            raise ArchiveReadError(
                f"{sidecar}: index describes {index.c_size} compressed bytes but "
                f"{path} holds {size}"
            )
        self._index = index
        self._member_starts = tuple(member.l0 for member in index.members)
        self._member_ends = tuple(
            member.t1 if member.t1 is not None else -1 for member in index.members
        )

    @property
    def has_index(self) -> bool:
        """Whether a sidecar was found and accepted; false means reads are full scans."""

        return self._index is not None

    @property
    def index(self) -> _ChunkIndex:
        """The accepted sidecar, or an error if this reader is scanning without one."""

        if self._index is None:
            raise ArchiveReadError(f"{self.path}: no index is loaded")
        return self._index

    @property
    def record_count(self) -> int:
        """How many records the shard holds, from the index or from a scan."""

        if self._index is not None:
            return self._index.record_count
        return sum(1 for _ in self.iter_records())

    def iter_records(self) -> Iterator[dict[str, JsonValue]]:
        """Yield every record in file order, whether or not there is an index."""

        if self._index is None:
            yield from self._scan()
            return
        with self.path.open("rb") as handle:
            for member in self._index.members:
                for offset, line in enumerate(self._member_lines(handle, member)):
                    yield _decode_record(line, f"{self.path} line {member.l0 + offset}")

    def read_lines(self, first: int, last: int) -> Iterator[dict[str, JsonValue]]:
        """Yield records whose 0-based line number is in the half-open ``[first, last)``."""

        if first < 0 or last < first:
            raise ArchiveReadError(f"invalid line range [{first}, {last})")
        if first == last:
            return
        if self._index is None:
            for number, record in enumerate(self._scan()):
                if number >= last:
                    return
                if number >= first:
                    yield record
            return
        members = self._index.members
        # `bisect_right - 1` steps back from "first member starting after `first`" to "the
        # member `first` is inside"; starts are strictly increasing except for a degenerate
        # empty member, which the max() guards.
        cursor = max(bisect_right(self._member_starts, first) - 1, 0)
        with self.path.open("rb") as handle:
            for member in members[cursor:]:
                if member.l0 >= last:
                    return
                if member.n == 0:
                    continue
                lines = self._member_lines(handle, member)
                lo = max(first - member.l0, 0)
                hi = min(last - member.l0, member.n)
                for offset in range(lo, hi):
                    yield _decode_record(
                        lines[offset], f"{self.path} line {member.l0 + offset}"
                    )

    def read_range(
        self, start_ms: int | None, end_ms: int | None
    ) -> Iterator[dict[str, JsonValue]]:
        """Yield records whose timestamp falls in the half-open ``[start_ms, end_ms)``.

        A record with no timestamp is never selected: a record without a position on the time
        axis is not "before everything", it is absent from the axis, and returning it would
        make two adjacent range queries overlap.
        """

        key = self._index.timestamp_key if self._index is not None else "at_ms"
        if self._index is None:
            for record in self._scan():
                at_ms = record.get(key)
                if isinstance(at_ms, int) and not isinstance(at_ms, bool):
                    if _within(at_ms, start_ms, end_ms):
                        yield record
            return
        with self.path.open("rb") as handle:
            for member in self._candidates(start_ms, end_ms):
                for offset, line in enumerate(self._member_lines(handle, member)):
                    where = f"{self.path} line {member.l0 + offset}"
                    record = _decode_record(line, where)
                    at_ms = record.get(key)
                    if not isinstance(at_ms, int) or isinstance(at_ms, bool):
                        continue
                    if _within(at_ms, start_ms, end_ms):
                        yield record

    def tail(self, count: int) -> list[dict[str, JsonValue]]:
        """Return the last *count* records, opening only the final member(s)."""

        if count <= 0:
            return []
        if self._index is None:
            collected: list[dict[str, JsonValue]] = []
            for record in self._scan():
                collected.append(record)
                if len(collected) > count:
                    del collected[0]
            return collected
        lines: list[bytes] = []
        first_line = self._index.record_count
        with self.path.open("rb") as handle:
            for member in reversed(self._index.members):
                if member.n == 0:
                    continue
                lines = self._member_lines(handle, member) + lines
                first_line = member.l0
                if len(lines) >= count:
                    break
        wanted = lines[-count:] if count < len(lines) else lines
        base = first_line + len(lines) - len(wanted)
        return [
            _decode_record(line, f"{self.path} line {base + offset}")
            for offset, line in enumerate(wanted)
        ]

    def _candidates(
        self, start_ms: int | None, end_ms: int | None
    ) -> Iterator[_ChunkMember]:
        index = self.index
        if not index.timestamps_sorted:
            # Linear over the member table, which lives in memory and is four orders of
            # magnitude smaller than the data. The number under test -- bytes read from the
            # shard -- is identical to the bisect path; only the table walk is O(members).
            for member in index.members:
                if member.overlaps(start_ms, end_ms):
                    yield member
            return
        cursor = 0
        if start_ms is not None:
            cursor = bisect_left(self._member_ends, start_ms)
        for member in index.members[cursor:]:
            if end_ms is not None and member.t0 is not None and member.t0 >= end_ms:
                return
            if member.overlaps(start_ms, end_ms):
                yield member

    def _member_lines(self, handle: BinaryIO, member: _ChunkMember) -> list[bytes]:
        if self._members is not None:
            cached = self._members.get(member.c_off)
            if cached is not None:
                return cached
            lines = self._read_member_lines(handle, member)
            self._members[member.c_off] = lines
            return lines
        return self._read_member_lines(handle, member)

    def _read_member_lines(self, handle: BinaryIO, member: _ChunkMember) -> list[bytes]:
        handle.seek(member.c_off)
        raw = handle.read(member.c_len)
        self.data_bytes_read += len(raw)
        if len(raw) != member.c_len:
            raise ArchiveReadError(
                f"{self.path}: member at {member.c_off} is {len(raw)} bytes, "
                f"index says {member.c_len}"
            )
        try:
            data = gzip.decompress(raw)
        except (EOFError, OSError, zlib.error) as error:
            raise ArchiveReadError(
                f"{self.path}: member at {member.c_off} did not inflate: {error}"
            ) from error
        # Free, because gzip already computed it, and it turns "the sidecar is stale in a way
        # the size check missed" from a wrong answer into an error at the exact member.
        if len(data) != member.u_len:
            raise ArchiveReadError(
                f"{self.path}: member at {member.c_off} inflates to {len(data)} bytes, "
                f"index says {member.u_len}"
            )
        return _split_member(data, str(self.path))

    def _scan(self) -> Iterator[dict[str, JsonValue]]:
        """Walk the whole shard when there is no usable sidecar, counting what it costs.

        A member boundary is *not* a line boundary as far as this loop is concerned, and that
        is the whole subtlety. The read window is a fixed number of **compressed** bytes, so
        each inflate hands back an arbitrary prefix of the stream, ending mid-record whenever
        a member happens to compress to more than one window -- which is not exotic: a shard
        of incompressible payloads reaches 796,663 compressed bytes in a single member at the
        default ~1 MiB target, three windows' worth. Handing that prefix to
        :func:`_split_member`, which requires a trailing newline because a *whole member* has
        one, turned "the sidecar is missing" into "the shard is unreadable" -- the opposite of
        the guarantee this method exists to provide, and on precisely the path that runs when
        the index has been lost. Lines are therefore reassembled across both window and member
        boundaries, and the only place a newline is demanded is the end of the stream.
        """

        number = 0
        pending = b""
        remainder = b""
        with self.path.open("rb") as handle:
            engine = zlib.decompressobj(_GZIP_WBITS)
            while True:
                if not pending:
                    pending = handle.read(_SCAN_BLOCK_BYTES)
                    if not pending:
                        break
                    self.data_bytes_read += len(pending)
                try:
                    chunk = engine.decompress(pending)
                except zlib.error as error:
                    raise ArchiveReadError(
                        f"{self.path}: not a gzip member: {error}"
                    ) from error
                if chunk:
                    remainder += chunk
                    cut = remainder.rfind(b"\n")
                    if cut >= 0:
                        complete, remainder = remainder[:cut], remainder[cut + 1 :]
                        for line in complete.split(b"\n"):
                            yield _decode_record(line, f"{self.path} line {number}")
                            number += 1
                if engine.eof:
                    pending = engine.unused_data
                    engine = zlib.decompressobj(_GZIP_WBITS)
                else:
                    pending = b""
        if remainder:
            raise ArchiveReadError(
                f"{self.path}: shard does not end at a line boundary"
            )


def _within(at_ms: int, start_ms: int | None, end_ms: int | None) -> bool:
    if start_ms is not None and at_ms < start_ms:
        return False
    return end_ms is None or at_ms < end_ms


SCHEMA_3_BOOTSTRAP_PATH = "data/timeline-v3.json"
_SCHEMA_3_ROOT = "data/timeline-v3"
_SCHEMA_3_KIND = "timeline-v3-bootstrap"
_SCHEMA_3_VERSION = 3
_SCHEMA_3_CONTAINER = "multi-member-gzip"
_SCHEMA_3_TIMESTAMP_KEY = "at_ms"
_SCHEMA_3_RECORD_KIND_KEY = "record_kind"
_SCHEMA_3_STREAMS = ("timeline", "spine", "bins")

#: The three streams that carry the transcript search corpus. Optional, and all-or-nothing: an
#: archive built before the corpus moved into schema 3 names none of them and gets the schema-2
#: corpus, while one that names some but not all is a generation this reader declines rather than
#: half-reads -- a corpus without its prefilter would silently over-read, and one without its
#: relationship sidecar would answer with linkage it cannot see, which is a wrong answer rather
#: than a slow one.
_SCHEMA_3_SEARCH_STREAMS = ("search", "search_bloom", "search_links")

_SCHEMA_3_SEARCH_KIND = "search_record"
_SCHEMA_3_BLOOM_KIND = "search_bloom"
_SCHEMA_3_PROMPT_LINK_KIND = "search_prompt"
_SCHEMA_3_RESPONSE_LINK_KIND = "search_response"

#: One UTC day, in milliseconds. A ``search`` shard's window is the day its catalogue entry names,
#: derived rather than published: the entry already carries ``day``, and a second copy of the same
#: interval is a second thing that can disagree with the records inside it.
_DAY_MS = 24 * 60 * 60 * 1000

#: Which spine kind answers a question about each stable-reference kind. ``phase`` is the
#: outlier and deliberately so: a schema-3 spine carries the phase *card*, the same nine fields
#: schema 2's phase index publishes, not the full phase with its state runs and transcript
#: pointer. Every phase question this file asks -- list, search, summary coverage, the
#: ``detail_path`` `show` follows -- is answerable from the card, and the full phase stays in
#: the timeline stream where it is one record among 379,006 events rather than something a
#: listing has to walk past.
_SCHEMA_3_SPINE_KIND_FOR: dict[str, str] = {
    "agent": "agent",
    "phase": "phase_card",
    "rollup": "rollup",
}

#: The derived spine kind carrying ``activity_start_ms``/``activity_end_ms``, keyed by the same
#: stable reference `show` takes.
_SCHEMA_3_BOUNDS_KIND = "activity_bounds"

#: Schema 1 names its collections in the plural and its references in the singular. Both spellings
#: are load-bearing -- ``timeline.agents`` is a field name, ``agent:`` is a reference prefix -- so
#: the mapping is written down once here rather than reconstructed by trimming an ``s``.
_SINGULAR: dict[str, str] = {"agents": "agent", "phases": "phase", "rollups": "rollup"}


class SchemaThreeDeclined(ValueError):
    """A schema-3 generation this reader will not read, with the reason it will not.

    Raised inside :meth:`_SchemaThreeArchive.open` and caught there: the caller gets ``None``
    and a sentence, not an exception, because "schema 3 is not usable" must always fall back to
    schema 2 rather than fail the query. The class exists so the refusals can be written as
    ordinary early returns instead of as a chain of nested conditionals, and so a test can
    match the sentence.
    """


@dataclass(frozen=True)
class _ShardEntry:
    """One catalog entry from the schema-3 bootstrap.

    ``line_ranges`` is empty for a time-addressed shard and ``t0``/``t1`` are ``None`` for a
    spine shard; a schema-3 stream is addressed one way or the other and never both.
    """

    stream: str
    team: str | None
    day: str | None
    path: str
    index_path: str
    records: int
    c_bytes: int
    t0: int | None
    t1: int | None
    t_end_exclusive: int | None
    line_ranges: dict[str, tuple[int, int]]


def _schema_3_line_ranges(raw: JsonValue, where: str) -> dict[str, tuple[int, int]]:
    if raw is None:
        return {}
    ranges: dict[str, tuple[int, int]] = {}
    for kind, value in as_object(raw, where).items():
        pair = as_array(value, f"{where}.{kind}")
        if len(pair) != 2:
            raise SchemaThreeDeclined(f"{where}.{kind}: expected [first, count]")
        first = as_int(pair[0], f"{where}.{kind}[0]")
        count = as_int(pair[1], f"{where}.{kind}[1]")
        if first < 0 or count < 0:
            raise SchemaThreeDeclined(f"{where}.{kind}: negative line range")
        ranges[kind] = (first, count)
    return ranges


def _schema_3_shard(raw: JsonValue, stream: str, where: str) -> _ShardEntry:
    entry = as_object(raw, where)
    team = entry.get("team")
    day = entry.get("day")
    return _ShardEntry(
        stream=stream,
        team=None if team is None else as_string(team, where + ".team"),
        day=None if day is None else as_string(day, where + ".day"),
        path=as_string(entry.get("path"), where + ".path"),
        index_path=as_string(entry.get("index_path"), where + ".index_path"),
        records=as_int(entry.get("records"), where + ".records"),
        c_bytes=as_int(entry.get("c_bytes"), where + ".c_bytes"),
        t0=_optional_json_int(entry.get("t0"), where + ".t0"),
        t1=_optional_json_int(entry.get("t1"), where + ".t1"),
        t_end_exclusive=_optional_json_int(
            entry.get("t_end_exclusive"), where + ".t_end_exclusive"
        ),
        line_ranges=_schema_3_line_ranges(entry.get("line_ranges"), where + ".line_ranges"),
    )


class _SchemaThreeArchive:
    """The schema-3 generation, opened lazily and read a line range at a time.

    What each question reads
    ------------------------
    Only the **spine** stream. Every question this file asks about the presentation timeline --
    which teams, which agents, which phases, which rollups, which project overviews, what a
    reference resolves to, how much summary text exists -- is answered from
    ``data/timeline-v3/spine/<team>.jsonl.gz``, and from one *line range* inside it rather than
    from the whole shard: the bootstrap publishes each kind's ``[first, count]``, so "the
    agents of one team" is a :meth:`_ChunkedJsonlReader.read_lines` call that inflates only the
    members those lines fall in. The **timeline** stream -- events, edges and full phases, 93%
    of schema 3's bytes -- is opened by none of them, and the **bins** stream only by
    :meth:`activity_bins`.

    That is not an oversight, it is the measurement: schema 2 answers the same questions from a
    ``global`` object and a ``phase_index`` object that together cost 25,692,901 bytes to open
    on the measured archive, unconditionally, before the first question is asked, because both
    are single JSON documents that must be parsed whole. The same questions against schema 3
    cost the 168,703-byte bootstrap plus the members the answer actually lives in. (89,298 of
    those bytes were the bootstrap before the search streams; the other 79,405 are their 96
    catalogue entries, and none of it is Bloom data -- schema 2 inlined 4.5 MB of that into the
    file every command reads.)

    Completeness, and why it is checked this way
    --------------------------------------------
    :meth:`open` returns ``None`` and a reason rather than raising, because reading a *partial*
    schema-3 generation as if it were whole is worse than not reading it at all: a listing that
    silently omits a team is indistinguishable from a team that did no work. The rule is:

    1. ``data/timeline-v3.json`` is a regular file (not a symlink), parses, and declares
       ``schema_version == 3`` and ``kind == "timeline-v3-bootstrap"``.
    2. Its ``codec`` block names a container, timestamp key, record-kind key and index suffix
       this reader implements. A future writer that changes any of them gets a fallback, not a
       misreading.
    3. It names all three streams, and every shard entry in them resolves to a path inside
       ``data/timeline-v3/``, is a regular file, is **exactly** ``c_bytes`` long, and has a
       sidecar beside it that is also a regular file.
    4. The set of teams named by spine shards equals the set of team slugs the bootstrap
       inlines. Every team gets a spine shard from the writer -- it always emits at least the
       ``team`` record -- so an inequality here is a generation that lost a shard.
    5. The converse of 3: every shard-shaped file **on disk** under ``data/timeline-v3/`` is
       named by the catalogue. Nothing published there is unaccounted for.

    Rules 1-5 cost one parse of the bootstrap -- 168,703 bytes -- one walk of a machine-owned
    directory tree, and two ``stat`` calls per shard: 181 shards on the measured archive once it
    carries the search streams, so 362 calls, against 85 shards and 170 calls before them. That
    is why they can run before every query rather than behind a flag.

    They rest on the writer's publication order, which is load-bearing and stated in
    :mod:`wrkviz.timeline_v3`: every shard and sidecar is written before the
    bootstrap that names them, and the bootstrap is replaced atomically. So the bootstrap
    existing implies the shards existed when it was written, and rules 3 and 4 catch the cases
    that fact does not cover -- a rebuild interrupted after rewriting a shard and before
    rewriting the bootstrap, a copy that stopped halfway, a shard deleted since.

    **Rule 5 is there because rules 1-4 cannot see a shard the bootstrap does not mention, and
    that is the direction of partial publication that loses a team.** Publication is
    shards-then-bootstrap with no atomicity across the set, so a build that adds a team and dies
    between the last spine shard and the bootstrap leaves the *previous* generation's bootstrap
    in place: every file it names is present at its declared length, rule 4 compares two sets
    that both come out of that same bootstrap, and the new team's shards sit on disk unread. The
    reader would answer from the older catalogue and omit a team the archive plainly has -- and
    `data/timeline-v2.json`, written earlier in the same build, would list it, so the website and
    ``./timeline`` would disagree about how many teams exist. Rule 5 turns that into a fallback
    to schema 2, which is the generation that does have the team.

    Only *shard-shaped* names count -- ``*.jsonl.gz`` and ``*.jsonl.gz.index.jsonl``, the two
    file shapes the writer emits. A stray note or an editor's scratch file under the root is
    ignored rather than costing the archive its fast read path, and a symlink is skipped for the
    same reason it is skipped everywhere else here: the bootstrap decides what this process
    opens, not the tree. The writer removes its own strays (see
    :func:`wrkviz.timeline_v3.write_timeline_v3`), so on an archive built by a
    completed run rule 5 has nothing to find; when it does find something, a rebuild is the
    remedy and `gc` says so rather than sweeping it.

    **The honest limit is a rewrite that preserves length.** The bootstrap also publishes
    ``c_sha256`` and ``u_sha256`` per shard, and neither is checked here, because checking them
    means reading all 38,288,394 bytes to answer a question that costs 300 KB -- which is the
    entire property schema 3 exists to provide. The three cheap agreement checks
    :class:`_ChunkedJsonlReader` already makes on every shard it opens (the sidecar names this
    file, the sidecar's ``c_size`` equals the file's length, and each member inflates to exactly
    the length the sidecar claims) narrow the gap further, and turn a stale sidecar into an
    error at the exact member rather than a confident wrong answer. What remains uncaught is a
    same-length substitution, which is the same limit the transcript projections' size check
    has, and `test_query_read_paths.py` pins it there rather than letting a docstring overstate
    it.

    **The cross-generation check is deferred, not skipped.** Whether schema 3 and schema 2
    describe the same source generation is only answerable by reading schema 2's bootstrap --
    5,702,530 bytes on the measured archive, sixty-five times the size of schema 3's, and
    exactly the file schema 3 exists to stop opening. So it is checked at the one place the two
    generations are used *together*: an archive whose schema 3 predates the ``search`` streams
    answers a transcript search out of schema 2's corpus while the phases and agents around it
    come from schema 3, so :meth:`TimelineQuery._search_bootstrap` refuses a ``source_digest``
    mismatch at the moment it opens that corpus -- where the schema-2 bootstrap is being read
    anyway and the comparison is free. On an archive whose schema 3 *does* carry the corpus there
    is nothing left to check: both halves come from this bootstrap.

    **What the search streams add.** Three optional streams -- ``search``, ``search_bloom`` and
    ``search_links`` -- carry the transcript search corpus when the build published one. They are
    the only streams this class reads besides the spine, and they are read only by a search: see
    :meth:`search_shards`, :meth:`search_blooms` and :meth:`search_links`, and
    `timeline_v3`'s module docstring for why they are three rather than one.
    """

    def __init__(
        self,
        root: Path,
        bootstrap: dict[str, JsonValue],
        bootstrap_bytes: int,
        shards: Sequence[_ShardEntry],
    ) -> None:
        self.root = root
        self.bootstrap = bootstrap
        self._bootstrap_bytes = bootstrap_bytes
        self._spine: dict[str, _ShardEntry] = {
            entry.team: entry
            for entry in shards
            if entry.stream == "spine" and entry.team is not None
        }
        self._bins: tuple[_ShardEntry, ...] = tuple(
            entry for entry in shards if entry.stream == "bins"
        )
        self._search: tuple[_ShardEntry, ...] = tuple(
            entry
            for entry in shards
            if entry.stream == "search" and entry.team is not None and entry.day is not None
        )
        self._search_bloom: dict[str, _ShardEntry] = {
            entry.team: entry
            for entry in shards
            if entry.stream == "search_bloom" and entry.team is not None
        }
        #: Which ``search`` shard paths each team owns, so a prefilter record can be checked
        #: against the shard it claims to describe rather than merely against the shard it was
        #: read out of. See :meth:`search_blooms` for what a missing check would cost.
        self._search_paths_by_team: dict[str, frozenset[str]] = {
            team: frozenset(
                entry.path for entry in self._search if entry.team == team
            )
            for team in {entry.team for entry in self._search if entry.team is not None}
        }
        self._search_links: dict[str, _ShardEntry] = {
            entry.team: entry
            for entry in shards
            if entry.stream == "search_links" and entry.team is not None
        }
        self.teams: tuple[str, ...] = tuple(sorted(self._spine))
        self._readers: dict[str, _ChunkedJsonlReader] = {}
        self._groups: dict[tuple[str, str], tuple[dict[str, JsonValue], ...]] = {}
        self._refs: dict[tuple[str, str], dict[str, dict[str, JsonValue]]] = {}

    @classmethod
    def open(cls, root: Path) -> tuple["_SchemaThreeArchive | None", str]:
        """Accept a complete schema-3 generation, or say in one sentence why not."""

        try:
            return cls._open(root), ""
        except SchemaThreeDeclined as error:
            return None, str(error)
        except (OSError, ValueError) as error:
            # A malformed bootstrap is a declined generation, not a failed query: the archive
            # still has schema 2 and schema 1, and refusing to answer at all would make schema
            # 3 a liability rather than an optimisation.
            return None, f"schema-3 bootstrap is unreadable: {error}"

    @classmethod
    def _open(cls, root: Path) -> "_SchemaThreeArchive":
        path = root / SCHEMA_3_BOOTSTRAP_PATH
        if path.is_symlink() or not path.is_file():
            raise SchemaThreeDeclined("no schema-3 bootstrap")
        encoded = path.read_bytes()
        bootstrap = as_object(
            _narrow_json(json.loads(encoded.decode("utf-8")), str(path)), str(path)
        )
        version = bootstrap.get("schema_version")
        if version != _SCHEMA_3_VERSION or bootstrap.get("kind") != _SCHEMA_3_KIND:
            raise SchemaThreeDeclined(
                f"{SCHEMA_3_BOOTSTRAP_PATH} is not a schema-3 bootstrap"
            )
        cls._check_codec(bootstrap)
        shards = cls._catalog(root, bootstrap)
        declared = cls._declared_team_slugs(bootstrap)
        published = {entry.team for entry in shards if entry.stream == "spine"}
        if declared != published:
            missing = sorted(slug for slug in declared if slug not in published)
            extra = sorted(slug for slug in published if slug is not None and slug not in declared)
            raise SchemaThreeDeclined(
                "schema-3 spine shards do not cover the published teams "
                f"(missing {missing}, unexpected {extra})"
            )
        cls._check_search_corpus(shards)
        cls._check_nothing_unnamed(root, shards)
        return cls(root, bootstrap, len(encoded), shards)

    @staticmethod
    def _check_search_corpus(shards: Sequence[_ShardEntry]) -> None:
        """Every team with a ``search`` shard has a prefilter shard and a linkage shard.

        The stream-level all-or-nothing rule in :meth:`_catalog` says the *format* is whole; this
        says the *corpus* is. They are different failures: a build that died between two teams
        leaves all three sections present and one team's relationships missing, and a search over
        that team would then report every linked response as unlinked -- a wrong answer that
        nothing downstream can detect, because "this prompt had no replies" is a perfectly
        ordinary result.
        """

        for entry in shards:
            if entry.stream not in _SCHEMA_3_SEARCH_STREAMS:
                continue
            # Addressed by team, and the ``search`` stream by day as well. Checked rather than
            # assumed because an entry missing either would be dropped by the per-stream tables
            # below and its records would simply never be searched -- the corpus would be short
            # by a shard and nothing would say so.
            if entry.team is None or (entry.stream == "search" and entry.day is None):
                raise SchemaThreeDeclined(
                    f"transcript search shard {entry.path} does not name the team and day it "
                    "is addressed by"
                )
        teams = {entry.team for entry in shards if entry.stream == "search"}
        if not teams:
            return
        for stream in ("search_bloom", "search_links"):
            published = {entry.team for entry in shards if entry.stream == stream}
            missing = sorted(team for team in teams - published if team is not None)
            if missing:
                raise SchemaThreeDeclined(
                    f"the transcript search corpus has no {stream} shard for {missing}"
                )

    @staticmethod
    def _check_codec(bootstrap: dict[str, JsonValue]) -> None:
        codec = as_object(bootstrap.get("codec"), "timeline-v3.codec")
        expected: tuple[tuple[str, str], ...] = (
            ("container", _SCHEMA_3_CONTAINER),
            ("timestamp_key", _SCHEMA_3_TIMESTAMP_KEY),
            ("record_kind_key", _SCHEMA_3_RECORD_KIND_KEY),
            ("index_suffix", _CHUNK_INDEX_SUFFIX),
        )
        for key, value in expected:
            found = codec.get(key)
            if found != value:
                raise SchemaThreeDeclined(
                    f"timeline-v3.codec.{key} is {found!r}; this reader implements {value!r}"
                )

    @staticmethod
    def _declared_team_slugs(bootstrap: dict[str, JsonValue]) -> set[str]:
        slugs: set[str] = set()
        for index, raw in enumerate(as_array(bootstrap.get("teams"), "timeline-v3.teams")):
            team = as_object(raw, f"timeline-v3.teams[{index}]")
            slugs.add(as_string(team.get("slug"), f"timeline-v3.teams[{index}].slug"))
        if not slugs:
            raise SchemaThreeDeclined("timeline-v3.teams: must name at least one team")
        return slugs

    @classmethod
    def _catalog(
        cls, root: Path, bootstrap: dict[str, JsonValue]
    ) -> tuple[_ShardEntry, ...]:
        streams = as_object(bootstrap.get("streams"), "timeline-v3.streams")
        missing = [name for name in _SCHEMA_3_STREAMS if name not in streams]
        if missing:
            raise SchemaThreeDeclined(f"timeline-v3.streams is missing {missing}")
        present = [name for name in _SCHEMA_3_SEARCH_STREAMS if name in streams]
        if present and len(present) != len(_SCHEMA_3_SEARCH_STREAMS):
            raise SchemaThreeDeclined(
                "timeline-v3.streams carries part of the transcript search corpus "
                f"({present}) and not the rest; a corpus without its prefilter or its "
                "relationship sidecar answers searches wrongly rather than slowly"
            )
        shards: list[_ShardEntry] = []
        seen: set[str] = set()
        for stream in (*_SCHEMA_3_STREAMS, *present):
            where = f"timeline-v3.streams.{stream}"
            section = as_object(streams[stream], where)
            for index, raw in enumerate(as_array(section.get("shards"), where + ".shards")):
                entry = _schema_3_shard(raw, stream, f"{where}.shards[{index}]")
                if entry.path in seen:
                    raise SchemaThreeDeclined(f"duplicate schema-3 shard {entry.path!r}")
                seen.add(entry.path)
                cls._check_present(root, entry)
                shards.append(entry)
        return tuple(shards)

    @staticmethod
    def _check_present(root: Path, entry: _ShardEntry) -> None:
        for relative, label in ((entry.path, "shard"), (entry.index_path, "sidecar")):
            if not relative.startswith(_SCHEMA_3_ROOT + "/"):
                raise SchemaThreeDeclined(
                    f"schema-3 {label} path is outside {_SCHEMA_3_ROOT}/: {relative!r}"
                )
            path = _schema_3_file(root, relative)
            if path is None:
                raise SchemaThreeDeclined(f"schema-3 {label} is missing: {relative}")
        size = _schema_3_size(root, entry.path)
        if size != entry.c_bytes:
            raise SchemaThreeDeclined(
                f"schema-3 shard {entry.path} is {size} bytes; the bootstrap says "
                f"{entry.c_bytes} -- the generation is partly published"
            )

    @staticmethod
    def _check_nothing_unnamed(root: Path, shards: Sequence[_ShardEntry]) -> None:
        """Rule 5: no shard-shaped file under the root is missing from the catalogue.

        Run *after* :meth:`_check_present`, so that damage visible from the catalogue keeps its
        own more specific sentence and this one is left to report the case only the tree can
        see. The listing is capped at three names because the interesting fact is the direction
        of the disagreement, not the inventory; the whole inventory is what `gc` prints.
        """

        named: set[str] = set()
        for entry in shards:
            named.add(entry.path)
            named.add(entry.index_path)
        stray = sorted(relative for relative in _schema_3_tree(root) if relative not in named)
        if not stray:
            return
        shown = ", ".join(stray[:3]) + (", ..." if len(stray) > 3 else "")
        raise SchemaThreeDeclined(
            f"{len(stray)} schema-3 file(s) on disk are named by no entry in "
            f"{SCHEMA_3_BOOTSTRAP_PATH} ({shown}) -- a build published shards and did not reach "
            "its bootstrap, so this catalogue is older than the tree it describes"
        )

    # -- reading -------------------------------------------------------------------------

    @property
    def bytes_read(self) -> int:
        """Bootstrap, sidecars and shard members physically read so far.

        Recomputed from the open readers rather than accumulated, so it cannot double-count a
        member a memoised group returned without reading.
        """

        return self._bootstrap_bytes + sum(
            reader.data_bytes_read + reader.index_bytes_read
            for reader in self._readers.values()
        )

    @property
    def opened_shards(self) -> tuple[str, ...]:
        """Which shards have been opened, so a test can assert on the set and not only the size.

        A byte count says an answer was cheap; this says *which* files it came out of, which is
        the claim that actually matters -- that a listing never opens the timeline stream, and
        that a one-team question never opens another team's spine. A count can be small for the
        wrong reason; a path set cannot.
        """

        return tuple(sorted(self._readers))

    def header(self) -> dict[str, JsonValue]:
        """The schema-1-shaped top of the timeline: everything the bootstrap inlines.

        ``schema_version`` is reported as 1 for the same reason
        :meth:`TimelineQuery._load_schema_2_timeline` reports it as 1 -- the value describes the
        *shape the caller sees*, which is schema 1's, not the generation it was assembled from.
        """

        header: dict[str, JsonValue] = {"schema_version": TIMELINE_SCHEMA_VERSION}
        for field in (
            "generated_at",
            "source_digest",
            "display_timezone",
            "display_timezone_source",
            "range",
            "stats",
            "artifact_catalog_path",
            "glossary_path",
            "teams",
        ):
            if field in self.bootstrap:
                header[field] = self.bootstrap[field]
        return header

    def _reader(self, relative: str, *, cache_members: bool = False) -> _ChunkedJsonlReader:
        reader = self._readers.get(relative)
        if reader is None:
            path = _schema_3_file(self.root, relative)
            if path is None:
                raise ArchiveReadError(f"schema-3 shard vanished: {relative}")
            reader = _ChunkedJsonlReader(path, cache_members=cache_members)
            self._readers[relative] = reader
        return reader

    def teams_in_scope(self, teams: Sequence[str]) -> tuple[str, ...]:
        """The published teams a filter selects, in shard order.

        An unknown slug in the filter selects nothing rather than raising: that is what the
        in-memory paths do -- ``_selected_team`` simply never matches -- and a seeking read must
        not turn a filter that used to return an empty list into an error.
        """

        if not teams:
            return self.teams
        wanted = frozenset(teams)
        return tuple(team for team in self.teams if team in wanted)

    def spine(self, kind: str, teams: Sequence[str] = ()) -> list[dict[str, JsonValue]]:
        """Every record of one spine kind, for the selected teams and no others."""

        records: list[dict[str, JsonValue]] = []
        for team in self.teams_in_scope(teams):
            records.extend(self._group(kind, team))
        return records

    def _group(self, kind: str, team: str) -> tuple[dict[str, JsonValue], ...]:
        """One kind's line range in one team's spine, read once and remembered.

        Memoised because the question is asked repeatedly for the same team -- ``list teams``
        derives each team's range from its agents, and every transcript-search record is
        validated against its agent -- and re-reading a member per question would make the
        counter this class is judged by report the same bytes many times over, which would be
        an honest count of a dishonest read.
        """

        memo = self._groups.get((kind, team))
        if memo is not None:
            return memo
        entry = self._spine.get(team)
        span = None if entry is None else entry.line_ranges.get(kind)
        if entry is None or span is None:
            self._groups[(kind, team)] = ()
            return ()
        first, count = span
        # Members are cached for a spine shard and for nothing else. A team's agents, phase
        # cards, structural edges and rollups are four line ranges inside the same member, and
        # `summary_stats` asks for three of them in a row -- without the cache that member is
        # inflated three times, and the archive-wide `stats` cost measured 5,803,943 bytes
        # against 2,257,791 with it. The bound is the spine, 2,088,865 compressed bytes across
        # twelve teams, and only the members actually touched; the timeline stream, which is 93%
        # of schema 3 and the one a cache could not hold, is never opened by this class.
        reader = self._reader(entry.path, cache_members=True)
        records = tuple(
            self._unwrap(record, kind, team, entry.path)
            for record in reader.read_lines(first, first + count)
        )
        self._groups[(kind, team)] = records
        return records

    @staticmethod
    def _unwrap(
        record: dict[str, JsonValue], kind: str, team: str, where: str
    ) -> dict[str, JsonValue]:
        """Turn a schema-3 line back into the schema-1 record the rest of this file expects.

        The envelope key is removed rather than passed through, because `show` returns the
        record as it found it and a ``record_kind`` leaking into the output would make the
        answer depend on which generation served it.

        ``team`` is stamped when the record does not carry it. The single-team render does not
        put a ``team`` field on its records -- there is only one, and schema 1 does not carry it
        -- and the writer's `_team_of` falls back to the sole team for exactly that case. The
        shard *is* the team, so this is the same fallback read from the other side, and it is
        strictly better informed than the writer's: it works for a multi-team archive too. A
        record that names a *different* team than the shard it sits in is corruption and says so.
        """

        found = record.pop(_SCHEMA_3_RECORD_KIND_KEY, None)
        if found != kind:
            raise ArchiveReadError(
                f"{where}: expected a {kind!r} record, found {found!r}"
            )
        declared = record.get("team")
        if declared is None:
            record["team"] = team
        elif declared != team:
            raise ArchiveReadError(
                f"{where}: record names team {declared!r} in {team!r}'s spine shard"
            )
        return record

    def bounds(self, team: str) -> dict[str, tuple[int, int]]:
        """The zoom bounds published for one team, keyed by stable reference."""

        table: dict[str, tuple[int, int]] = {}
        for record in self._group(_SCHEMA_3_BOUNDS_KIND, team):
            where = f"{team} activity bounds"
            reference = as_string(record.get("ref"), where + ".ref")
            table[reference] = (
                as_int(record.get("activity_start_ms"), where + ".activity_start_ms"),
                as_int(record.get("activity_end_ms"), where + ".activity_end_ms"),
            )
        return table

    def refs(
        self, kind: str, team: str, reference_of: Callable[[dict[str, JsonValue]], str]
    ) -> dict[str, dict[str, JsonValue]]:
        """One team's records of one reference kind, indexed by reference and zoom-bounded.

        The bounds are merged here and nowhere else. `show` is the only surface that returns a
        record as it found it, so it is the only one whose answer changes if they are absent --
        and it asks about one reference in one team, so merging costs that team's bounds range
        and not the archive's.
        """

        memo = self._refs.get((kind, team))
        if memo is not None:
            return memo
        table: dict[str, dict[str, JsonValue]] = {}
        bounds = self.bounds(team)
        for record in self._group(_SCHEMA_3_SPINE_KIND_FOR[kind], team):
            reference = reference_of(record)
            if reference in table:
                raise ValueError(f"duplicate stable reference {reference!r}")
            merged = dict(record)
            pair = bounds.get(reference)
            if pair is not None:
                merged["activity_start_ms"], merged["activity_end_ms"] = pair
            table[reference] = merged
        self._refs[(kind, team)] = table
        return table

    def activity_bins(
        self, start_ms: int | None, end_ms: int | None
    ) -> list[dict[str, JsonValue]]:
        """The pre-aggregated activity bins overlapping a half-open window.

        The one time-addressed read in this class, and the reason
        :meth:`_ChunkedJsonlReader.read_range` exists on the reader rather than only in its
        tests. No command-line surface consumes the bins today -- schema 2 validated them at
        open and no query has ever asked for one -- but they are the collection a windowed
        reader can actually seek into, and publishing the door here is what lets a caller
        (an overview chart, a future ``bins`` listing) get them without loading the archive.
        """

        records: list[dict[str, JsonValue]] = []
        for entry in self._bins:
            reader = self._reader(entry.path)
            for record in reader.read_range(start_ms, end_ms):
                record.pop(_SCHEMA_3_RECORD_KIND_KEY, None)
                # Both envelope keys come off, not just the classifying one. A bin's schema-1
                # record has no ``at_ms``; the writer adds it as the sort key, and leaving it
                # on would make a schema-3 bin a different record from a schema-2 bin -- which
                # is the one thing this read path is not allowed to be.
                record.pop(_SCHEMA_3_TIMESTAMP_KEY, None)
                records.append(record)
        return records

    # -- the transcript search corpus --------------------------------------------------------

    @property
    def has_search_corpus(self) -> bool:
        """Whether this generation carries the corpus, or search must fall back to schema 2."""

        return bool(self._search)

    def search_shards(self) -> tuple[_ShardEntry, ...]:
        """Every ``search`` shard, in catalogue order: ``(team, UTC day)``, sorted."""

        return self._search

    def search_blooms(self, teams: Sequence[str]) -> dict[str, JsonValue]:
        """The prefilter for each selected team's shards, keyed by the shard's own path.

        Keyed by path rather than by ``(team, day)`` because that is the join a caller actually
        makes -- it holds a shard entry and wants to know whether to open it -- and because the
        path is the one identifier the writer guarantees unique across the stream.

        Read only when a query has a term a trigram filter can act on; a two-byte query never
        calls this at all. That is the whole reason the filters are a stream instead of bootstrap
        fields: schema 2 parsed 4,527,592 base64 characters out of its bootstrap before every
        command, search or not, and then skipped every one of them for exactly this query.

        **The named path is checked against the naming team, not merely parsed.** Keying by path
        is what makes one team's shard reachable from another team's file, and that is the one
        way this stream can be worse than schema 2's, where each filter is inlined on the very
        catalogue entry it belongs to and cannot be addressed anywhere else. If ``A``'s prefilter
        shard carried a record whose ``shard`` named ``B``'s day, the loop below would overwrite
        ``B``'s own filter with ``A``'s -- and a Bloom filter's only wrong answer is a *false
        miss*, so `_bloom_might_match` would report a definite miss and the day would be skipped
        whole. Every record in it would then be silently absent from the result, with no error
        and no diagnostic: the exact shape of wrong answer this reader refuses elsewhere.

        The writer derives the path from the bucket it is filtering, so nothing produces this
        today. It is checked anyway because :meth:`_check_completeness` deliberately does not
        verify ``c_sha256``/``u_sha256`` -- see this class's docstring for why -- and a
        length-preserving corruption is precisely what that leaves uncaught. The check costs a
        set membership against a catalogue this reader has already validated.
        """

        blooms: dict[str, JsonValue] = {}
        for team in self.teams_in_scope(teams):
            entry = self._search_bloom.get(team)
            if entry is None:
                continue
            owned = self._search_paths_by_team.get(team, frozenset())
            reader = self._reader(entry.path)
            for record in reader.iter_records():
                where = f"{entry.path} bloom"
                self._unwrap(record, _SCHEMA_3_BLOOM_KIND, team, where)
                shard = as_string(record.get("shard"), where + ".shard")
                if shard not in owned:
                    raise ArchiveReadError(
                        f"{where}: prefilter names {shard!r}, which is not a search shard of "
                        f"team {team!r}"
                    )
                blooms[shard] = record.get("bloom")
        return blooms

    def iter_search_shard(self, entry: _ShardEntry) -> Iterator[dict[str, JsonValue]]:
        """Yield one ``search`` shard's records, one member at a time.

        The shard is read whole rather than through :meth:`_ChunkedJsonlReader.read_range`, even
        though it is time-addressed and a window filter is usually in play, because a shard *is* a
        UTC day: a caller that has already decided to open it wants nearly all of it, and the
        record-level window filter the caller applies anyway is exact where a member's ``t0``/
        ``t1`` is only conservative. The seek stays available for a caller that wants a slice of a
        day; nothing asks for one today.
        """

        team = entry.team
        if team is None:
            raise ArchiveReadError(f"schema-3 search shard names no team: {entry.path}")
        reader = self._reader(entry.path)
        # The catalogue's count against the sidecar's, before a single record is yielded. Schema 2
        # makes the same comparison in the same place and for the same reason: a caller that has
        # already begun consuming records cannot un-yield them when the shard turns out to be
        # short, and a short shard is what a half-written generation looks like.
        if reader.record_count != entry.records:
            raise ValueError(
                f"transcript search shard count mismatch: {entry.path} holds "
                f"{reader.record_count} records, the bootstrap says {entry.records}"
            )
        for record in reader.iter_records():
            yield self._unwrap(record, _SCHEMA_3_SEARCH_KIND, team, entry.path)

    def search_links(
        self, team: str, *, prompts: bool, responses: bool
    ) -> tuple[list[dict[str, JsonValue]], list[dict[str, JsonValue]]]:
        """One team's relationship sidecar, reading only the halves the caller asked for.

        The two line ranges are what make that possible, and they pay off in both directions: a
        search whose matches are all responses needs prompt excerpts and no response edges, and
        one whose matches are all prompts needs the edges to count replies and no excerpts. Asking
        for neither reads nothing, which is what a search with no linked candidates does.
        """

        entry = self._search_links.get(team)
        if entry is None:
            return [], []
        wanted = {_SCHEMA_3_PROMPT_LINK_KIND: prompts, _SCHEMA_3_RESPONSE_LINK_KIND: responses}
        found: dict[str, list[dict[str, JsonValue]]] = {
            _SCHEMA_3_PROMPT_LINK_KIND: [],
            _SCHEMA_3_RESPONSE_LINK_KIND: [],
        }
        for kind, asked in wanted.items():
            span = entry.line_ranges.get(kind)
            if not asked or span is None:
                continue
            first, count = span
            reader = self._reader(entry.path, cache_members=True)
            found[kind] = [
                self._unwrap(record, kind, team, entry.path)
                for record in reader.read_lines(first, first + count)
            ]
        return found[_SCHEMA_3_PROMPT_LINK_KIND], found[_SCHEMA_3_RESPONSE_LINK_KIND]


def _schema_3_file(root: Path, relative: str) -> Path | None:
    """Resolve an archive-relative schema-3 path, or ``None`` if it is missing or unsafe.

    Symlinks are refused in both directions -- a symlinked component and a symlinked leaf --
    because the bootstrap is data and the shard paths in it decide what this process opens.
    """

    pure = PurePosixPath(relative)
    if pure.is_absolute() or any(part in {"", ".", ".."} for part in pure.parts):
        return None
    cursor = root
    for part in pure.parts:
        cursor = cursor / part
        if cursor.is_symlink():
            return None
    return cursor if cursor.is_file() else None


def _schema_3_size(root: Path, relative: str) -> int:
    path = _schema_3_file(root, relative)
    return -1 if path is None else path.stat().st_size


#: The two file shapes the schema-3 writer publishes. Restated here rather than imported for the
#: reason every other schema-3 constant in this file is restated: this module ships as a
#: self-contained launcher inside the archive and imports nothing from the package. The writer
#: holds the same predicate under the same name, and the pair is load-bearing in one direction --
#: a reader that recognised a shape the writer does not remove would decline a generation that
#: rebuilding cannot repair -- so `test_timeline_v3.py` asserts the two agree.
_SCHEMA_3_SHARD_SUFFIX = ".jsonl.gz"
_SCHEMA_3_SIDECAR_SUFFIX = _SCHEMA_3_SHARD_SUFFIX + _CHUNK_INDEX_SUFFIX


def is_schema_3_shard_name(name: str) -> bool:
    """Whether *name* is a file the schema-3 writer would have produced."""

    return name.endswith(_SCHEMA_3_SHARD_SUFFIX) or name.endswith(_SCHEMA_3_SIDECAR_SUFFIX)


def _schema_3_tree(root: Path) -> frozenset[str]:
    """Every shard-shaped regular file under ``data/timeline-v3/``, archive-relative.

    Symlinks -- to a directory or to a file -- are neither descended nor reported. That matches
    :func:`_schema_3_file`, which refuses to *open* through one, and it keeps this set to files
    the writer could have written and could remove again.

    Written on :func:`os.scandir`, and on plain strings, because this runs before every query.
    Both choices were measured on a 720-file tree the size of the measured archive's schema-3
    generation, and both are larger than they look. ``scandir`` carries the entry type in the
    directory read itself, so the walk costs one ``getdents`` per directory and no ``stat`` at
    all, where the ``os.walk`` form has to ask ``Path.is_symlink`` and ``Path.is_file`` per name.
    And building the archive-relative name by slicing rather than by
    ``Path(...).relative_to(root).as_posix()`` is worth **23 ms of the 24**: pathlib dominated
    the whole function. Together, 27.3 ms became 0.6 ms. Twenty-seven milliseconds on the way to
    an answer whose entire point is that it costs 300 KB would have been the largest single item
    on the read path.
    """

    start = root / _SCHEMA_3_ROOT
    if start.is_symlink() or not start.is_dir():
        return frozenset()
    found: set[str] = set()
    trim = len(str(start))
    pending = [str(start)]
    while pending:
        try:
            with os.scandir(pending.pop()) as entries:
                for entry in entries:
                    if entry.is_symlink():
                        continue
                    if entry.is_dir(follow_symlinks=False):
                        pending.append(entry.path)
                    elif entry.is_file(follow_symlinks=False) and is_schema_3_shard_name(
                        entry.name
                    ):
                        found.add(
                            _SCHEMA_3_ROOT + entry.path[trim:].replace(os.sep, "/")
                        )
        except OSError:
            # A directory that vanished or cannot be read is not evidence of an unnamed shard,
            # and the catalogue check has already established that everything named is present.
            continue
    return frozenset(found)


class _SeekableJsonlText:
    """Binary-search an uncompressed, key-sorted JSONL projection in place.

    The archive's transcript projections are plain `.jsonl` -- that is their integrity
    contract, digested in `extracted/transcripts/manifest.json` -- and this reads slices of
    them without a sidecar, an index, or a second copy on disk. A sorted uncompressed file
    does not need one: the position of a key is recoverable by probing.

    Probing costs ``O(log bytes)`` reads of one record each rather than ``O(1)``, and that is
    the whole trade. Against the alternative -- publishing a chunked `.gz` twin of every
    projection -- it saves about 24 MB of duplicated bytes per rebuild on the measured
    archive and costs roughly twenty-seven extra small reads on a query that would otherwise
    have read one member. The duplication is the thing schema 3 was built to stop.
    """

    def __init__(self, path: Path, *, expected_bytes: int | None = None) -> None:
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"transcript export file is missing or unsafe: {path}")
        self.path = path
        self.bytes_read = 0
        self.size = path.stat().st_size
        # The manifest's byte count is checked at open even when the digest is not, because
        # it is a stat and it catches the failure the digest check was written for: a
        # generation that was interrupted, or a file appended to after the manifest was
        # written. What it cannot catch -- an equal-length rewrite -- is what
        # :meth:`stream_verified` and ``--verify`` are for.
        if expected_bytes is not None and expected_bytes != self.size:
            raise ValueError(
                f"transcript export generation is incomplete: {path.name} holds "
                f"{self.size} bytes, the manifest declares {expected_bytes}"
            )

    def stream_verified(self, expected_sha256: str | None) -> Iterator[bytes]:
        """Yield every line, reproducing the manifest digest as the bytes go past.

        This is the path a query that genuinely needs the whole file takes, and on it the
        archive's integrity contract is checked in full and for free -- the digest is folded
        over bytes already being read. The verification happens at the *end*, so a caller
        that abandons the iterator early gets no guarantee and no false one either.
        """

        digest = hashlib.sha256()
        window = _PROBE_BLOCK_BYTES
        with self.path.open("rb") as handle:
            remainder = b""
            while True:
                block = handle.read(window)
                if not block:
                    break
                # Ramped for the same reason `iter_from` is: a caller with `--limit 5`
                # abandons this iterator after the first few records, and reading a
                # scan-sized block to satisfy it would make the smallest possible request
                # cost a quarter of a megabyte. A caller who does read to the end reaches
                # full block size after six reads.
                window = min(window * 4, _SCAN_BLOCK_BYTES)
                self.bytes_read += len(block)
                digest.update(block)
                remainder += block
                lines = remainder.split(b"\n")
                remainder = lines.pop()
                yield from (line for line in lines if line)
            if remainder:
                yield remainder
        if expected_sha256 is not None and digest.hexdigest() != expected_sha256:
            raise ValueError(
                f"transcript export generation is incomplete: {self.path.name}"
            )

    def iter_from(self, offset: int) -> Iterator[tuple[int, bytes]]:
        """Yield ``(line start offset, line)`` from *offset*, which must be a line start.

        The read window ramps from a probe-sized block up to a scan-sized one. Both ends
        matter: a caller taking three records from the middle of a 105 MB file should not pay
        a quarter of a megabyte to get them, and a caller streaming the rest of the file
        should not pay a syscall every four kilobytes to do it.
        """

        window = _PROBE_BLOCK_BYTES
        with self.path.open("rb") as handle:
            handle.seek(offset)
            position = offset
            remainder = b""
            while True:
                block = handle.read(window)
                if not block:
                    break
                window = min(window * 4, _SCAN_BLOCK_BYTES)
                self.bytes_read += len(block)
                remainder += block
                while True:
                    cut = remainder.find(b"\n")
                    if cut < 0:
                        break
                    line = remainder[:cut]
                    remainder = remainder[cut + 1 :]
                    if line:
                        yield position, line
                    position += cut + 1
            if remainder:
                yield position, remainder

    def backwards(self, end: int | None = None) -> "_BackwardCursor":
        """Open a resumable backwards read ending at *end*.

        *end* must be a line start -- in practice the offset a :meth:`seek_first` returned --
        so that "the last twenty records before this instant" is one backwards read from
        there rather than a forward scan of everything preceding it.
        """

        return _BackwardCursor(self, end)

    def tail_lines(self, count: int, *, end: int | None = None) -> list[bytes]:
        """Return the last *count* non-empty lines before *end*, reading backwards."""

        if count <= 0:
            return []
        cursor = self.backwards(end)
        return cursor.extend_to(count)[-count:]

    def seek_first(self, key: str, target: int, where: str) -> int:
        """Return the offset of the first record whose integer *key* is at least *target*.

        A textbook offset bisect with one wrinkle worth naming: the search space is byte
        offsets, but only line starts are meaningful, so the probe reads forward from the
        midpoint to the next line boundary and evaluates *that* record. The predicate
        "the first record starting at or after this offset has key >= target" is monotone in
        the offset precisely because the file is sorted, which is why the answer is the
        smallest offset satisfying it and not merely some offset that does.
        """

        low = 0
        high = self.size
        while low < high:
            middle = (low + high) // 2
            probe = self._record_at_or_after(middle, key, where)
            if probe is None or probe[1] >= target:
                high = middle
            else:
                low = middle + 1
        aligned = self._record_at_or_after(low, key, where)
        return self.size if aligned is None else aligned[0]

    def _record_at_or_after(
        self, offset: int, key: str, where: str
    ) -> tuple[int, int] | None:
        """The ``(line start, key)`` of the first record starting at or after *offset*."""

        with self.path.open("rb") as handle:
            start = offset
            if offset > 0:
                handle.seek(offset - 1)
                probe = handle.read(1)
                self.bytes_read += len(probe)
                if probe != b"\n":
                    skipped = self._advance_past_newline(handle)
                    if skipped is None:
                        return None
                    start = skipped
            handle.seek(start)
            line = self._read_line(handle)
        if not line:
            return None
        record = _decode_record(line, f"{where}:{start}")
        value = record.get(key)
        if not isinstance(value, int) or isinstance(value, bool):
            raise ValueError(f"{where}:{start}: {key} is not an integer")
        return start, value

    def _advance_past_newline(self, handle: BinaryIO) -> int | None:
        position = handle.tell()
        window = _PROBE_BLOCK_BYTES
        while True:
            block = handle.read(window)
            if not block:
                return None
            window = min(window * 2, _SCAN_BLOCK_BYTES)
            self.bytes_read += len(block)
            cut = block.find(b"\n")
            if cut >= 0:
                return position + cut + 1
            position += len(block)

    def _read_line(self, handle: BinaryIO) -> bytes:
        collected = b""
        window = _PROBE_BLOCK_BYTES
        while True:
            block = handle.read(window)
            if not block:
                return collected
            window = min(window * 2, _SCAN_BLOCK_BYTES)
            self.bytes_read += len(block)
            cut = block.find(b"\n")
            if cut >= 0:
                return collected + block[:cut]
            collected += block


class _BackwardCursor:
    """A backwards read of a projection that widens without re-reading what it has.

    This exists because the obvious shape does not compose with filtering. "The last twenty
    records matching a team" cannot be answered by one backwards read of twenty lines, so the
    caller widens -- and if widening means calling ``tail_lines`` again with a bigger count,
    round *k* re-reads everything rounds 1..k-1 read. Measured on the 105,453,039-byte message
    projection, a ``--tail 20`` restricted to a team whose last message sits at index 35,578 of
    66,114 read 201,412,186 bytes: 1.9 times the file, to return twenty records. With the buffer
    kept, the same query reads 112,283,226 -- the message projection once and the prompt
    projection once, since the selection is resolved through both -- and neither byte twice.
    One pass is the only ceiling worth having on a method whose entire justification is that it
    does not read the file.

    It is still a full pass in that shape, and no buffer discipline can fix that: the accepted
    records end at index 35,578, so a backwards walk has to cross the 30,536 rejected ones to
    reach them. What was removed is the second and third crossing, not the first.

    The window still doubles per round, for the reason it always did: a selection that rejects
    most of what it sees would otherwise cost one syscall per rejected record. What changed is
    that the doubling now buys *additional* bytes rather than a larger re-read.
    """

    def __init__(self, reader: _SeekableJsonlText, end: int | None) -> None:
        self._reader = reader
        self._start = reader.size if end is None else max(min(end, reader.size), 0)
        self._buffer = b""
        self._window = _TAIL_BLOCK_BYTES

    @property
    def exhausted(self) -> bool:
        """Whether the window has reached offset zero and nothing earlier remains."""

        return self._start == 0

    def _widen(self) -> None:
        start = max(0, self._start - self._window)
        with self._reader.path.open("rb") as handle:
            handle.seek(start)
            block = handle.read(self._start - start)
        self._reader.bytes_read += len(block)
        self._buffer = block + self._buffer
        self._start = start
        self._window *= 2

    def lines(self) -> list[bytes]:
        """The complete non-empty lines in the buffer, in file order.

        The earliest element is dropped unless the window has reached offset zero: it may be
        the tail of a record that begins before the window, and nothing in the buffer can tell
        it apart from a whole one. Dropping it is conservative rather than lossy -- the next
        widen brings it back with its beginning attached.
        """

        collected = [line for line in self._buffer.split(b"\n") if line]
        if self._start > 0 and collected:
            collected.pop(0)
        return collected

    def extend_to(self, wanted: int) -> list[bytes]:
        """Widen until the buffer holds *wanted* complete lines, or the file runs out."""

        collected = self.lines()
        while len(collected) < wanted and not self.exhausted:
            self._widen()
            collected = self.lines()
        return collected


@dataclass(frozen=True)
class _ManagedFile:
    """One manifest-declared transcript projection and everything claimed about it."""

    name: str
    path: Path
    sha256: str
    declared_bytes: int | None


class TranscriptQuery:
    """Validated read-only access to the zero-model transcript projection.

    **What this class stopped doing, and why.** It used to SHA-256 all four managed files in
    its constructor -- 227,545,190 bytes on the measured archive -- and then materialize two
    of them with ``read_text().splitlines()``. Every query paid all of it: ``messages --range
    3978-3980`` read 339,828,416 bytes at 994,996,224 bytes of resident memory and returned
    nine records, and its cost was byte-identical to ``--range 1`` because the filtering
    happened after full materialization. Of what it hashed, 115,261,964 bytes -- the
    occurrence log and the system-input projection -- are consulted by no query at all.

    The same three questions now cost 3,054,373 bytes, 65,536 bytes and zero: the range read
    seeks to its span, ``--tail 20`` reads one backwards block, and constructing this object
    reads nothing whatsoever. `test_query_read_paths.py` asserts each of those as a bound on
    :attr:`bytes_read` rather than as a duration.

    Now nothing is read until a query asks for it, and what a query asks for is a slice.

    **The integrity contract, restated honestly**, because it did change and pretending
    otherwise would be worse than the change:

    * At open, every managed file is checked for existence, for not being a symlink, and --
      where the manifest declares one -- for its exact byte count. That is four ``stat``
      calls and it catches the failure the constructor's digest loop was written to catch: a
      generation interrupted midway, or a file appended to after the manifest was written.
      All four, not just the two a query consults: a truncated ``occurrences.jsonl`` says the
      generation is damaged whether or not this particular question would have read it, and
      an open that only checked what it intended to read would report a healthy archive while
      holding the evidence that it is not.
    * A query that reads a file *end to end* verifies its digest in full, folded over bytes
      it was reading anyway, so it costs nothing. ``stats`` and an unfiltered ``prompts``
      still fail closed exactly as before.
    * A query that reads a *slice* verifies the byte count and the structure of the records
      it returns, and does not reproduce the whole-file digest -- it never read the whole
      file. ``verify=True`` (``--verify`` on the command line) restores the complete check on
      demand, for the reader who wants the old guarantee at the old price.
    * A manifest written before the per-file ``bytes`` field existed declares no size, so
      there is nothing to check with a ``stat``; such a file gets the complete digest check on
      first use instead. Degrading to "no check" there would silently weaken exactly the
      oldest archives, which is the one direction this change must not go.

    **What a slice read does not promise.** An equal-length rewrite of bytes no query touched
    is invisible to a size check, and no amount of seeking will see it -- that is what
    ``--verify`` is for, and why the bullet above says which measurement each path made rather
    than claiming one guarantee for both.
    """

    #: The projections a shipped archive carries. `occurrences.jsonl` is deliberately absent:
    #: it is the exporter's monotonic baseline, read only by the next extraction, and the
    #: largest thing the exporter writes -- so it lives in the build store beside the rest of
    #: the rerun state rather than in the directory an operator ships. Its digest is still in
    #: the manifest; what is gone is the requirement that this reader find a file the archive
    #: was never meant to carry.
    _FILES = (
        "prompts.jsonl",
        "messages.jsonl",
        "system-inputs.jsonl",
    )

    #: The two projections any query actually consults. The third is declared, checked for
    #: presence, and never opened -- naming them here is what keeps that deliberate.
    _CONSULTED = ("prompts.jsonl", "messages.jsonl")

    def __init__(self, root: Path, *, verify: bool = False) -> None:
        self.root = root.resolve()
        self.transcript_root = self.root / "extracted" / "transcripts"
        manifest_path = self.transcript_root / "manifest.json"
        if manifest_path.is_symlink() or not manifest_path.is_file():
            raise ValueError(
                f"no extracted transcripts at {manifest_path}; run extract-transcripts"
            )
        manifest = as_object(read_json(manifest_path), str(manifest_path))
        schema = as_int(manifest.get("schema_version"), "transcript manifest schema")
        if schema != TRANSCRIPT_EXPORT_SCHEMA_VERSION:
            raise ValueError(
                f"unsupported transcript export schema version {schema}; "
                f"expected {TRANSCRIPT_EXPORT_SCHEMA_VERSION}"
            )
        files = as_object(manifest.get("files"), "transcript manifest files")
        self._declared: dict[str, _ManagedFile] = {}
        for name in self._FILES:
            entry = as_object(files.get(name), f"transcript manifest files.{name}")
            path = self.transcript_root / name
            if path.is_symlink() or not path.is_file():
                raise ValueError(f"transcript export file is missing or unsafe: {path}")
            declared_bytes = _optional_json_int(entry.get("bytes"), f"{name}.bytes")
            # The stat happens here, for every projection present, not in `_SeekableJsonlText`.
            # It used to happen only there, which meant it happened only for the two
            # projections a query opens -- so a truncated third projection was accepted in
            # silence by an open that had already been told how long the file should be. The
            # constructor's old digest loop caught that; a size check is the cheap half of
            # what it caught, and the half that costs a stat is the half worth keeping
            # unconditional.
            if declared_bytes is not None:
                size = path.stat().st_size
                if size != declared_bytes:
                    raise ValueError(
                        f"transcript export generation is incomplete: {name} holds "
                        f"{size} bytes, the manifest declares {declared_bytes}"
                    )
            self._declared[name] = _ManagedFile(
                name=name,
                path=path,
                sha256=as_string(entry.get("sha256"), f"{name}.sha256"),
                declared_bytes=declared_bytes,
            )
        self._readers: dict[str, _SeekableJsonlText] = {}
        self._verified: set[str] = set()
        # `verify` means "rely on nothing this archive merely promises". It re-digests every
        # managed projection end to end -- all of them, because "the old guarantee at the old
        # price" is not a guarantee if it silently drops the one this archive carries and no
        # query reads -- and it also widens the message read in
        # :meth:`list_messages` from a seek to a full scan, because those are the two places
        # a slice read substitutes an invariant for a measurement.
        self._exhaustive = verify
        if verify:
            for name in self._FILES:
                self._verify_whole(name)

    @property
    def bytes_read(self) -> int:
        """Bytes physically read from the transcript projections by this instance.

        Published so the cost of a query can be *asserted* rather than timed. A timing
        assertion is a flake on a loaded build host; a byte-count assertion is a proof, and
        it is the only way to keep a future refactor from quietly reintroducing the full
        materialization this class was rewritten to remove.
        """

        return sum(reader.bytes_read for reader in self._readers.values())

    def _managed(self, name: str) -> _ManagedFile:
        declared = self._declared.get(name)
        if declared is None:
            raise ValueError(f"{name} is not a managed transcript projection")
        return declared

    def _reader(self, name: str) -> _SeekableJsonlText:
        existing = self._readers.get(name)
        if existing is not None:
            return existing
        declared = self._managed(name)
        reader = _SeekableJsonlText(declared.path, expected_bytes=declared.declared_bytes)
        self._readers[name] = reader
        if declared.declared_bytes is None and name not in self._verified:
            # No declared length to check, so the only guard left is the digest. Pay for it
            # once, here, rather than answering from bytes nothing has vouched for.
            self._verify_whole(name)
        return reader

    def _verify_whole(self, name: str) -> None:
        declared = self._managed(name)
        reader = self._readers.get(name)
        if reader is None:
            reader = _SeekableJsonlText(
                declared.path, expected_bytes=declared.declared_bytes
            )
            self._readers[name] = reader
        for _line in reader.stream_verified(declared.sha256):
            pass
        self._verified.add(name)

    def _stream(self, name: str) -> Iterator[dict[str, JsonValue]]:
        """Yield every record in one projection, verifying its digest as the bytes go past."""

        declared = self._managed(name)
        reader = self._reader(name)
        expected = None if name in self._verified else declared.sha256
        for number, line in enumerate(reader.stream_verified(expected)):
            yield _decode_record(line, f"{declared.path}:{number + 1}")
        self._verified.add(name)

    @staticmethod
    def _selected(record: dict[str, JsonValue], filters: QueryFilters) -> bool:
        team = as_string(record.get("team_slug"), "transcript record.team_slug")
        if filters.teams and team not in filters.teams:
            return False
        if filters.window is None:
            return True
        at_ms = as_int(record.get("timestamp_ms"), "transcript record.timestamp_ms")
        return filters.window.contains(at_ms)

    @staticmethod
    def _prompt_class(record: dict[str, JsonValue]) -> str:
        author_kind = as_string(record.get("author_kind"), "prompt.author_kind")
        if author_kind in _HUMAN_AUTHOR_KINDS:
            return "human"
        if author_kind in _BOT_AUTHOR_KINDS:
            return "bot"
        return "unclassified"

    @classmethod
    def _selected_prompt_authorship(
        cls, record: dict[str, JsonValue], which: str
    ) -> bool:
        if which not in PROMPT_SELECTIONS:
            raise ValueError("prompt selection must be human, bot, or all")
        return which == "all" or cls._prompt_class(record) == which

    @staticmethod
    def _window_bounds(filters: QueryFilters) -> tuple[int | None, int | None]:
        window = filters.window
        return (None, None) if window is None else (window.start_ms, window.end_ms)

    def _iter_instant_span(
        self, name: str, start_ms: int | None, end_ms: int | None
    ) -> Iterator[dict[str, JsonValue]]:
        """Yield records with ``start_ms <= timestamp_ms``, stopping at *end_ms*.

        The sortedness the bisect depends on is re-checked across the span actually read.
        That is the honest amount to check -- it cannot vouch for bytes nobody looked at --
        and it is enough to turn "the projection stopped being sorted and the seek silently
        under-returned" into a refusal at the first record out of order.
        """

        reader = self._reader(name)
        where = str(self._managed(name).path)
        offset = (
            0 if start_ms is None else reader.seek_first("timestamp_ms", start_ms, where)
        )
        previous: int | None = None
        for position, line in reader.iter_from(offset):
            record = _decode_record(line, f"{where}:{position}")
            at_ms = as_int(record.get("timestamp_ms"), "transcript record.timestamp_ms")
            if previous is not None and at_ms < previous:
                raise ValueError(
                    f"{where}:{position}: records are not ordered by timestamp_ms, so a "
                    "seeked read would under-return; rebuild the transcript export"
                )
            previous = at_ms
            if end_ms is not None and at_ms >= end_ms:
                return
            yield record

    def _iter_prompts_by_ordinal(
        self, ordinal_range: OrdinalRange
    ) -> Iterator[dict[str, JsonValue]]:
        """Yield the prompts in an inclusive ordinal range, seeking straight to the first.

        ``ordinal`` is bisectable for the same reason ``timestamp_ms`` is: the exporter
        assigns it with ``enumerate(prompts, 1)`` over the already-sorted projection, so it
        is the line number plus one. Consecutiveness is asserted across the records returned
        rather than assumed, because the failure mode of a non-dense ordinal column is a
        window that quietly omits its middle.
        """

        reader = self._reader("prompts.jsonl")
        where = str(self._managed("prompts.jsonl").path)
        offset = reader.seek_first("ordinal", ordinal_range.first, where)
        expected: int | None = None
        for position, line in reader.iter_from(offset):
            record = _decode_record(line, f"{where}:{position}")
            ordinal = as_int(record.get("ordinal"), "prompt.ordinal")
            if ordinal > ordinal_range.last:
                return
            if expected is not None and ordinal != expected:
                raise ValueError(
                    f"{where}:{position}: prompt ordinals jump from {expected - 1} to "
                    f"{ordinal}; the projection is not the dense chronological index its "
                    "manifest declares"
                )
            expected = ordinal + 1
            yield record

    def _tail_matching(
        self,
        name: str,
        count: int,
        accept: "Callable[[dict[str, JsonValue]], bool]",
        *,
        end: int | None = None,
    ) -> list[dict[str, JsonValue]]:
        """Return the last *count* accepted records, reading backwards from *end*.

        The filter is applied *during* the backwards walk, so this returns *count* records
        that match rather than the matches among the last *count* records. The window widens
        by a factor of four when the records it caught were filtered away, rather than by one
        record at a time: a selection that rejects most of what it sees would otherwise cost
        one backwards read per rejected record.

        The widening happens on a :class:`_BackwardCursor` rather than by re-issuing a bigger
        ``tail_lines``, which is what keeps the total at one pass. Each round decodes only the
        lines the round actually added -- they are exactly the front of the buffer, because
        widening only prepends -- so neither the bytes nor the JSON is paid for twice.
        """

        reader = self._reader(name)
        where = str(self._managed(name).path)
        cursor = reader.backwards(end)
        matched: list[dict[str, JsonValue]] = []
        decoded = 0
        wanted = max(count, 1)
        while True:
            lines = cursor.extend_to(wanted)
            fresh = lines[: len(lines) - decoded]
            decoded = len(lines)
            matched = [
                dict(record)
                for record in (
                    _decode_record(line, f"{where}: tail record") for line in fresh
                )
                if accept(record)
            ] + matched
            if len(matched) >= count or cursor.exhausted:
                return matched[-count:]
            wanted = max(len(lines), wanted) * 4

    def _prompt_candidates(
        self, filters: QueryFilters, ordinal_range: OrdinalRange | None
    ) -> Iterator[dict[str, JsonValue]]:
        """Yield prompt records from the narrowest span that can hold the answer."""

        if ordinal_range is not None:
            yield from self._iter_prompts_by_ordinal(ordinal_range)
            return
        start_ms, end_ms = self._window_bounds(filters)
        if start_ms is not None:
            yield from self._iter_instant_span("prompts.jsonl", start_ms, end_ms)
            return
        # No physical narrowing is possible, so this is the end-to-end path -- and being the
        # end-to-end path, it is the one that verifies the manifest digest.
        for record in self._stream("prompts.jsonl"):
            if end_ms is not None and (
                as_int(record.get("timestamp_ms"), "transcript record.timestamp_ms")
                >= end_ms
            ):
                return
            yield record

    def list_prompts(
        self,
        filters: QueryFilters,
        ordinal_range: OrdinalRange | None,
        which: str = "human",
        *,
        limit: int | None = None,
        tail: int | None = None,
    ) -> list[dict[str, JsonValue]]:
        """Return verbatim authored prompt records in global timestamp order."""

        _check_page(limit, tail)

        def accept(record: dict[str, JsonValue]) -> bool:
            return self._selected_prompt_authorship(
                record, which
            ) and self._selected(record, filters)

        start_ms, end_ms = self._window_bounds(filters)
        if tail is not None and ordinal_range is None and start_ms is None:
            end_offset = (
                None
                if end_ms is None
                else self._reader("prompts.jsonl").seek_first(
                    "timestamp_ms", end_ms, str(self._managed("prompts.jsonl").path)
                )
            )
            return self._tail_matching("prompts.jsonl", tail, accept, end=end_offset)
        result: list[dict[str, JsonValue]] = []
        for record in self._prompt_candidates(filters, ordinal_range):
            if not accept(record):
                continue
            result.append(dict(record))
            if limit is not None and len(result) >= limit:
                break
        return result[-tail:] if tail is not None else result

    def list_messages(
        self,
        filters: QueryFilters,
        ordinal_range: OrdinalRange | None,
        which: str = "human",
        *,
        limit: int | None = None,
        tail: int | None = None,
    ) -> list[dict[str, JsonValue]]:
        """Return prompts plus mechanically associated coordinator responses.

        **Where the message read starts, and why that is sound.** A response is emitted only
        when its ``in_reply_to_prompt_id`` names a selected prompt, so the scan can begin at
        the earliest selected prompt's instant instead of at byte zero. That rests on one
        writer invariant: ``transcript_export._response_records`` links a response only to a
        prompt occurrence at or before the response's own instant, and both projections are
        sorted on that instant. The narrow gap is a logical prompt with several occurrences
        whose *representative* occurrence -- the earliest one carrying an authorship
        classification -- is later than the occurrence a response was actually linked to; on
        the measured archive that is 0 of 4,043 prompts, but it is not structurally
        impossible. ``verify=True`` therefore does not merely re-digest: it also abandons this
        lower bound and resolves linkage by full scan, so the reader who will not rely on an
        invariant the archive only promises has a way to say so.

        When a time window is set, the lower bound is the window's own start instead, which
        rests on nothing at all -- a message outside the window is dropped by ``_selected``
        regardless of what it replies to.

        **There is a lower bound and no upper one, and that asymmetry is structural.** The
        invariant above is one-sided: a response is linked to a prompt occurrence at or before
        its own instant, and *nothing* bounds how long after that instant it arrives. So the
        scan can start late and cannot stop early. Selecting the last few prompts of an
        archive therefore costs almost nothing -- the lower bound does all the work, and
        ``--range 3978-3980`` on the measured archive reads 3,054,373 bytes of a 105,453,039
        byte projection -- while selecting the *first* few costs the whole projection:
        ``--range 1-3`` reads all of it, because ``lower`` is the first prompt's instant and
        there is no instant after which a reply to prompt 1 becomes impossible.

        Three ways to bound the top were considered. Stopping at the last selected prompt's
        instant is wrong by construction, since that is precisely where the replies begin.
        Stopping once every selected prompt has been seen replied to assumes one reply per
        prompt, which the projection does not promise. Recording a per-prompt "last reply"
        offset in the manifest would work and is a change to ``transcript_export``, not to a
        reader, so it is not smuggled in here. What *is* honoured is the caller's own bound:
        ``--limit`` stops the scan the moment it has enough, ``--tail`` runs backwards from
        the end instead, and a window end caps it. An unbounded question over an early
        selection reads to EOF, and the section comment above says so rather than leaving it
        to be measured.
        """

        _check_page(limit, tail)
        selected_prompts = self.list_prompts(filters, ordinal_range, which)
        if not selected_prompts:
            return []
        selected_prompt_ids = {
            as_string(record.get("record_id"), "prompt.record_id")
            for record in selected_prompts
        }
        prompt_ordinals = {
            as_string(record.get("record_id"), "prompt.record_id"): as_int(
                record.get("ordinal"), "prompt.ordinal"
            )
            for record in selected_prompts
        }
        earliest = min(
            as_int(record.get("timestamp_ms"), "prompt.timestamp_ms")
            for record in selected_prompts
        )
        window_start, window_end = self._window_bounds(filters)
        if self._exhaustive:
            lower = window_start
        elif window_start is not None:
            lower = max(window_start, earliest)
        else:
            lower = earliest

        def accept(record: dict[str, JsonValue]) -> bool:
            return self._message_ordinal(record, selected_prompt_ids) is not None and (
                self._selected(record, filters)
            )

        def project(record: dict[str, JsonValue]) -> dict[str, JsonValue]:
            item = dict(record)
            if as_string(record.get("record_type"), "message.record_type") == "response":
                prompt_id = record.get("in_reply_to_prompt_id")
                item["prompt_ordinal"] = (
                    prompt_ordinals.get(prompt_id) if isinstance(prompt_id, str) else None
                )
            return item

        if tail is not None:
            end_offset = (
                None
                if window_end is None
                else self._reader("messages.jsonl").seek_first(
                    "timestamp_ms", window_end, str(self._managed("messages.jsonl").path)
                )
            )
            return [
                project(record)
                for record in self._tail_matching(
                    "messages.jsonl", tail, accept, end=end_offset
                )
            ]
        result: list[dict[str, JsonValue]] = []
        for record in self._iter_instant_span("messages.jsonl", lower, window_end):
            if not accept(record):
                continue
            result.append(project(record))
            if limit is not None and len(result) >= limit:
                break
        return result

    def _message_ordinal(
        self, record: dict[str, JsonValue], selected_prompt_ids: set[str]
    ) -> str | None:
        """Return the message's record type when it belongs to the selection, else ``None``."""

        record_type = as_string(record.get("record_type"), "message.record_type")
        if record_type == "prompt":
            record_id = as_string(record.get("record_id"), "prompt.record_id")
            return record_type if record_id in selected_prompt_ids else None
        if record_type == "response":
            prompt_id = record.get("in_reply_to_prompt_id")
            return (
                record_type
                if isinstance(prompt_id, str) and prompt_id in selected_prompt_ids
                else None
            )
        raise ValueError(f"unknown transcript message type {record_type!r}")

    def content_stats(
        self, filters: QueryFilters
    ) -> tuple[TextTotals, TextTotals, TextTotals, TextTotals, TextTotals]:
        """Count mechanically attributed prompt classes and responses in *filters*."""

        human_prompt_texts: list[str] = []
        bot_prompt_texts: list[str] = []
        unattributed_prompt_texts: list[str] = []
        for record in self._stream("prompts.jsonl"):
            if not self._selected(record, filters):
                continue
            text = as_string(record.get("text"), "prompt.text")
            prompt_class = self._prompt_class(record)
            if prompt_class == "human":
                human_prompt_texts.append(text)
            elif prompt_class == "bot":
                bot_prompt_texts.append(text)
            else:
                unattributed_prompt_texts.append(text)
        linked_response_texts: list[str] = []
        unlinked_response_texts: list[str] = []
        for record in self._stream("messages.jsonl"):
            if (
                as_string(record.get("record_type"), "message.record_type")
                != "response"
            ):
                continue
            if not self._selected(record, filters):
                continue
            text = as_string(record.get("text"), "response.text")
            if isinstance(record.get("in_reply_to_prompt_id"), str):
                linked_response_texts.append(text)
            else:
                unlinked_response_texts.append(text)
        return (
            TextTotals.from_texts(human_prompt_texts),
            TextTotals.from_texts(bot_prompt_texts),
            TextTotals.from_texts(unattributed_prompt_texts),
            TextTotals.from_texts(linked_response_texts),
            TextTotals.from_texts(unlinked_response_texts),
        )


def _record_array(
    timeline: dict[str, JsonValue], key: str
) -> tuple[dict[str, JsonValue], ...]:
    return tuple(
        as_object(value, f"timeline.{key}[{index}]")
        for index, value in enumerate(as_array(timeline.get(key), f"timeline.{key}"))
    )


def _team(record: dict[str, JsonValue], where: str) -> str:
    return as_string(record.get("team"), f"{where}.team")


def _stamp_sole_team(records: Sequence[dict[str, JsonValue]], slug: str | None) -> None:
    """Give records their team when the archive has exactly one and they do not say so.

    The single-team renderer does not put a ``team`` field on a phase, a rollup or a summary
    file: there is one team, and schema 1 does not carry the field. Every reference and every
    listing in this file is team-qualified, so without this the archive shape *most* archives
    have could not be read at all -- ``./timeline rollups`` on a single-team export raised
    ``expected a string`` from the middle of the schema-2 loader, and ``./timeline phases``
    raised the same from schema 1. That was true before schema 3 existed and is fixed here
    rather than left, because "schema 1 and schema 2 remain readable" is the contract schema 3
    is allowed to be added under, and a contract nothing checks is a contract that has already
    lapsed.

    The writer's `timeline_v3._team_of` makes exactly this inference on the way out, and a
    schema-3 spine shard makes it structurally -- the shard *is* the team. This is the same
    rule, applied to the two generations that have nowhere else to get it from.

    Stamped in place. The dictionaries belong to the parsed timeline, nothing else reads those
    collections out of it, and copying them would double the resident size of a 247 MB schema-1
    document to add one short string per record.
    """

    if slug is None:
        return
    for record in records:
        if record.get("team") is None:
            record["team"] = slug


def _local_identifier(team: str, identifier: str) -> str:
    prefix = f"{team}::"
    return identifier[len(prefix) :] if identifier.startswith(prefix) else identifier


def team_ref(team: str) -> str:
    """Return the canonical reference for one team."""

    return f"team:{team}"


def agent_ref(record: dict[str, JsonValue], where: str = "agent") -> str:
    """Return a canonical reference independent of single- versus multi-team export IDs."""

    team = _team(record, where)
    identifier = as_string(record.get("id"), f"{where}.id")
    return f"agent:{team}::{_local_identifier(team, identifier)}"


def phase_ref(record: dict[str, JsonValue], where: str = "phase") -> str:
    """Return the canonical reference for one work phase."""

    team = _team(record, where)
    identifier = as_string(record.get("id"), f"{where}.id")
    return f"phase:{team}::{_local_identifier(team, identifier)}"


def rollup_ref(record: dict[str, JsonValue], where: str = "rollup") -> str:
    """Return the canonical reference for one calendar rollup."""

    team = _team(record, where)
    kind = as_string(record.get("kind"), f"{where}.kind")
    start_ms = as_int(record.get("start_ms"), f"{where}.start_ms")
    return f"rollup:{team}::{kind}::{start_ms}"


#: How to build the reference for each record kind that has one. A table rather than an
#: ``if`` chain because both the whole-archive index and the schema-3 seek need exactly this
#: dispatch, and two copies of it would be two chances to disagree about what a reference is.
_REFERENCE_OF: dict[str, Callable[[dict[str, JsonValue]], str]] = {
    "agent": agent_ref,
    "phase": phase_ref,
    "rollup": rollup_ref,
}


def _timestamp(timestamp_ms: int) -> str:
    return (
        datetime.fromtimestamp(timestamp_ms / 1000, tz=timezone.utc)
        .isoformat(timespec="milliseconds")
        .replace("+00:00", "Z")
    )


def _interval(record: dict[str, JsonValue], where: str) -> tuple[int, int]:
    start_ms = as_int(record.get("start_ms"), f"{where}.start_ms")
    end_ms = as_int(record.get("end_ms"), f"{where}.end_ms")
    return start_ms, end_ms


def _overlaps(
    record: dict[str, JsonValue], where: str, window: TimeWindow | None
) -> bool:
    if window is None:
        return True
    start_ms, end_ms = _interval(record, where)
    return window.overlaps(start_ms, end_ms)


def _with_times(
    result: dict[str, JsonValue], record: dict[str, JsonValue], where: str
) -> None:
    start_ms, end_ms = _interval(record, where)
    result["start_time"] = _timestamp(start_ms)
    result["end_time"] = _timestamp(end_ms)


def _optional_string(record: dict[str, JsonValue], key: str) -> str | None:
    value = record.get(key)
    return value if isinstance(value, str) else None


def _optional_int(record: dict[str, JsonValue], key: str) -> int | None:
    value = record.get(key)
    return value if isinstance(value, int) and not isinstance(value, bool) else None


def _summary_available(record: dict[str, JsonValue], key: str) -> bool:
    if key not in record:
        return True
    value = record[key]
    if not isinstance(value, bool):
        raise ValueError(f"{key}: expected a boolean")
    return value


def _compact_text(value: str) -> str:
    return _WHITESPACE.sub(" ", value).strip()


def _search_word_character(value: str) -> bool:
    return bool(value) and (value == "_" or value.isalnum())


def _pattern_ranges(
    pattern: re.Pattern[str], compact: str, *, whole_word: bool = False
) -> tuple[tuple[int, int], ...]:
    result: list[tuple[int, int]] = []
    for match in pattern.finditer(compact):
        if whole_word and (
            _search_word_character(compact[match.start() - 1 : match.start()])
            or _search_word_character(compact[match.end() : match.end() + 1])
        ):
            continue
        result.append((match.start(), match.end()))
        if len(result) == 64:
            break
    return tuple(result)


def _smart_parts(needle: str) -> tuple[tuple[str, bool], ...]:
    parts: list[tuple[str, bool]] = []
    for match in re.finditer(r'"([^"\\]*(?:\\.[^"\\]*)*)"|([^ ]+)', needle):
        quoted = match.group(1)
        if quoted is not None:
            value = quoted.replace(r"\"", '"').replace(r"\\", "\\")
            if value:
                parts.append((value, True))
        else:
            value = match.group(2)
            if value:
                parts.append((value, False))
    return tuple(parts)


def _compile_search_matcher(
    needle: str,
    *,
    match_mode: str,
    case_sensitive: bool,
) -> _SearchMatcher:
    compact_needle = _compact_text(needle)
    if not compact_needle:
        raise ValueError("search text must not be empty")

    def pattern_flags(value: str) -> int:
        if case_sensitive:
            return 0
        # The portable Bloom format lowercases ASCII only.  Keep non-ASCII
        # code points exact while folding ASCII letters, avoiding Python's
        # extra Unicode IGNORECASE equivalences (for example K/Kelvin sign).
        return re.IGNORECASE | re.ASCII

    patterns: tuple[re.Pattern[str], ...]
    whole_words: tuple[bool, ...]
    bloom_terms: tuple[str, ...]
    if match_mode in {"literal", "phrase"}:
        patterns = (
            re.compile(re.escape(compact_needle), pattern_flags(compact_needle)),
        )
        whole_words = (False,)
        bloom_terms = (compact_needle,)
    elif match_mode == "smart":
        parts = _smart_parts(compact_needle)
        if not parts:
            raise ValueError("search text must contain a term")
        compiled: list[re.Pattern[str]] = []
        whole: list[bool] = []
        for part, quoted in parts:
            flags = pattern_flags(part)
            compiled.append(re.compile(re.escape(part), flags))
            whole.append(
                not quoted
                and all(
                    character in {"_", "-"} or character.isalnum()
                    for character in part
                )
            )
        patterns = tuple(compiled)
        whole_words = tuple(whole)
        bloom_terms = tuple(part for part, _ in parts)
    else:
        raise ValueError(f"unsupported search match mode {match_mode!r}")
    return _SearchMatcher(
        compact_needle,
        patterns,
        whole_words,
        bloom_terms,
        case_sensitive,
    )


def _ascii_lower_text(value: str) -> str:
    return value.translate(
        str.maketrans("ABCDEFGHIJKLMNOPQRSTUVWXYZ", "abcdefghijklmnopqrstuvwxyz")
    )


def _ascii_lower_utf8(value: str) -> bytes:
    encoded = value.encode("utf-8")
    return bytes(byte + 32 if 65 <= byte <= 90 else byte for byte in encoded)


def _trigram_positions(
    trigram: bytes, bit_count: int, hash_count: int
) -> tuple[int, ...]:
    def digest(seed: int) -> int:
        result = seed
        for byte in trigram:
            result ^= byte
            result = (result * _TRIGRAM_BLOOM_FNV_PRIME) & _UINT32_MASK
        return result

    first = digest(_TRIGRAM_BLOOM_FNV_OFFSET)
    second = digest(_TRIGRAM_BLOOM_FNV_OFFSET ^ _TRIGRAM_BLOOM_SECOND_SEED) | 1
    mask = bit_count - 1
    return tuple((first + index * second) & mask for index in range(hash_count))


def _bloom_might_match(raw: JsonValue, terms: tuple[str, ...], where: str) -> bool:
    """Validate one catalog Bloom filter and reject only definite misses."""

    if raw is None:
        return True
    value = as_object(raw, where)
    algorithm = as_string(value.get("algorithm"), where + ".algorithm")
    if algorithm != _TRIGRAM_BLOOM_ALGORITHM:
        raise ValueError(f"{where}.algorithm: unsupported value {algorithm!r}")
    bit_count = as_int(value.get("bit_count"), where + ".bit_count")
    if bit_count < 64 or bit_count & (bit_count - 1):
        raise ValueError(f"{where}.bit_count: expected a power of two of at least 64")
    hash_count = as_int(value.get("hash_count"), where + ".hash_count")
    if hash_count != _TRIGRAM_BLOOM_HASH_COUNT:
        raise ValueError(f"{where}.hash_count: expected {_TRIGRAM_BLOOM_HASH_COUNT}")
    trigram_count = as_int(value.get("trigram_count"), where + ".trigram_count")
    if trigram_count < 0:
        raise ValueError(f"{where}.trigram_count: expected a non-negative integer")
    encoded = as_string(value.get("bits_base64"), where + ".bits_base64")
    try:
        bits = base64.b64decode(encoded, validate=True)
    except ValueError as error:
        raise ValueError(f"{where}.bits_base64: invalid base64") from error
    if len(bits) * 8 != bit_count:
        raise ValueError(
            f"{where}.bits_base64: decoded {len(bits) * 8} bits, expected {bit_count}"
        )
    for term in terms:
        compact = _compact_text(term)
        if not compact.isascii():
            continue
        term_bytes = _ascii_lower_utf8(compact)
        if len(term_bytes) < 3:
            continue
        for offset in range(len(term_bytes) - 2):
            trigram = term_bytes[offset : offset + 3]
            for position in _trigram_positions(trigram, bit_count, hash_count):
                if not bits[position // 8] & (1 << (position % 8)):
                    return False
    return True


def _bloom_can_prune(term: str) -> bool:
    """Whether a trigram filter could ever reject a shard on account of this term.

    The mirror of the two ``continue``s inside :func:`_bloom_might_match`: a term with a non-ASCII
    code point is skipped because the portable filter normalizes ASCII only, and one shorter than
    three bytes has no trigram to look up. A query all of whose terms are skipped cannot prune
    anything, so under schema 3 -- where the filters are a stream rather than bootstrap fields --
    it does not read them. Stated as its own predicate rather than inlined so that the condition
    for *fetching* the filters is textually the condition for *using* them.
    """

    compact = _compact_text(term)
    return compact.isascii() and len(_ascii_lower_utf8(compact)) >= 3


def _day_bounds(day: str, where: str) -> tuple[int, int]:
    """The half-open UTC interval a ``YYYY-MM-DD`` shard label names."""

    try:
        midnight = datetime.strptime(day, "%Y-%m-%d").replace(tzinfo=timezone.utc)
    except ValueError as error:
        raise ValueError(f"{where}: not a UTC day label: {day!r}") from error
    start_ms = int(midnight.timestamp() * 1000)
    return start_ms, start_ms + _DAY_MS


def _search_excerpt(match: _TextMatch) -> dict[str, JsonValue]:
    first = min(start for start, _ in match.ranges)
    start = max(0, first - 120)
    end = min(len(match.compact), start + 480)
    excerpt = match.compact[start:end]
    ranges: list[JsonValue] = []
    for left, right in match.ranges:
        if right <= start or left >= end:
            continue
        ranges.append([max(0, left - start), min(end, right) - start])
    return {
        "text": excerpt,
        "full_characters": len(match.compact),
        "leading_omitted_characters": start,
        "trailing_omitted_characters": len(match.compact) - end,
        "is_truncated": start > 0 or end < len(match.compact),
        "match_ranges": ranges,
    }


def _copy_fields(
    record: dict[str, JsonValue], keys: tuple[str, ...]
) -> dict[str, JsonValue]:
    return {key: record[key] for key in keys if key in record}


def _sorted_refs(references: Iterable[str]) -> list[JsonValue]:
    """A sorted list of stable references, typed as the JSON it becomes."""

    return [reference for reference in sorted(references)]


def schema_3_completeness(root: Path) -> tuple[bool, str]:
    """Whether *root* holds a schema-3 generation complete enough to read, and why not if not.

    The same five-clause rule :class:`_SchemaThreeArchive` applies before it will answer a
    question, published as a plain predicate so a *writer* can ask it too. The caller that
    needs it is :mod:`wrkviz.archive_gc`: "may the schema-1 monolith be reclaimed"
    is exactly "is there a complete newer generation that answers the same questions", and the
    only defensible way to answer that is to run the reader's own acceptance rule rather than a
    second, looser one that could say yes where the reader says no.

    Returning a reason rather than a bare bool is what makes a refusal actionable: a dry run
    that says "held: no schema-3 bootstrap" tells an operator to rebuild, and one that says
    "held: shard X is 40 bytes short" tells them the last build was interrupted.
    """

    archive, declined = _SchemaThreeArchive.open(root)
    return archive is not None, declined


#: How ``inspect``'s six counts are assembled from the shard catalogue. The left-hand names are
#: schema-1 collection names because that is what the command has always printed; the right-hand
#: pairs are ``(stream, record kind)`` as the bootstrap publishes them. ``edges`` is two entries
#: because schema 3 splits them: the spawn tree lives in the spine and everything else is
#: time-addressed, and schema 1 counted the union.
_SCHEMA_3_COUNT_SOURCES: tuple[tuple[str, tuple[tuple[str, str], ...]], ...] = (
    ("agents", (("spine", "agent"),)),
    ("phases", (("timeline", "phase"),)),
    ("edges", (("timeline", "edge"), ("spine", "structural_edge"))),
    ("events", (("timeline", "event"),)),
    ("rollups", (("spine", "rollup"),)),
    ("summary_files", (("spine", "summary_file"),)),
)


def schema_3_record_counts(root: Path) -> dict[str, int] | None:
    """How many records of each schema-1 collection a complete schema-3 generation holds.

    ``None`` when there is no complete generation to count, so a caller can fall back.

    This exists because ``inspect`` printed six integers and paid 246,973,399 bytes and 1.44 GiB
    of resident memory for them, by parsing the schema-1 monolith whole. Every one of the six is
    already published in the 168,703-byte bootstrap -- a timeline shard's catalogue entry carries
    ``counts`` per record kind and a spine shard's carries ``line_ranges``, whose second element
    is a count -- so the answer is a sum over a list the reader already validated.

    The counts are read off the *catalogue* rather than by opening shards, which is the whole
    point: no shard is inflated, and in particular the timeline stream, 93% of the generation,
    stays shut. That the catalogue's numbers are the shards' numbers is not taken on trust
    either; it is what :meth:`_SchemaThreeArchive._check_present` establishes for the bytes and
    what `test_timeline_v3.py` establishes for the records.
    """

    archive, _declined = _SchemaThreeArchive.open(root)
    if archive is None:
        return None
    tallies: dict[tuple[str, str], int] = {}
    streams = as_object(archive.bootstrap.get("streams"), "timeline-v3.streams")
    for stream, raw_section in streams.items():
        where = f"timeline-v3.streams.{stream}"
        section = as_object(raw_section, where)
        for index, raw in enumerate(as_array(section.get("shards"), where + ".shards")):
            entry = as_object(raw, f"{where}.shards[{index}]")
            counts = entry.get("counts")
            if counts is not None:
                for kind, value in as_object(counts, f"{where}.shards[{index}].counts").items():
                    key = (stream, kind)
                    tallies[key] = tallies.get(key, 0) + as_int(
                        value, f"{where}.shards[{index}].counts.{kind}"
                    )
            for kind, span in _schema_3_line_ranges(
                entry.get("line_ranges"), f"{where}.shards[{index}].line_ranges"
            ).items():
                key = (stream, kind)
                tallies[key] = tallies.get(key, 0) + span[1]
    return {
        name: sum(tallies.get(source, 0) for source in sources)
        for name, sources in _SCHEMA_3_COUNT_SOURCES
    }


class TimelineQuery:
    """Validated, read-only view of a built single- or multi-team timeline.

    Three generations answer the same questions and this class prefers the cheapest complete
    one: schema 3's shards, then schema 2's content-addressed objects, then schema 1's
    monolith. The older two are **not** dead code -- an archive built before schema 3 existed
    must keep working, and that compatibility is the contract, not a leftover.

    Under schema 3 nothing is read at construction beyond the 168,703-byte bootstrap, and every
    collection below is a property that reads the line range it needs when it is first asked
    for. Under schemas 2 and 1 the whole projection is in memory before the first question,
    because those formats offer nothing smaller to read. :attr:`bytes_read` reports what it
    cost either way, which is how the difference is asserted rather than described.
    """

    def __init__(self, root: Path) -> None:
        self.root = root.resolve()
        self._bytes_read = 0
        self._timeline_v2_bootstrap: dict[str, JsonValue] | None = None
        self._schema_3: _SchemaThreeArchive | None = None
        #: Empty when schema 3 was used; otherwise one sentence saying why it was not. Read by
        #: the tests that pin the completeness rule, and by anyone asking why a rebuilt archive
        #: is still being served from schema 2.
        self._schema_3, self.schema_3_declined = _SchemaThreeArchive.open(self.root)
        if self._schema_3 is not None:
            self.timeline = self._schema_3.header()
        else:
            self.timeline = self._load_older_generation()
        schema_version = as_int(
            self.timeline.get("schema_version"), "timeline.schema_version"
        )
        if schema_version != TIMELINE_SCHEMA_VERSION:
            raise ValueError(
                f"unsupported timeline schema version {schema_version}; "
                f"expected {TIMELINE_SCHEMA_VERSION}"
            )
        self.teams = _record_array(self.timeline, "teams")
        #: The one team's slug when the archive has exactly one, otherwise ``None``. See
        #: :func:`_stamp_sole_team` for what depends on it.
        self._sole_team = (
            as_string(self.teams[0].get("slug"), "timeline.teams[0].slug")
            if len(self.teams) == 1
            else None
        )
        self._collections: dict[str, tuple[dict[str, JsonValue], ...]] = {}
        self._entries: dict[str, _IndexEntry] | None = None
        self._phase_intervals: dict[tuple[str, str], tuple[tuple[int, int, str], ...]] | None = (
            None
        )

    # -- opening ---------------------------------------------------------------------------

    def _load_older_generation(self) -> dict[str, JsonValue]:
        """Schema 2 if it is present and current, otherwise schema 1."""

        bootstrap_path = self.root / "data" / "timeline-v2.json"
        if not bootstrap_path.is_file():
            if bootstrap_path.exists():
                raise ValueError(
                    f"timeline schema-2 bootstrap is not a regular file: {bootstrap_path}"
                )
            return self._load_schema_1_timeline()
        bootstrap = as_object(self._read_json(bootstrap_path), str(bootstrap_path))
        bootstrap_schema = as_int(
            bootstrap.get("schema_version"), "timeline-v2.schema_version"
        )
        predates_phase_index = (
            bootstrap_schema == 2
            and bootstrap.get("kind") == "timeline-bootstrap"
            and "phase_index" not in bootstrap
        )
        if predates_phase_index:
            return self._load_schema_1_timeline()
        timeline, self._timeline_v2_bootstrap = self._load_schema_2_timeline(
            bootstrap_path, bootstrap
        )
        return timeline

    def _load_schema_1_timeline(self) -> dict[str, JsonValue]:
        timeline_path = self.root / "data" / "timeline.json"
        if not timeline_path.is_file():
            raise ValueError(f"no built timeline at {timeline_path}")
        return as_object(self._read_json(timeline_path), str(timeline_path))

    # -- accounting ------------------------------------------------------------------------

    @property
    def bytes_read(self) -> int:
        """Every byte this query has pulled off disk, including the generation it opened with.

        A byte count and not a clock, for the reason `test_query_read_paths.py` gives about the
        transcript projections: the claim under test is "an answer costs the span it lives in",
        and a stopwatch on a build host measures the host.
        """

        schema_3 = 0 if self._schema_3 is None else self._schema_3.bytes_read
        return self._bytes_read + schema_3

    def _read_bytes(self, path: Path) -> bytes:
        encoded = path.read_bytes()
        self._bytes_read += len(encoded)
        return encoded

    def _read_json(self, path: Path) -> JsonValue:
        raw = self._read_bytes(path)
        if path.name.endswith(".gz"):
            # Counted against the read budget as the bytes actually read off disk, which is what
            # `_read_bytes` already recorded; decompressing does not read anything more.
            raw = gzip.decompress(raw)
        return _narrow_json(json.loads(raw.decode("utf-8")), str(path))

    def _read_markdown(self, path: Path) -> str:
        return self._read_bytes(path).decode("utf-8")

    @property
    def opened_shards(self) -> tuple[str, ...]:
        """The schema-3 shards this query has opened; empty under schemas 1 and 2."""

        return () if self._schema_3 is None else self._schema_3.opened_shards

    # -- the one place two generations meet ------------------------------------------------

    def _search_bootstrap(self) -> dict[str, JsonValue]:
        """The schema-2 bootstrap, which is where an older archive's search corpus lives.

        Schema 3 now publishes a corpus of its own -- the ``search``, ``search_bloom`` and
        ``search_links`` streams -- and :meth:`_iter_search_records` prefers it. This path is what
        an archive built before those streams gets: its schema 3 answers phases and agents from
        the spine and its messages still come from schema 2, and that is the only operation in
        this file that touches both generations. It is reached less and less as archives are
        rebuilt, and it is not dead: the compatibility is the contract.

        It is therefore still the place the cross-generation check belongs -- and only here,
        because a schema-3 corpus makes the question moot: the messages and the phases then come
        out of the same bootstrap and cannot disagree about their source. Verifying at open that
        schema 3 and schema 2 describe the same ``source_digest`` would mean parsing schema 2's
        5,702,530-byte bootstrap before answering a question schema 3 answers in 300 KB -- the
        cost schema 3 exists to remove, paid to guard a mismatch that only matters here.
        Deferring it costs nothing: the file is being read anyway, and the comparison is a
        string.

        The mismatch is a refusal rather than a fallback. Two generations of one archive
        disagreeing about their source is not something a reader can paper over by picking one:
        the phases would come from one build and the messages from another, and the linkage
        between them -- which phase a message falls inside -- would be silently wrong.
        """

        if self._timeline_v2_bootstrap is None:
            bootstrap_path = self.root / "data" / "timeline-v2.json"
            if not bootstrap_path.is_file():
                raise ValueError(
                    "this archive has no transcript search corpus; rebuild the website"
                )
            bootstrap = as_object(
                self._read_json(bootstrap_path), str(bootstrap_path)
            )
            if (
                as_int(bootstrap.get("schema_version"), "timeline-v2.schema_version") != 2
                or bootstrap.get("kind") != "timeline-bootstrap"
            ):
                raise ValueError(
                    f"unsupported timeline schema-2 bootstrap at {bootstrap_path}"
                )
            ours = self.timeline.get("source_digest")
            theirs = bootstrap.get("source_digest")
            if ours is not None and theirs is not None and ours != theirs:
                raise ValueError(
                    "the transcript search corpus belongs to a different source generation "
                    f"({theirs!r}) than the timeline being read ({ours!r}); rebuild the website"
                )
            self._timeline_v2_bootstrap = bootstrap
        if self._timeline_v2_bootstrap.get("search") is None:
            raise ValueError(
                "this archive has no transcript search corpus; rebuild the website"
            )
        return self._timeline_v2_bootstrap

    # -- collections -----------------------------------------------------------------------

    def _collection(self, name: str) -> tuple[dict[str, JsonValue], ...]:
        """One top-level collection, read from whichever generation is open.

        Under schema 3 this is the seek: the spine kind's published line range, for every team,
        read once and remembered. Under schemas 1 and 2 the records are already in
        ``self.timeline`` and this is a lookup.
        """

        cached = self._collections.get(name)
        if cached is not None:
            return cached
        if self._schema_3 is None:
            records = _record_array(self.timeline, name)
            _stamp_sole_team(records, self._sole_team)
        else:
            # A schema-3 spine shard is per team, so `_SchemaThreeArchive._unwrap` has already
            # stamped every record from the path it read it out of -- better informed than the
            # sole-team inference, and correct for a multi-team archive too.
            records = tuple(
                self._schema_3.spine(_SCHEMA_3_SPINE_KIND_FOR[_SINGULAR[name]])
            )
        self._collections[name] = records
        return records

    def _scoped(
        self, name: str, filters: QueryFilters
    ) -> tuple[dict[str, JsonValue], ...]:
        """One collection restricted to the teams a filter selects.

        The restriction is pushed down to the shard under schema 3 -- a ``--team`` listing
        opens one spine and no others -- and applied afterwards under schemas 1 and 2, where
        the records are in memory and there is nothing to push down to. Callers still filter by
        team themselves; this narrows what they have to walk, it does not replace the check.
        """

        if self._schema_3 is None or not filters.teams:
            return self._collection(name)
        return tuple(
            self._schema_3.spine(
                _SCHEMA_3_SPINE_KIND_FOR[_SINGULAR[name]], filters.teams
            )
        )

    def _for_team(self, name: str, team: str) -> tuple[dict[str, JsonValue], ...]:
        """One collection restricted to a single named team."""

        if self._schema_3 is None:
            return tuple(
                record
                for record in self._collection(name)
                if _team(record, _SINGULAR[name]) == team
            )
        return tuple(
            self._schema_3.spine(_SCHEMA_3_SPINE_KIND_FOR[_SINGULAR[name]], (team,))
        )

    @property
    def agents(self) -> tuple[dict[str, JsonValue], ...]:
        """Every agent in the archive."""

        return self._collection("agents")

    @property
    def phases(self) -> tuple[dict[str, JsonValue], ...]:
        """Every work phase in the archive.

        Under schema 3 these are the phase *cards* -- the same nine fields schema 2's phase
        index publishes -- rather than the full phase records schema 1 holds. Nothing in this
        file reads a phase field outside the card.
        """

        return self._collection("phases")

    @property
    def rollups(self) -> tuple[dict[str, JsonValue], ...]:
        """Every calendar rollup in the archive."""

        return self._collection("rollups")

    def _content_addressed_object(
        self, raw_reference: JsonValue, where: str
    ) -> tuple[dict[str, JsonValue], str]:
        reference = as_object(raw_reference, where)
        digest = as_string(reference.get("sha256"), where + ".sha256")
        if re.fullmatch(r"[0-9a-f]{64}", digest) is None:
            raise ValueError(f"{where}.sha256: expected a lowercase SHA-256 digest")
        relative = as_string(reference.get("url"), where + ".url")
        expected_relative = f"data/timeline-v2/objects/{digest}.json"
        if relative != expected_relative:
            raise ValueError(
                f"{where}.url: expected content-addressed path {expected_relative!r}"
            )
        path, _pure = self._safe_file(relative)
        encoded = self._read_bytes(path)
        if hashlib.sha256(encoded).hexdigest() != digest:
            raise ValueError(f"{where}: object digest mismatch for {relative}")
        raw_bytes = reference.get("bytes")
        if raw_bytes is not None and as_int(raw_bytes, where + ".bytes") != len(
            encoded
        ):
            raise ValueError(f"{where}: object byte count mismatch for {relative}")
        try:
            decoded: object = json.loads(encoded.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise ValueError(f"{where}: invalid JSON object {relative}") from error
        return as_object(_narrow_json(decoded, relative), relative), relative

    @staticmethod
    def _schema_2_range(
        record: dict[str, JsonValue], where: str
    ) -> tuple[dict[str, JsonValue], int, int]:
        time_range = as_object(record.get("range"), where + ".range")
        start_ms = as_int(time_range.get("start_ms"), where + ".range.start_ms")
        end_ms = as_int(time_range.get("end_ms"), where + ".range.end_ms")
        if end_ms <= start_ms:
            raise ValueError(f"{where}.range: end must be after start")
        return time_range, start_ms, end_ms

    @staticmethod
    def _schema_2_teams(
        record: dict[str, JsonValue], where: str
    ) -> tuple[list[JsonValue], frozenset[str]]:
        raw_teams = as_array(record.get("teams"), where + ".teams")
        if not raw_teams:
            raise ValueError(f"{where}.teams: must contain at least one team")
        team_slugs: set[str] = set()
        for index, raw_team in enumerate(raw_teams):
            team = as_object(raw_team, f"{where}.teams[{index}]")
            slug = as_string(team.get("slug"), f"{where}.teams[{index}].slug")
            if not slug:
                raise ValueError(f"{where}.teams[{index}].slug: must not be empty")
            if slug in team_slugs:
                raise ValueError(f"{where}.teams: duplicate team slug {slug!r}")
            team_slugs.add(slug)
        return raw_teams, frozenset(team_slugs)

    def _load_schema_2_timeline(
        self,
        bootstrap_path: Path,
        bootstrap: dict[str, JsonValue],
    ) -> tuple[dict[str, JsonValue], dict[str, JsonValue]]:
        if (
            as_int(bootstrap.get("schema_version"), "timeline-v2.schema_version") != 2
            or bootstrap.get("kind") != "timeline-bootstrap"
        ):
            raise ValueError(
                f"unsupported timeline schema-2 bootstrap at {bootstrap_path}"
            )
        source_digest = as_string(
            bootstrap.get("source_digest"), "timeline-v2.source_digest"
        )
        if not source_digest:
            raise ValueError("timeline-v2.source_digest: must not be empty")
        teams, team_slugs = self._schema_2_teams(bootstrap, "timeline-v2")
        time_range, timeline_start_ms, timeline_end_ms = self._schema_2_range(
            bootstrap, "timeline-v2"
        )

        global_record, global_relative = self._content_addressed_object(
            bootstrap.get("global"), "timeline-v2.global"
        )
        if (
            as_int(
                global_record.get("schema_version"), global_relative + ".schema_version"
            )
            != 2
            or global_record.get("kind") != "timeline-global"
        ):
            raise ValueError(f"unsupported timeline global object: {global_relative}")
        global_digest = global_record.get("source_digest")
        if (
            global_digest is not None
            and as_string(global_digest, global_relative + ".source_digest")
            != source_digest
        ):
            raise ValueError(
                "timeline global object belongs to a different source generation"
            )
        if "range" in global_record:
            global_range, _global_start, _global_end = self._schema_2_range(
                global_record, global_relative
            )
            if global_range != time_range:
                raise ValueError("timeline global range does not match bootstrap")
        if "teams" in global_record:
            _global_teams, global_team_slugs = self._schema_2_teams(
                global_record, global_relative
            )
            if global_team_slugs != team_slugs:
                raise ValueError("timeline global team set does not match bootstrap")

        agents = tuple(
            as_object(raw, f"{global_relative}.agents[{index}]")
            for index, raw in enumerate(
                as_array(global_record.get("agents"), global_relative + ".agents")
            )
        )
        # The sole-team inference has to happen before the validation below rather than after
        # it: a single-team export's rollups, project overviews and phases carry no ``team`` at
        # all, and every one of these loops reads the field. Applied to the *global object's*
        # own collections, which is what the returned timeline is assembled from.
        sole = next(iter(team_slugs)) if len(team_slugs) == 1 else None
        agent_teams: dict[str, str] = {}
        _stamp_sole_team(agents, sole)
        for index, agent in enumerate(agents):
            where = f"{global_relative}.agents[{index}]"
            identifier = as_string(agent.get("id"), where + ".id")
            team = _team(agent, where)
            if team not in team_slugs:
                raise ValueError(f"{where}.team: unknown team {team!r}")
            agent_start_ms, agent_end_ms = _interval(agent, where)
            if (
                agent_end_ms <= agent_start_ms
                or agent_start_ms < timeline_start_ms
                or agent_end_ms > timeline_end_ms
            ):
                raise ValueError(f"{where}: agent interval is outside timeline range")
            if identifier in agent_teams:
                raise ValueError(
                    f"{global_relative}.agents: duplicate exact agent id {identifier!r}"
                )
            agent_teams[identifier] = team
        for key in ("rollups", "project_overviews"):
            raw_values = global_record.get(key)
            if raw_values is None:
                continue
            for index, raw in enumerate(
                as_array(raw_values, f"{global_relative}.{key}")
            ):
                record = as_object(raw, f"{global_relative}.{key}[{index}]")
                _stamp_sole_team((record,), sole)
                team = _team(record, f"{global_relative}.{key}[{index}]")
                if team not in team_slugs:
                    raise ValueError(
                        f"{global_relative}.{key}[{index}].team: unknown team {team!r}"
                    )

        phase_index, phase_relative = self._content_addressed_object(
            bootstrap.get("phase_index"), "timeline-v2.phase_index"
        )
        if (
            as_int(
                phase_index.get("schema_version"), phase_relative + ".schema_version"
            )
            != 2
            or phase_index.get("kind") != "timeline-phase-index"
        ):
            raise ValueError(f"unsupported timeline phase index: {phase_relative}")
        phase_digest = phase_index.get("source_digest")
        if (
            phase_digest is not None
            and as_string(phase_digest, phase_relative + ".source_digest")
            != source_digest
        ):
            raise ValueError(
                "timeline phase index belongs to a different source generation"
            )
        phases: list[JsonValue] = []
        seen_phase_ids: set[str] = set()
        for index, raw_phase in enumerate(
            as_array(phase_index.get("phases"), phase_relative + ".phases")
        ):
            where = f"{phase_relative}.phases[{index}]"
            phase = dict(as_object(raw_phase, where))
            phase_id = as_string(phase.get("id"), where + ".id")
            if phase_id in seen_phase_ids:
                raise ValueError(f"{phase_relative}.phases: duplicate id {phase_id!r}")
            seen_phase_ids.add(phase_id)
            agent_id = as_string(phase.get("agent_id"), where + ".agent_id")
            phase_start_ms, phase_end_ms = _interval(phase, where)
            if (
                phase_end_ms <= phase_start_ms
                or phase_start_ms < timeline_start_ms
                or phase_end_ms > timeline_end_ms
            ):
                raise ValueError(f"{where}: phase interval is outside timeline range")
            phase_team = agent_teams.get(agent_id)
            if phase_team is None:
                raise ValueError(
                    f"{where}.agent_id: no exact global agent match for {agent_id!r}"
                )
            existing_team = phase.get("team")
            if (
                existing_team is not None
                and as_string(existing_team, where + ".team") != phase_team
            ):
                raise ValueError(f"{where}.team: does not match phase agent")
            phase["team"] = phase_team
            phases.append(phase)

        for index, raw_bin in enumerate(
            as_array(bootstrap.get("activity_bins"), "timeline-v2.activity_bins")
        ):
            activity_bin = as_object(raw_bin, f"timeline-v2.activity_bins[{index}]")
            _stamp_sole_team((activity_bin,), sole)
            team = _team(activity_bin, f"timeline-v2.activity_bins[{index}]")
            if team not in team_slugs:
                raise ValueError(
                    f"timeline-v2.activity_bins[{index}].team: unknown team {team!r}"
                )

        timeline = {
            key: value
            for key, value in global_record.items()
            if key
            not in {
                "schema_version",
                "kind",
                "source_digest",
                "range",
                "teams",
            }
        }
        timeline.update(
            {
                "schema_version": TIMELINE_SCHEMA_VERSION,
                "generated_at": as_string(
                    bootstrap.get("generated_at"), "timeline-v2.generated_at"
                ),
                "source_digest": source_digest,
                "display_timezone": as_string(
                    bootstrap.get("display_timezone"), "timeline-v2.display_timezone"
                ),
                "display_timezone_source": as_string(
                    bootstrap.get("display_timezone_source", "legacy_team_data"),
                    "timeline-v2.display_timezone_source",
                ),
                "range": time_range,
                "teams": teams,
                "activity_bins": bootstrap.get("activity_bins"),
                "phases": phases,
                "events": [],
            }
        )
        return timeline, bootstrap

    def _project_overviews(
        self, teams: Sequence[str] = ()
    ) -> tuple[dict[str, JsonValue], ...]:
        if self._schema_3 is not None:
            # One spine kind whichever renderer produced the archive: the writer folds the
            # single-team ``project_overview`` object and the combined export's labelled list
            # into the same kind, so the reader never has to know which it opened.
            return tuple(self._schema_3.spine("project_overview", teams))
        plural = self.timeline.get("project_overviews")
        if plural is not None:
            return tuple(
                as_object(value, f"timeline.project_overviews[{index}]")
                for index, value in enumerate(
                    as_array(plural, "timeline.project_overviews")
                )
            )
        singular = self.timeline.get("project_overview")
        if singular is None:
            return ()
        if len(self.teams) != 1:
            raise ValueError(
                "timeline.project_overview requires exactly one timeline team"
            )
        result = dict(as_object(singular, "timeline.project_overview"))
        result["team"] = as_string(self.teams[0].get("slug"), "timeline.teams[0].slug")
        return (result,)

    @staticmethod
    def _summary_kind_stats(texts: list[str], unavailable: int) -> SummaryKindStats:
        return SummaryKindStats(
            available=len(texts),
            unavailable=unavailable,
            content=TextTotals.from_texts(texts),
        )

    def activity_bins(self, filters: QueryFilters) -> list[dict[str, JsonValue]]:
        """The pre-aggregated activity bins overlapping the filter's window.

        The only collection in the archive that a windowed read can *seek* into rather than
        scan: bins are one time-sorted shard in schema 3, so a caller asking for one day reads
        the members that day falls in. Every other collection this class serves -- agents,
        phase cards, rollups -- is addressed by line range, because it has no honest position
        on the time axis (an agent alive across the whole archive has one ``start_ms``, which is
        outside almost every window a reader will ask about) and a range read over it would
        appear to work and quietly under-return.

        No command-line subcommand consumes this today; schema 2 validated the bins at open and
        no query has ever asked for one. It is published because the bins are what an overview
        chart draws, and the point of schema 3 is that drawing that chart should not require
        loading the archive.
        """

        start_ms = None if filters.window is None else filters.window.start_ms
        end_ms = None if filters.window is None else filters.window.end_ms
        if self._schema_3 is not None:
            records = self._schema_3.activity_bins(start_ms, end_ms)
        else:
            records = [
                as_object(raw, f"timeline.activity_bins[{index}]")
                for index, raw in enumerate(
                    as_array(
                        self.timeline.get("activity_bins") or [], "timeline.activity_bins"
                    )
                )
            ]
            _stamp_sole_team(records, self._sole_team)
            records = [
                record
                for record in records
                if _within(
                    as_int(record.get("start_ms"), "activity_bin.start_ms"),
                    start_ms,
                    end_ms,
                )
            ]
        selected = [
            record
            for record in records
            if self._selected_team(_team(record, "activity_bin"), filters)
        ]
        # Sorted, and totally: schema 3 reads the bins in instant order because that is how the
        # shard is written, and schemas 1 and 2 hand them back in the order the renderer emitted
        # them. Neither order is the archive's, so the answer states one. The tiebreaker is the
        # encoded record, the same total order the writer sorts shards by.
        selected.sort(
            key=lambda record: (
                as_int(record.get("start_ms"), "activity_bin.start_ms"),
                canonical_json(record),
            )
        )
        return selected

    def summary_stats(self, filters: QueryFilters) -> dict[str, SummaryKindStats]:
        """Count available and unavailable summaries in the presentation projection."""

        project_texts: list[str] = []
        project_unavailable = 0
        # Project overviews describe a whole team and have no honest time interval. Exclude
        # them from time-sliced statistics instead of pretending they belong to every window.
        if filters.window is None:
            for record in self._project_overviews(filters.teams):
                team = _team(record, "project_overview")
                if not self._selected_team(team, filters):
                    continue
                if _summary_available(record, "summary_available"):
                    project_texts.append(
                        as_string(record.get("text"), "project_overview.text")
                    )
                else:
                    project_unavailable += 1

        agent_texts: list[str] = []
        agent_unavailable = 0
        for record in self._scoped("agents", filters):
            team = _team(record, "agent")
            if not self._selected_team(team, filters):
                continue
            if not _overlaps(record, agent_ref(record), filters.window):
                continue
            if _summary_available(record, "summary_available"):
                agent_texts.append(
                    as_string(record.get("lifetime_summary"), "agent.lifetime_summary")
                )
            else:
                agent_unavailable += 1

        phase_texts: list[str] = []
        phase_unavailable = 0
        for record in self._scoped("phases", filters):
            team = _team(record, "phase")
            if not self._selected_team(team, filters):
                continue
            if not _overlaps(record, phase_ref(record), filters.window):
                continue
            if _summary_available(record, "summary_available"):
                phrase = as_string(record.get("phrase"), "phase.phrase")
                paragraph = as_string(record.get("paragraph"), "phase.paragraph")
                phase_texts.append(f"{phrase}\n{paragraph}")
            else:
                phase_unavailable += 1

        technical_texts: list[str] = []
        technical_unavailable = 0
        plain_texts: list[str] = []
        plain_unavailable = 0
        for record in self._scoped("rollups", filters):
            team = _team(record, "rollup")
            kind = as_string(record.get("kind"), "rollup.kind")
            reference = rollup_ref(record)
            if not self._selected_team(team, filters):
                continue
            if filters.rollup_kinds and kind not in filters.rollup_kinds:
                continue
            if not _overlaps(record, reference, filters.window):
                continue
            if _summary_available(record, "technical_summary_available"):
                relative = as_string(
                    record.get("technical_path"), "rollup.technical_path"
                )
                technical_texts.append(
                    self._read_markdown(self._rollup_file(relative, team))
                )
            else:
                technical_unavailable += 1
            if _summary_available(record, "plain_language_summary_available"):
                relative = as_string(
                    record.get("plain_language_path"),
                    "rollup.plain_language_path",
                )
                plain_texts.append(
                    self._read_markdown(self._rollup_file(relative, team))
                )
            else:
                plain_unavailable += 1

        return {
            "project_overviews": self._summary_kind_stats(
                project_texts, project_unavailable
            ),
            "agent_lifetimes": self._summary_kind_stats(agent_texts, agent_unavailable),
            "work_phases": self._summary_kind_stats(phase_texts, phase_unavailable),
            "rollup_technical": self._summary_kind_stats(
                technical_texts, technical_unavailable
            ),
            "rollup_plain_language": self._summary_kind_stats(
                plain_texts, plain_unavailable
            ),
        }

    def lookup(self, reference: str) -> _IndexEntry | None:
        """Resolve one stable reference, reading as little as the generation allows.

        Under schemas 1 and 2 this is a dictionary built over everything, once, because
        everything is already in memory. Under schema 3 it is a **seek**: a reference carries
        the team it belongs to -- ``agent:<team>::<id>``, ``phase:<team>::<id>``,
        ``rollup:<team>::<kind>::<start_ms>`` -- so only that team's spine is opened, and only
        the line range of the one kind the prefix names. Resolving a reference in a twelve-team
        archive therefore costs one team's records rather than 14,584.

        A malformed or unknown reference returns ``None`` rather than raising, because two
        callers want different words for it: `show` says "unknown stable reference" and
        ``--agent`` says "must be an agent:... reference".
        """

        kind, separator, remainder = reference.partition(":")
        if not separator:
            return None
        if kind == "team":
            for index, record in enumerate(self.teams):
                slug = as_string(record.get("slug"), f"timeline.teams[{index}].slug")
                if slug == remainder:
                    return _IndexEntry("team", record)
            return None
        if kind not in _SCHEMA_3_SPINE_KIND_FOR:
            return None
        if self._schema_3 is None:
            return self._index().get(reference)
        team, team_separator, _ = remainder.partition("::")
        if not team_separator:
            return None
        found = self._schema_3.refs(kind, team, _REFERENCE_OF[kind]).get(reference)
        return None if found is None else _IndexEntry(kind, found)

    def _index(self) -> dict[str, _IndexEntry]:
        """The whole-archive reference index, for the generations that have no other door.

        Built on first use rather than at construction, which moves one check with it: two
        records sharing a stable reference used to be refused when the archive was opened and is
        now refused the first time a reference is resolved. That is a deliberate trade -- the
        check cost a full walk of every agent, phase and rollup on every `stats` invocation that
        would never resolve a reference -- and it is narrower than it looks, because schema 2's
        loader refuses a duplicate agent identifier on its own path, and schema 3's writer
        refuses a duplicate team slug before it publishes anything.
        """

        if self._entries is not None:
            return self._entries
        entries: dict[str, _IndexEntry] = {}
        for name, kind in _SINGULAR.items():
            reference_of = _REFERENCE_OF[kind]
            for record in self._collection(name):
                reference = reference_of(record)
                if reference in entries:
                    raise ValueError(f"duplicate stable reference {reference!r}")
                entries[reference] = _IndexEntry(kind, record)
        self._entries = entries
        return entries

    def _selected_team(self, team: str, filters: QueryFilters) -> bool:
        return not filters.teams or team in filters.teams

    def _validated_agent_filter(self, filters: QueryFilters) -> str | None:
        if filters.agent_ref is None:
            return None
        entry = self.lookup(filters.agent_ref)
        if entry is None:
            raise ValueError(f"unknown stable reference {filters.agent_ref!r}")
        if entry.kind != "agent":
            raise ValueError("--agent must be an agent:... reference")
        return filters.agent_ref

    def list_records(
        self, resource: str, filters: QueryFilters
    ) -> list[dict[str, JsonValue]]:
        """Return concise, stable projections for one resource kind."""

        selected_agent = self._validated_agent_filter(filters)
        if resource == "teams":
            records = self._list_teams(filters)
        elif resource == "agents":
            records = self._list_agents(filters, selected_agent)
        elif resource == "phases":
            records = self._list_phases(filters, selected_agent)
        elif resource == "rollups":
            records = self._list_rollups(filters)
        else:
            raise ValueError(f"unsupported query resource {resource!r}")
        return sorted(
            records,
            key=lambda item: (
                _optional_int(item, "start_ms") or -1,
                _optional_string(item, "ref") or "",
            ),
        )

    def _team_range(self, slug: str) -> tuple[int, int] | None:
        intervals = [
            _interval(record, f"agent {agent_ref(record)}")
            for record in self._for_team("agents", slug)
        ]
        if not intervals:
            intervals = [
                _interval(record, f"rollup {rollup_ref(record)}")
                for record in self._for_team("rollups", slug)
            ]
        if not intervals:
            return None
        return min(start for start, _end in intervals), max(
            end for _start, end in intervals
        )

    def _list_teams(self, filters: QueryFilters) -> list[dict[str, JsonValue]]:
        results: list[dict[str, JsonValue]] = []
        for index, record in enumerate(self.teams):
            where = f"timeline.teams[{index}]"
            slug = as_string(record.get("slug"), f"{where}.slug")
            interval = self._team_range(slug)
            if not self._selected_team(slug, filters):
                continue
            if interval is not None and filters.window is not None:
                if not filters.window.overlaps(interval[0], interval[1]):
                    continue
            result = _copy_fields(
                record,
                ("slug", "label", "provider", "projects", "hosts", "stats"),
            )
            result["ref"] = team_ref(slug)
            if interval is not None:
                result["start_ms"], result["end_ms"] = interval
                result["start_time"] = _timestamp(interval[0])
                result["end_time"] = _timestamp(interval[1])
            results.append(result)
        return results

    def _list_agents(
        self, filters: QueryFilters, selected_agent: str | None
    ) -> list[dict[str, JsonValue]]:
        results: list[dict[str, JsonValue]] = []
        for index, record in enumerate(self._scoped("agents", filters)):
            where = f"timeline.agents[{index}]"
            reference = agent_ref(record, where)
            team = _team(record, where)
            if not self._selected_team(team, filters):
                continue
            if selected_agent is not None and reference != selected_agent:
                continue
            if not _overlaps(record, where, filters.window):
                continue
            result = _copy_fields(
                record,
                (
                    "team",
                    "short_name",
                    "official_name",
                    "nickname",
                    "lifetime_summary",
                    "summary_available",
                    "depth",
                    "status",
                    "start_ms",
                    "end_ms",
                ),
            )
            result["ref"] = reference
            parent = record.get("parent_id")
            result["parent_ref"] = (
                None
                if parent is None
                else self._agent_reference_for_id(
                    team, as_string(parent, f"{where}.parent_id")
                )
            )
            _with_times(result, record, where)
            results.append(result)
        return results

    def _list_phases(
        self, filters: QueryFilters, selected_agent: str | None
    ) -> list[dict[str, JsonValue]]:
        results: list[dict[str, JsonValue]] = []
        for index, record in enumerate(self._scoped("phases", filters)):
            where = f"timeline.phases[{index}]"
            team = _team(record, where)
            current_agent = self._agent_reference_for_id(
                team, as_string(record.get("agent_id"), f"{where}.agent_id")
            )
            if not self._selected_team(team, filters):
                continue
            if selected_agent is not None and current_agent != selected_agent:
                continue
            if not _overlaps(record, where, filters.window):
                continue
            result = _copy_fields(
                record,
                (
                    "team",
                    "phrase",
                    "paragraph",
                    "summary_available",
                    "stats",
                    "start_ms",
                    "end_ms",
                ),
            )
            result["ref"] = phase_ref(record, where)
            result["agent_ref"] = current_agent
            _with_times(result, record, where)
            results.append(result)
        return results

    def _list_rollups(self, filters: QueryFilters) -> list[dict[str, JsonValue]]:
        results: list[dict[str, JsonValue]] = []
        for index, record in enumerate(self._scoped("rollups", filters)):
            where = f"timeline.rollups[{index}]"
            team = _team(record, where)
            kind = as_string(record.get("kind"), f"{where}.kind")
            if not self._selected_team(team, filters):
                continue
            if filters.rollup_kinds and kind not in filters.rollup_kinds:
                continue
            if not _overlaps(record, where, filters.window):
                continue
            result = _copy_fields(
                record,
                (
                    "team",
                    "kind",
                    "label",
                    "start_ms",
                    "end_ms",
                    "technical_path",
                    "plain_language_path",
                    "technical_summary_available",
                    "plain_language_summary_available",
                    "summary_available",
                    "stats",
                ),
            )
            result["ref"] = rollup_ref(record, where)
            _with_times(result, record, where)
            results.append(result)
        return results

    def _agent_reference_for_id(self, team: str, identifier: str) -> str:
        return f"agent:{team}::{_local_identifier(team, identifier)}"

    @staticmethod
    def _same_agent_identifier(team: str, left: str, right: str) -> bool:
        return _local_identifier(team, left) == _local_identifier(team, right)

    def _safe_file(self, relative: str) -> tuple[Path, PurePosixPath]:
        pure = PurePosixPath(relative)
        if (
            pure.is_absolute()
            or not pure.parts
            or any(part in {"", ".", ".."} for part in pure.parts)
        ):
            raise ValueError(f"archive path escapes root: {relative!r}")
        path = self.root.joinpath(*pure.parts).resolve()
        try:
            path.relative_to(self.root)
        except ValueError as error:
            raise ValueError(f"archive path escapes root: {relative!r}") from error
        if not path.is_file():
            # A resource whose stored form is the compressed member has no identity file on
            # disk. Resolve it the way the server does -- identity first, then the `.gz` -- so
            # a caller can keep naming the logical path. Without this the CLI reads the
            # filesystem and finds nothing where a browser, going through the server, sees the
            # document: `show <phase>` broke exactly that way when the per-phase details
            # stopped keeping a twin.
            stored = path.with_name(path.name + ".gz")
            if stored.is_file():
                return stored, pure
            raise ValueError(f"archive file is missing: {relative}")
        return path, pure

    def _detail_file(self, relative: str) -> Path:
        path, pure = self._safe_file(relative)
        if (
            len(pure.parts) < 3
            or pure.parts[:2] != ("data", "details")
            or pure.suffix != ".json"
        ):
            raise ValueError(f"phase detail path is outside data/details: {relative!r}")
        # Validated on the LOGICAL name above, deliberately: `pure` is what the timeline stream
        # recorded and is always `.json`, while `path` may be the `.gz` the archive stores. A
        # check against the stored name would reject every detail the moment it is compressed.
        return path

    def _rollup_file(self, relative: str, team: str) -> Path:
        path, pure = self._safe_file(relative)
        if (
            len(pure.parts) < 5
            or pure.parts[:3] != ("teams", team, "summaries")
            or pure.suffix != ".md"
        ):
            raise ValueError(
                f"rollup path is outside teams/{team}/summaries: {relative!r}"
            )
        return path

    def _iter_search_records(
        self,
        filters: QueryFilters | None = None,
        *,
        bloom_terms: tuple[str, ...] = (),
    ) -> Iterable[dict[str, JsonValue]]:
        """Yield validated records while retaining at most one decoded day shard.

        Two generations can answer this and the newer one is preferred, exactly as everywhere else
        in this class -- but here the fallback is not a slower path to the same file, it is a
        different file. A schema-3 generation that carries the ``search`` streams answers out of
        its own bootstrap and never opens ``data/timeline-v2.json``; one built before those streams
        existed has no corpus of its own, and the schema-2 objects are still the only copy.

        The records the two paths yield are the same records, field for field, because the
        schema-3 line *is* the schema-2 record plus the envelope key the reader strips. That is
        what lets both go through one validator below, and it is what
        `test_timeline_v3_search.py` asserts query by query rather than by inspection.
        """

        if self._schema_3 is not None and self._schema_3.has_search_corpus:
            yield from self._iter_search_records_v3(
                self._schema_3, filters, bloom_terms
            )
            return
        yield from self._iter_search_records_v2(filters, bloom_terms)

    def _iter_search_records_v3(
        self,
        archive: "_SchemaThreeArchive",
        filters: QueryFilters | None,
        bloom_terms: tuple[str, ...],
    ) -> Iterator[dict[str, JsonValue]]:
        """Walk the schema-3 ``search`` stream, shard by shard, pruning before opening."""

        timeline_teams = frozenset(
            as_string(team.get("slug"), "timeline team.slug") for team in self.teams
        )
        teams = () if filters is None else filters.teams
        # The prefilter is fetched only when at least one term can produce a trigram. A query
        # like the case study's `B3` -- two bytes -- cannot, so `_bloom_might_match` would accept
        # every shard and reading the filters would be pure cost. Schema 2 could not make this
        # choice: its filters arrive inside a bootstrap every command parses regardless.
        blooms = (
            archive.search_blooms(teams)
            if any(_bloom_can_prune(term) for term in bloom_terms)
            else {}
        )
        seen: set[str] = set()
        for entry in archive.search_shards():
            where = f"timeline-v3.streams.search {entry.path}"
            shard_team = entry.team
            if shard_team is None or entry.day is None:
                raise ValueError(f"invalid transcript search shard scope at {where}")
            if shard_team not in timeline_teams:
                raise ValueError(f"invalid transcript search shard scope at {where}")
            shard_start, shard_end = _day_bounds(entry.day, where)
            if filters is not None:
                if filters.teams and shard_team not in filters.teams:
                    continue
                if filters.window is not None and not filters.window.overlaps(
                    shard_start, shard_end
                ):
                    continue
            if blooms and not _bloom_might_match(
                blooms.get(entry.path), bloom_terms, where + ".bloom"
            ):
                continue
            for index, record in enumerate(archive.iter_search_shard(entry)):
                yield self._validated_search_record(
                    record,
                    f"{entry.path}.records[{index}]",
                    shard_team,
                    shard_start,
                    shard_end,
                    seen,
                )

    def _iter_search_records_v2(
        self,
        filters: QueryFilters | None,
        bloom_terms: tuple[str, ...],
    ) -> Iterator[dict[str, JsonValue]]:
        """Walk schema 2's content-addressed day shards, for an archive with no schema-3 corpus."""

        bootstrap = self._search_bootstrap()
        timeline_teams = sorted(
            as_string(team.get("slug"), "timeline team.slug") for team in self.teams
        )
        raw_search = bootstrap.get("search")
        if raw_search is None:
            raise ValueError(
                "this archive has no transcript search corpus; rebuild the website"
            )
        search = as_object(raw_search, "timeline-v2.search")
        if search.get("strategy") != "transcript-message-shards":
            raise ValueError(
                "this archive predates transcript search; rebuild the website"
            )
        if (
            as_int(search.get("schema_version"), "timeline-v2.search.schema_version")
            != 1
        ):
            raise ValueError("unsupported transcript search schema")
        source_digest = as_string(
            bootstrap.get("source_digest"), "timeline-v2.source_digest"
        )
        seen: set[str] = set()
        for shard_index, raw_shard in enumerate(
            as_array(search.get("shards"), "timeline-v2.search.shards")
        ):
            where = f"timeline-v2.search.shards[{shard_index}]"
            shard = as_object(raw_shard, where)
            shard_team = as_string(shard.get("team"), where + ".team")
            shard_start = as_int(shard.get("start_ms"), where + ".start_ms")
            shard_end = as_int(shard.get("end_ms"), where + ".end_ms")
            if shard_team not in timeline_teams or shard_end <= shard_start:
                raise ValueError(f"invalid transcript search shard scope at {where}")
            if filters is not None:
                if filters.teams and shard_team not in filters.teams:
                    continue
                if filters.window is not None and not filters.window.overlaps(
                    shard_start, shard_end
                ):
                    continue
            if bloom_terms and not _bloom_might_match(
                shard.get("trigram_bloom"),
                bloom_terms,
                where + ".trigram_bloom",
            ):
                continue
            root, relative = self._content_addressed_object(shard, where)
            if (
                root.get("schema_version") != 1
                or root.get("kind") != "timeline-search-day"
            ):
                raise ValueError(f"unsupported transcript search shard: {relative}")
            shard_digest = root.get("source_digest")
            if (
                shard_digest is not None
                and as_string(shard_digest, relative + ".source_digest")
                != source_digest
            ):
                raise ValueError(
                    f"transcript search shard belongs to a different source generation: {relative}"
                )
            if as_string(root.get("team"), relative + ".team") != shard_team:
                raise ValueError(f"transcript search shard team mismatch: {relative}")
            root_range = as_object(root.get("range"), relative + ".range")
            if root_range != {"start_ms": shard_start, "end_ms": shard_end}:
                raise ValueError(f"transcript search shard range mismatch: {relative}")
            raw_records = as_array(root.get("records"), relative + ".records")
            catalog_counts = as_object(shard.get("counts"), where + ".counts")
            if as_int(catalog_counts.get("records"), where + ".counts.records") != len(
                raw_records
            ):
                raise ValueError(f"transcript search shard count mismatch: {relative}")
            for record_index, raw_record in enumerate(raw_records):
                yield self._validated_search_record(
                    as_object(raw_record, f"{relative}.records[{record_index}]"),
                    f"{relative}.records[{record_index}]",
                    shard_team,
                    shard_start,
                    shard_end,
                    seen,
                )

    def _validated_search_record(
        self,
        record: dict[str, JsonValue],
        where: str,
        shard_team: str,
        shard_start: int,
        shard_end: int,
        seen: set[str],
    ) -> dict[str, JsonValue]:
        """Check one transcript search record against everything else the archive says.

        Shared by both generations rather than written twice, and that is the point: these
        refusals are the contract a search result rests on -- that a reference names the record it
        is attached to, that the agent it is attributed to exists in the same team, that a
        ``prompt_ref`` does not cross a team boundary -- and a second copy for the newer format
        would be a second chance for the two to disagree about what a valid corpus is. The
        schema-3 record is the schema-2 record plus one envelope key the reader has already
        stripped, so there is nothing left for a separate validator to say.
        """

        reference = as_string(record.get("ref"), f"{where}.ref")
        if as_int(record.get("schema_version"), f"{where}.schema_version") != 1:
            raise ValueError(f"unsupported transcript search record: {reference}")
        record_team = as_string(record.get("team"), f"{where}.team")
        at_ms = as_int(record.get("at_ms"), f"{where}.at_ms")
        if record_team != shard_team or not shard_start <= at_ms < shard_end:
            raise ValueError(f"transcript search record escapes shard: {reference}")
        message_prefix = f"message:{record_team}::"
        tool_prefix = f"tool:{record_team}::"
        if not reference.startswith((message_prefix, tool_prefix)):
            raise ValueError(f"invalid transcript search reference {reference!r}")
        record_type = as_string(record.get("record_type"), f"{where}.record_type")
        event_identifier = as_string(record.get("event_id"), f"{where}.event_id")
        expected_reference = (
            tool_prefix if record_type == "tool" else message_prefix
        ) + event_identifier
        if reference != expected_reference:
            raise ValueError(f"transcript search reference kind mismatch: {reference}")
        agent_reference = as_string(record.get("agent_ref"), f"{where}.agent_ref")
        agent_identifier = as_string(record.get("agent_id"), f"{where}.agent_id")
        if self._agent_reference_for_id(record_team, agent_identifier) != agent_reference:
            raise ValueError(
                f"transcript search record agent identity mismatch: {reference}"
            )
        agent_entry = self.lookup(agent_reference)
        if agent_entry is None or agent_entry.kind != "agent":
            raise ValueError(
                f"transcript search record has unknown agent {agent_reference!r}"
            )
        if _team(agent_entry.record, "search record agent") != record_team:
            raise ValueError(
                f"transcript search record agent belongs to another team: {reference}"
            )
        prompt_reference = record.get("prompt_ref")
        if prompt_reference is not None and not as_string(
            prompt_reference, f"{where}.prompt_ref"
        ).startswith(message_prefix):
            raise ValueError(
                f"transcript search prompt reference belongs to another team: {reference}"
            )
        prompt_in_scope = record.get("prompt_in_scope")
        if prompt_in_scope is not None and not isinstance(prompt_in_scope, bool):
            raise ValueError(f"{where}.prompt_in_scope: expected a boolean")
        role = as_string(record.get("role"), f"{where}.role")
        if role not in SEARCH_ROLES:
            raise ValueError(f"unsupported transcript search role {role!r}")
        as_string(record.get("text"), f"{where}.text")
        if reference in seen:
            raise ValueError(f"duplicate transcript search reference {reference!r}")
        seen.add(reference)
        return record

    def _search_link_context_from_records(
        self,
        filters: QueryFilters,
        selected_agent: str | None,
        candidate_prompt_refs: frozenset[str],
        needed_prompt_refs: frozenset[str],
    ) -> _SearchLinkContext:
        """Compatibility path for archives written before linkage sidecars."""

        prompt_excerpts: dict[str, str] = {}
        response_counts: dict[str, int] = {}
        team_filters = QueryFilters(teams=filters.teams)
        for record in self._iter_search_records(team_filters):
            reference = as_string(record.get("ref"), "search record.ref")
            record_type = as_string(
                record.get("record_type"), "search record.record_type"
            )
            if reference in needed_prompt_refs and record_type in {
                "prompt",
                "inter_agent_prompt",
            }:
                prompt_excerpts[reference] = _compact_text(
                    as_string(record.get("text"), "search record.text")
                )[:320]
            if record_type not in {"response", "inter_agent_response"}:
                continue
            prompt_reference = _optional_string(record, "prompt_ref")
            if prompt_reference not in candidate_prompt_refs:
                continue
            at_ms = as_int(record.get("at_ms"), "search record.at_ms")
            if filters.window is not None and not filters.window.contains(at_ms):
                continue
            agent_reference = as_string(
                record.get("agent_ref"), "search record.agent_ref"
            )
            if selected_agent is not None and agent_reference != selected_agent:
                continue
            response_counts[prompt_reference] = (
                response_counts.get(prompt_reference, 0) + 1
            )
        return _SearchLinkContext(prompt_excerpts, response_counts)

    def _search_link_context_from_sidecars(
        self,
        shards: list[JsonValue],
        filters: QueryFilters,
        selected_agent: str | None,
        candidate_prompt_refs: frozenset[str],
        needed_prompt_refs: frozenset[str],
    ) -> _SearchLinkContext:
        """Read compact linkage shards while retaining only candidate relationships."""

        bootstrap = self._search_bootstrap()
        source_digest = as_string(
            bootstrap.get("source_digest"), "timeline-v2.source_digest"
        )
        timeline_teams = frozenset(
            as_string(team.get("slug"), "timeline team.slug") for team in self.teams
        )
        prompt_excerpts: dict[str, str] = {}
        response_counts: dict[str, int] = {}
        seen_prompts: set[str] = set()
        seen_responses: set[str] = set()
        for shard_index, raw_shard in enumerate(shards):
            where = f"timeline-v2.search.shards[{shard_index}]"
            shard = as_object(raw_shard, where)
            shard_team = as_string(shard.get("team"), where + ".team")
            shard_start = as_int(shard.get("start_ms"), where + ".start_ms")
            shard_end = as_int(shard.get("end_ms"), where + ".end_ms")
            if shard_team not in timeline_teams or shard_end <= shard_start:
                raise ValueError(f"invalid transcript search shard scope at {where}")
            if filters.teams and shard_team not in filters.teams:
                continue
            raw_linkage = shard.get("linkage")
            linkage_reference = as_object(raw_linkage, where + ".linkage")
            linkage, relative = self._content_addressed_object(
                linkage_reference, where + ".linkage"
            )
            if (
                as_int(linkage.get("schema_version"), relative + ".schema_version") != 1
                or linkage.get("kind") != "timeline-search-links-day"
            ):
                raise ValueError(
                    f"unsupported transcript search linkage shard: {relative}"
                )
            if (
                as_string(linkage.get("source_digest"), relative + ".source_digest")
                != source_digest
            ):
                raise ValueError(
                    f"transcript search linkage shard belongs to a different source generation: {relative}"
                )
            if as_string(linkage.get("team"), relative + ".team") != shard_team:
                raise ValueError(
                    f"transcript search linkage shard team mismatch: {relative}"
                )
            linkage_range = as_object(linkage.get("range"), relative + ".range")
            if linkage_range != {"start_ms": shard_start, "end_ms": shard_end}:
                raise ValueError(
                    f"transcript search linkage shard range mismatch: {relative}"
                )
            raw_prompts = as_array(linkage.get("prompts"), relative + ".prompts")
            raw_responses = as_array(linkage.get("responses"), relative + ".responses")
            catalog_counts = as_object(
                linkage_reference.get("counts"), where + ".linkage.counts"
            )
            if as_int(
                catalog_counts.get("prompts"), where + ".linkage.counts.prompts"
            ) != len(raw_prompts):
                raise ValueError(
                    f"transcript search linkage prompt count mismatch: {relative}"
                )
            if as_int(
                catalog_counts.get("responses"),
                where + ".linkage.counts.responses",
            ) != len(raw_responses):
                raise ValueError(
                    f"transcript search linkage response count mismatch: {relative}"
                )
            message_prefix = f"message:{shard_team}::"
            agent_prefix = f"agent:{shard_team}::"
            for prompt_index, raw_prompt in enumerate(raw_prompts):
                prompt = as_object(raw_prompt, f"{relative}.prompts[{prompt_index}]")
                reference = as_string(
                    prompt.get("ref"), f"{relative}.prompts[{prompt_index}].ref"
                )
                if not reference.startswith(message_prefix):
                    raise ValueError(
                        f"transcript search linkage prompt belongs to another team: {reference}"
                    )
                excerpt = as_string(
                    prompt.get("excerpt"),
                    f"{relative}.prompts[{prompt_index}].excerpt",
                )
                if reference in needed_prompt_refs:
                    if reference in seen_prompts:
                        raise ValueError(
                            f"duplicate transcript search linkage prompt {reference!r}"
                        )
                    seen_prompts.add(reference)
                    prompt_excerpts[reference] = excerpt
            for response_index, raw_response in enumerate(raw_responses):
                response = as_object(
                    raw_response, f"{relative}.responses[{response_index}]"
                )
                reference = as_string(
                    response.get("ref"),
                    f"{relative}.responses[{response_index}].ref",
                )
                if not reference.startswith(message_prefix):
                    raise ValueError(
                        f"transcript search linkage response belongs to another team: {reference}"
                    )
                prompt_reference = as_string(
                    response.get("prompt_ref"),
                    f"{relative}.responses[{response_index}].prompt_ref",
                )
                if not prompt_reference.startswith(message_prefix):
                    raise ValueError(
                        f"transcript search linkage prompt belongs to another team: {reference}"
                    )
                at_ms = as_int(
                    response.get("at_ms"),
                    f"{relative}.responses[{response_index}].at_ms",
                )
                if not shard_start <= at_ms < shard_end:
                    raise ValueError(
                        f"transcript search linkage response escapes shard: {reference}"
                    )
                agent_reference = as_string(
                    response.get("agent_ref"),
                    f"{relative}.responses[{response_index}].agent_ref",
                )
                if not agent_reference.startswith(agent_prefix):
                    raise ValueError(
                        f"transcript search linkage agent belongs to another team: {reference}"
                    )
                agent_entry = self.lookup(agent_reference)
                if agent_entry is None or agent_entry.kind != "agent":
                    raise ValueError(
                        f"transcript search linkage response has unknown agent {agent_reference!r}"
                    )
                if (
                    _team(agent_entry.record, "search linkage response agent")
                    != shard_team
                ):
                    raise ValueError(
                        f"transcript search linkage agent belongs to another team: {reference}"
                    )
                if prompt_reference not in candidate_prompt_refs:
                    continue
                if filters.window is not None and not filters.window.contains(at_ms):
                    continue
                if selected_agent is not None and agent_reference != selected_agent:
                    continue
                if reference in seen_responses:
                    raise ValueError(
                        f"duplicate transcript search linkage response {reference!r}"
                    )
                seen_responses.add(reference)
                response_counts[prompt_reference] = (
                    response_counts.get(prompt_reference, 0) + 1
                )
        return _SearchLinkContext(prompt_excerpts, response_counts)

    def _search_link_context_from_streams(
        self,
        archive: "_SchemaThreeArchive",
        filters: QueryFilters,
        selected_agent: str | None,
        candidate_prompt_refs: frozenset[str],
        needed_prompt_refs: frozenset[str],
    ) -> _SearchLinkContext:
        """Resolve relationships from the schema-3 ``search_links`` stream.

        The same answer as :meth:`_search_link_context_from_sidecars` computed from the same
        fields, and deliberately the same refusals, with two differences that come from the
        substrate rather than from a change of mind.

        First, only the halves the caller needs are read: excerpts are fetched only when some
        matched record cites a prompt, and edges only when some matched record *is* a prompt whose
        replies have to be counted. Schema 2 reads both, because a content-addressed object is
        indivisible; a line range is not.

        Second, a response's instant is checked against the days the ``search`` stream actually
        publishes for that team rather than against the one day a per-day sidecar would have been
        cut to. That is the same check -- "this edge belongs to a shard of this corpus" -- restated
        for a per-team file, and it is why the day set is built here rather than assumed.
        """

        prompt_excerpts: dict[str, str] = {}
        response_counts: dict[str, int] = {}
        seen_prompts: set[str] = set()
        seen_responses: set[str] = set()
        days_by_team: dict[str, set[int]] = {}
        for entry in archive.search_shards():
            if entry.team is None or entry.day is None:
                continue
            start_ms, _end_ms = _day_bounds(
                entry.day, f"timeline-v3.streams.search {entry.path}"
            )
            days_by_team.setdefault(entry.team, set()).add(start_ms)
        for team in archive.teams_in_scope(filters.teams):
            prompts, responses = archive.search_links(
                team,
                prompts=bool(needed_prompt_refs),
                responses=bool(candidate_prompt_refs),
            )
            where = f"timeline-v3.streams.search_links {team}"
            message_prefix = f"message:{team}::"
            agent_prefix = f"agent:{team}::"
            days = days_by_team.get(team, set())
            for index, prompt in enumerate(prompts):
                reference = as_string(prompt.get("ref"), f"{where}.prompts[{index}].ref")
                if not reference.startswith(message_prefix):
                    raise ValueError(
                        f"transcript search linkage prompt belongs to another team: {reference}"
                    )
                excerpt = as_string(
                    prompt.get("excerpt"), f"{where}.prompts[{index}].excerpt"
                )
                if reference not in needed_prompt_refs:
                    continue
                if reference in seen_prompts:
                    raise ValueError(
                        f"duplicate transcript search linkage prompt {reference!r}"
                    )
                seen_prompts.add(reference)
                prompt_excerpts[reference] = excerpt
            for index, response in enumerate(responses):
                spot = f"{where}.responses[{index}]"
                reference = as_string(response.get("ref"), spot + ".ref")
                if not reference.startswith(message_prefix):
                    raise ValueError(
                        f"transcript search linkage response belongs to another team: {reference}"
                    )
                prompt_reference = as_string(
                    response.get("prompt_ref"), spot + ".prompt_ref"
                )
                if not prompt_reference.startswith(message_prefix):
                    raise ValueError(
                        f"transcript search linkage prompt belongs to another team: {reference}"
                    )
                at_ms = as_int(response.get("at_ms"), spot + ".at_ms")
                if at_ms - (at_ms % _DAY_MS) not in days:
                    raise ValueError(
                        f"transcript search linkage response escapes shard: {reference}"
                    )
                agent_reference = as_string(response.get("agent_ref"), spot + ".agent_ref")
                if not agent_reference.startswith(agent_prefix):
                    raise ValueError(
                        f"transcript search linkage agent belongs to another team: {reference}"
                    )
                agent_entry = self.lookup(agent_reference)
                if agent_entry is None or agent_entry.kind != "agent":
                    raise ValueError(
                        f"transcript search linkage response has unknown agent "
                        f"{agent_reference!r}"
                    )
                if _team(agent_entry.record, "search linkage response agent") != team:
                    raise ValueError(
                        f"transcript search linkage agent belongs to another team: {reference}"
                    )
                if prompt_reference not in candidate_prompt_refs:
                    continue
                if filters.window is not None and not filters.window.contains(at_ms):
                    continue
                if selected_agent is not None and agent_reference != selected_agent:
                    continue
                if reference in seen_responses:
                    raise ValueError(
                        f"duplicate transcript search linkage response {reference!r}"
                    )
                seen_responses.add(reference)
                response_counts[prompt_reference] = (
                    response_counts.get(prompt_reference, 0) + 1
                )
        return _SearchLinkContext(prompt_excerpts, response_counts)

    def _search_link_context(
        self,
        filters: QueryFilters,
        selected_agent: str | None,
        candidate_prompt_refs: frozenset[str],
        needed_prompt_refs: frozenset[str],
    ) -> _SearchLinkContext:
        """Resolve candidate relationships without reopening pruned text shards."""

        if not candidate_prompt_refs and not needed_prompt_refs:
            return _SearchLinkContext({}, {})
        if self._schema_3 is not None and self._schema_3.has_search_corpus:
            return self._search_link_context_from_streams(
                self._schema_3,
                filters,
                selected_agent,
                candidate_prompt_refs,
                needed_prompt_refs,
            )
        bootstrap = self._search_bootstrap()
        search = as_object(bootstrap.get("search"), "timeline-v2.search")
        shards = as_array(search.get("shards"), "timeline-v2.search.shards")
        if shards and all(
            "linkage" in as_object(raw, f"timeline-v2.search.shards[{index}]")
            for index, raw in enumerate(shards)
        ):
            return self._search_link_context_from_sidecars(
                shards,
                filters,
                selected_agent,
                candidate_prompt_refs,
                needed_prompt_refs,
            )
        return self._search_link_context_from_records(
            filters,
            selected_agent,
            candidate_prompt_refs,
            needed_prompt_refs,
        )

    def _search_phase_intervals(
        self,
    ) -> dict[tuple[str, str], tuple[tuple[int, int, str], ...]]:
        """Phase intervals per ``(team, agent)``, built the first time a search asks.

        Built lazily rather than in the constructor, which is where it used to live: a
        transcript search is the only caller, and paying for the whole phase collection to open
        an archive made every `list` and `stats` invocation carry the cost of a feature it does
        not use.
        """

        if self._phase_intervals is not None:
            return self._phase_intervals
        intervals: dict[tuple[str, str], list[tuple[int, int, str]]] = {}
        for index, phase in enumerate(self.phases):
            where = f"timeline.phases[{index}]"
            team = _team(phase, where)
            agent_id = _local_identifier(
                team, as_string(phase.get("agent_id"), where + ".agent_id")
            )
            start_ms, end_ms = _interval(phase, where)
            intervals.setdefault((team, agent_id), []).append(
                (start_ms, end_ms, phase_ref(phase, where))
            )
        self._phase_intervals = {
            key: tuple(sorted(values)) for key, values in intervals.items()
        }
        return self._phase_intervals

    def _phase_reference_for_search_record(
        self, record: dict[str, JsonValue]
    ) -> str | None:
        team = as_string(record.get("team"), "search record.team")
        agent_id = as_string(record.get("agent_id"), "search record.agent_id")
        at_ms = as_int(record.get("at_ms"), "search record.at_ms")
        candidates: list[tuple[int, str]] = []
        for start_ms, end_ms, reference in self._search_phase_intervals().get(
            (team, _local_identifier(team, agent_id)), ()
        ):
            if start_ms <= at_ms < end_ms:
                candidates.append((end_ms - start_ms, reference))
        return min(candidates)[1] if candidates else None

    def _show_search_record(self, reference: str) -> dict[str, JsonValue]:
        _, separator, remainder = reference.partition(":")
        team, team_separator, _ = remainder.partition("::")
        if not separator or not team_separator or not team:
            raise ValueError(f"invalid transcript search reference {reference!r}")
        record: dict[str, JsonValue] | None = None
        prompts: dict[str, dict[str, JsonValue]] = {}
        responses: list[dict[str, JsonValue]] = []
        for candidate in self._iter_search_records(QueryFilters(teams=(team,))):
            candidate_reference = as_string(candidate.get("ref"), "search record.ref")
            candidate_type = as_string(
                candidate.get("record_type"), "search record.record_type"
            )
            if candidate_type in {"prompt", "inter_agent_prompt"}:
                prompts[candidate_reference] = dict(candidate)
            if candidate_reference == reference:
                record = dict(candidate)
            if (
                _optional_string(candidate, "prompt_ref") == reference
                and candidate_reference != reference
                and candidate_type in {"response", "inter_agent_response"}
            ):
                responses.append(dict(candidate))
        if record is None:
            raise ValueError(f"unknown stable reference {reference!r}")
        result = dict(record)
        result["at_time"] = _timestamp(
            as_int(record.get("at_ms"), "search record.at_ms")
        )
        phase_reference = self._phase_reference_for_search_record(record)
        if phase_reference is not None:
            result["phase_ref"] = phase_reference
        prompt_reference = _optional_string(record, "prompt_ref")
        if prompt_reference is not None and prompt_reference != reference:
            prompt = prompts.get(prompt_reference)
            result["linked_prompt"] = dict(prompt) if prompt is not None else None
        if record.get("record_type") in {"prompt", "inter_agent_prompt"}:
            responses.sort(
                key=lambda candidate: (
                    as_int(candidate.get("at_ms"), "search response.at_ms"),
                    as_string(candidate.get("ref"), "search response.ref"),
                )
            )
            linked_responses: list[JsonValue] = []
            linked_responses.extend(responses)
            result["linked_responses"] = linked_responses
        return result

    def show(self, reference: str, *, transcript: bool = False) -> dict[str, JsonValue]:
        """Resolve one stable reference and include useful relationship links."""

        entry = self.lookup(reference)
        if entry is None:
            if reference.startswith(("message:", "tool:")):
                return self._show_search_record(reference)
            raise ValueError(f"unknown stable reference {reference!r}")
        result = dict(entry.record)
        result["ref"] = reference
        result["record_type"] = entry.kind
        if entry.kind == "team":
            self._expand_team(result, entry.record)
        elif entry.kind == "agent":
            self._expand_agent(result, entry.record)
        elif entry.kind == "phase":
            self._expand_phase(result, entry.record, transcript)
        elif entry.kind == "rollup":
            self._expand_rollup(result, entry.record)
        return result

    def _expand_team(
        self, result: dict[str, JsonValue], record: dict[str, JsonValue]
    ) -> None:
        slug = as_string(record.get("slug"), "team.slug")
        # Sorted, so the answer is a property of the archive rather than of the generation that
        # served it. Schema 2 hands these back in the order the renderer emitted them and a
        # schema-3 spine hands them back in identifier order, and a `show` whose output depends
        # on which file happened to be on disk is a `show` nobody can diff across a rebuild.
        result["agent_refs"] = _sorted_refs(
            agent_ref(agent) for agent in self._for_team("agents", slug)
        )
        result["rollup_refs"] = _sorted_refs(
            rollup_ref(rollup) for rollup in self._for_team("rollups", slug)
        )
        interval = self._team_range(slug)
        if interval is not None:
            result["start_ms"], result["end_ms"] = interval
            result["start_time"] = _timestamp(interval[0])
            result["end_time"] = _timestamp(interval[1])

    def _expand_agent(
        self, result: dict[str, JsonValue], record: dict[str, JsonValue]
    ) -> None:
        team = _team(record, "agent")
        identifier = as_string(record.get("id"), "agent.id")
        parent = record.get("parent_id")
        result["parent_ref"] = (
            None
            if parent is None
            else self._agent_reference_for_id(
                team, as_string(parent, "agent.parent_id")
            )
        )
        result["child_refs"] = _sorted_refs(
            agent_ref(candidate)
            for candidate in self._for_team("agents", team)
            if isinstance(candidate.get("parent_id"), str)
            and self._same_agent_identifier(
                team,
                as_string(candidate.get("parent_id"), "agent.parent_id"),
                identifier,
            )
        )
        result["phase_refs"] = _sorted_refs(
            phase_ref(phase)
            for phase in self._for_team("phases", team)
            if self._same_agent_identifier(
                team,
                as_string(phase.get("agent_id"), "phase.agent_id"),
                identifier,
            )
        )
        _with_times(result, record, "agent")

    def _expand_phase(
        self,
        result: dict[str, JsonValue],
        record: dict[str, JsonValue],
        transcript: bool,
    ) -> None:
        team = _team(record, "phase")
        identifier = as_string(record.get("agent_id"), "phase.agent_id")
        result["agent_ref"] = self._agent_reference_for_id(team, identifier)
        relative = as_string(record.get("detail_path"), "phase.detail_path")
        detail = as_object(self._read_json(self._detail_file(relative)), relative)
        result["detail"] = (
            detail
            if transcript
            else {key: value for key, value in detail.items() if key != "transcript"}
        )
        _with_times(result, record, "phase")

    def _expand_rollup(
        self, result: dict[str, JsonValue], record: dict[str, JsonValue]
    ) -> None:
        team = _team(record, "rollup")
        if _summary_available(record, "technical_summary_available"):
            technical_path = as_string(
                record.get("technical_path"), "rollup.technical_path"
            )
            result["technical_markdown"] = self._read_markdown(
                self._rollup_file(technical_path, team)
            )
        if _summary_available(record, "plain_language_summary_available"):
            plain_path = as_string(
                record.get("plain_language_path"), "rollup.plain_language_path"
            )
            result["plain_language_markdown"] = self._read_markdown(
                self._rollup_file(plain_path, team)
            )
        _with_times(result, record, "rollup")

    def search(
        self,
        needle: str,
        *,
        scope: str,
        filters: QueryFilters,
        case_sensitive: bool,
        limit: int,
    ) -> list[dict[str, JsonValue]]:
        """Search summaries and/or condensed transcript messages without an index."""

        if not needle:
            raise ValueError("search text must not be empty")
        if scope not in {"summaries", "transcripts", "all"}:
            raise ValueError(f"unsupported search scope {scope!r}")
        if limit < 1:
            raise ValueError("--limit must be at least 1")
        selected_agent = self._validated_agent_filter(filters)
        matches: list[dict[str, JsonValue]] = []
        if scope in {"summaries", "all"}:
            matches.extend(
                self._search_summaries(needle, filters, selected_agent, case_sensitive)
            )
        if scope in {"transcripts", "all"}:
            matches.extend(
                self._search_transcripts(
                    needle, filters, selected_agent, case_sensitive
                )
            )
        matches.sort(
            key=lambda item: (
                _optional_int(item, "at_ms") or _optional_int(item, "start_ms") or -1,
                _optional_string(item, "ref") or "",
                _optional_string(item, "field") or "",
            )
        )
        return matches[:limit]

    def search_v2(
        self,
        needle: str,
        *,
        corpus: str,
        filters: QueryFilters,
        case_sensitive: bool,
        match_mode: str,
        sort: str,
        prompt_author: str,
        linkage: str,
        roles: tuple[str, ...],
        offset: int,
        limit: int,
    ) -> SearchResults:
        """Search the canonical phase-independent transcript corpus."""

        if corpus not in SEARCH_CORPORA:
            raise ValueError(f"unsupported search corpus {corpus!r}")
        if match_mode not in SEARCH_MATCH_MODES:
            raise ValueError(f"unsupported search match mode {match_mode!r}")
        if sort not in SEARCH_SORTS:
            raise ValueError(f"unsupported search sort {sort!r}")
        if prompt_author not in SEARCH_PROMPT_AUTHORS:
            raise ValueError(f"unsupported prompt author filter {prompt_author!r}")
        if linkage not in SEARCH_LINKAGES:
            raise ValueError(f"unsupported linkage filter {linkage!r}")
        if any(role not in SEARCH_ROLES for role in roles):
            raise ValueError("unsupported transcript role filter")
        if offset < 0:
            raise ValueError("--offset must not be negative")
        if limit < 1:
            raise ValueError("--limit must be at least 1")
        selected_agent = self._validated_agent_filter(filters)
        matcher = _compile_search_matcher(
            needle,
            match_mode=match_mode,
            case_sensitive=case_sensitive,
        )
        candidates: list[tuple[int, int, str, dict[str, JsonValue], _TextMatch]] = []
        for record in self._iter_search_records(
            filters, bloom_terms=matcher.bloom_terms
        ):
            prompt_reference = _optional_string(record, "prompt_ref")
            reference = as_string(record.get("ref"), "search record.ref")
            record_type = as_string(
                record.get("record_type"), "search record.record_type"
            )
            text = as_string(record.get("text"), "search record.text")
            team = as_string(record.get("team"), "search record.team")
            if not self._selected_team(team, filters):
                continue
            at_ms = as_int(record.get("at_ms"), "search record.at_ms")
            if filters.window is not None and not filters.window.contains(at_ms):
                continue
            agent_reference = as_string(
                record.get("agent_ref"), "search record.agent_ref"
            )
            if selected_agent is not None and agent_reference != selected_agent:
                continue
            role = as_string(record.get("role"), "search record.role")
            if roles and role not in roles:
                continue
            author_kind = _optional_string(record, "author_kind")
            prompt_kind = _optional_string(record, "prompt_author_kind")
            if corpus == "owner-prompts" and not (
                record_type == "prompt" and author_kind == "owner_human"
            ):
                continue
            if corpus == "agent-responses" and record_type not in {
                "response",
                "inter_agent_response",
            }:
                continue
            selected_prompt_kind = (
                author_kind
                if record_type in {"prompt", "inter_agent_prompt"}
                else prompt_kind
            )
            if prompt_author == "owner" and selected_prompt_kind != "owner_human":
                continue
            if (
                prompt_author == "agent"
                and selected_prompt_kind not in _BOT_AUTHOR_KINDS
            ):
                continue
            if prompt_author == "unclassified" and selected_prompt_kind in (
                _HUMAN_AUTHOR_KINDS | _BOT_AUTHOR_KINDS
            ):
                continue
            text_match = matcher.match(text)
            if text_match is None:
                continue
            candidates.append(
                (text_match.score, at_ms, reference, dict(record), text_match)
            )

        candidate_prompt_refs = frozenset(
            reference
            for _score, _at_ms, reference, record, _text_match in candidates
            if as_string(record.get("record_type"), "search record.record_type")
            in {"prompt", "inter_agent_prompt"}
        )
        needed_prompt_refs = frozenset(
            prompt_reference
            for _score, _at_ms, reference, record, _text_match in candidates
            if (
                (prompt_reference := _optional_string(record, "prompt_ref")) is not None
                and prompt_reference != reference
            )
        )
        link_context = self._search_link_context(
            filters,
            selected_agent,
            candidate_prompt_refs,
            needed_prompt_refs,
        )
        response_counts = link_context.response_counts

        matches: list[tuple[int, int, str, dict[str, JsonValue]]] = []
        for score, at_ms, reference, record, text_match in candidates:
            record_type = as_string(
                record.get("record_type"), "search record.record_type"
            )
            prompt_reference = _optional_string(record, "prompt_ref")
            linked = (
                response_counts.get(reference, 0) > 0
                if record_type in {"prompt", "inter_agent_prompt"}
                else prompt_reference is not None and prompt_reference != reference
            )
            if linkage == "linked" and not linked:
                continue
            if linkage == "unlinked" and linked:
                continue
            team = as_string(record.get("team"), "search record.team")
            role = as_string(record.get("role"), "search record.role")
            agent_reference = as_string(
                record.get("agent_ref"), "search record.agent_ref"
            )
            prompt_kind = _optional_string(record, "prompt_author_kind")
            excerpt_details = _search_excerpt(text_match)
            excerpt = as_string(excerpt_details.get("text"), "search excerpt.text")
            item: dict[str, JsonValue] = {
                "result_id": reference,
                "ref": reference,
                "record_type": record_type,
                "role": role,
                "team": team,
                "agent_ref": agent_reference,
                "agent_path": record.get("agent_path"),
                "at_ms": at_ms,
                "at_time": _timestamp(at_ms),
                "prompt_ref": prompt_reference,
                "prompt_at_ms": record.get("prompt_at_ms"),
                "prompt_in_scope": record.get("prompt_in_scope"),
                "prompt_author_kind": prompt_kind,
                "linked_response_count": response_counts.get(reference, 0),
                "score": score,
                "ranking_version": "search-rank-v1",
                "content_fidelity": record.get("content_fidelity"),
                "excerpt": excerpt,
                "excerpt_details": excerpt_details,
            }
            phase_reference = self._phase_reference_for_search_record(record)
            if phase_reference is not None:
                item["phase_ref"] = phase_reference
            if prompt_reference is not None and prompt_reference != reference:
                prompt_excerpt = link_context.prompt_excerpts.get(prompt_reference)
                if prompt_excerpt is not None:
                    item["prompt_excerpt"] = prompt_excerpt
            matches.append((score, at_ms, reference, item))
        if sort == "relevance":
            matches.sort(key=lambda item: (-item[0], -item[1], item[2]))
        elif sort == "newest":
            matches.sort(key=lambda item: (-item[1], -item[0], item[2]))
        else:
            matches.sort(key=lambda item: (item[1], -item[0], item[2]))
        total_matches = len(matches)
        page = tuple(item for _, _, _, item in matches[offset : offset + limit])
        return SearchResults(
            page,
            total_matches,
            offset,
            limit,
            corpus,
            match_mode,
            sort,
        )

    @staticmethod
    def _contains(text: str, needle: str, case_sensitive: bool) -> bool:
        if case_sensitive:
            return needle in text
        return needle.casefold() in text.casefold()

    @staticmethod
    def _excerpt(text: str, needle: str, case_sensitive: bool) -> str:
        compact = _WHITESPACE.sub(" ", text).strip()
        haystack = compact if case_sensitive else compact.casefold()
        sought = needle if case_sensitive else needle.casefold()
        position = haystack.find(sought)
        if position < 0:
            return compact[:240]
        start = max(0, position - 100)
        end = min(len(compact), position + len(needle) + 140)
        prefix = "…" if start else ""
        suffix = "…" if end < len(compact) else ""
        return f"{prefix}{compact[start:end]}{suffix}"

    def _match(
        self,
        results: list[dict[str, JsonValue]],
        *,
        needle: str,
        case_sensitive: bool,
        reference: str,
        record_type: str,
        team: str,
        field: str,
        text: str,
        start_ms: int | None = None,
        at_ms: int | None = None,
    ) -> None:
        if not self._contains(text, needle, case_sensitive):
            return
        result: dict[str, JsonValue] = {
            "ref": reference,
            "record_type": record_type,
            "team": team,
            "field": field,
            "excerpt": self._excerpt(text, needle, case_sensitive),
        }
        if start_ms is not None:
            result["start_ms"] = start_ms
            result["start_time"] = _timestamp(start_ms)
        if at_ms is not None:
            result["at_ms"] = at_ms
            result["at_time"] = _timestamp(at_ms)
        results.append(result)

    def _search_summaries(
        self,
        needle: str,
        filters: QueryFilters,
        selected_agent: str | None,
        case_sensitive: bool,
    ) -> list[dict[str, JsonValue]]:
        results: list[dict[str, JsonValue]] = []
        for record in self._scoped("agents", filters):
            team = _team(record, "agent")
            reference = agent_ref(record)
            if not self._selected_team(team, filters):
                continue
            if selected_agent is not None and reference != selected_agent:
                continue
            if not _overlaps(record, reference, filters.window):
                continue
            start_ms, _end_ms = _interval(record, reference)
            fields = [
                "short_name",
                "official_name",
                "nickname",
            ]
            if _summary_available(record, "summary_available"):
                fields.extend(("lifetime_summary", "naming_rationale"))
            for field in fields:
                text = _optional_string(record, field)
                if text:
                    self._match(
                        results,
                        needle=needle,
                        case_sensitive=case_sensitive,
                        reference=reference,
                        record_type="agent",
                        team=team,
                        field=field,
                        text=text,
                        start_ms=start_ms,
                    )
        for record in self._scoped("phases", filters):
            team = _team(record, "phase")
            current_agent = self._agent_reference_for_id(
                team, as_string(record.get("agent_id"), "phase.agent_id")
            )
            if not self._selected_team(team, filters):
                continue
            if selected_agent is not None and current_agent != selected_agent:
                continue
            reference = phase_ref(record)
            if not _overlaps(record, reference, filters.window):
                continue
            if not _summary_available(record, "summary_available"):
                continue
            start_ms, _end_ms = _interval(record, reference)
            for field in ("phrase", "paragraph"):
                text = _optional_string(record, field)
                if text:
                    self._match(
                        results,
                        needle=needle,
                        case_sensitive=case_sensitive,
                        reference=reference,
                        record_type="phase",
                        team=team,
                        field=field,
                        text=text,
                        start_ms=start_ms,
                    )
        if selected_agent is None:
            results.extend(self._search_rollups(needle, filters, case_sensitive))
        return results

    def _search_rollups(
        self, needle: str, filters: QueryFilters, case_sensitive: bool
    ) -> list[dict[str, JsonValue]]:
        results: list[dict[str, JsonValue]] = []
        for record in self._scoped("rollups", filters):
            team = _team(record, "rollup")
            kind = as_string(record.get("kind"), "rollup.kind")
            reference = rollup_ref(record)
            if not self._selected_team(team, filters):
                continue
            if filters.rollup_kinds and kind not in filters.rollup_kinds:
                continue
            if not _overlaps(record, reference, filters.window):
                continue
            start_ms, _end_ms = _interval(record, reference)
            fields: list[tuple[str, str]] = []
            if _summary_available(record, "technical_summary_available"):
                fields.append(
                    (
                        "technical_markdown",
                        as_string(
                            record.get("technical_path"),
                            "rollup.technical_path",
                        ),
                    )
                )
            if _summary_available(record, "plain_language_summary_available"):
                fields.append(
                    (
                        "plain_language_markdown",
                        as_string(
                            record.get("plain_language_path"),
                            "rollup.plain_language_path",
                        ),
                    )
                )
            for field, relative in fields:
                text = self._read_markdown(self._rollup_file(relative, team))
                self._match(
                    results,
                    needle=needle,
                    case_sensitive=case_sensitive,
                    reference=reference,
                    record_type="rollup",
                    team=team,
                    field=field,
                    text=text,
                    start_ms=start_ms,
                )
        return results

    def _search_transcripts(
        self,
        needle: str,
        filters: QueryFilters,
        selected_agent: str | None,
        case_sensitive: bool,
    ) -> list[dict[str, JsonValue]]:
        results: list[dict[str, JsonValue]] = []
        for record in self._scoped("phases", filters):
            team = _team(record, "phase")
            current_agent = self._agent_reference_for_id(
                team, as_string(record.get("agent_id"), "phase.agent_id")
            )
            if not self._selected_team(team, filters):
                continue
            if selected_agent is not None and current_agent != selected_agent:
                continue
            reference = phase_ref(record)
            if not _overlaps(record, reference, filters.window):
                continue
            relative = as_string(record.get("detail_path"), "phase.detail_path")
            detail = as_object(self._read_json(self._detail_file(relative)), relative)
            transcript = as_array(detail.get("transcript"), f"{relative}.transcript")
            for index, raw_message in enumerate(transcript):
                message = as_object(raw_message, f"{relative}.transcript[{index}]")
                at_ms = as_int(
                    message.get("at_ms"),
                    f"{relative}.transcript[{index}].at_ms",
                )
                if filters.window is not None and not filters.window.contains(at_ms):
                    continue
                text = as_string(
                    message.get("text"), f"{relative}.transcript[{index}].text"
                )
                if not self._contains(text, needle, case_sensitive):
                    continue
                role = as_string(
                    message.get("role"),
                    f"{relative}.transcript[{index}].role",
                )
                self._match(
                    results,
                    needle=needle,
                    case_sensitive=case_sensitive,
                    reference=reference,
                    record_type="transcript",
                    team=team,
                    field=role,
                    text=text,
                    at_ms=at_ms,
                )
        return results


@dataclass(frozen=True)
class ArchiveStats:
    """Content and summary-coverage totals for one filtered archive view."""

    human_prompts: TextTotals
    bot_prompts: TextTotals
    unattributed_prompts: TextTotals
    linked_responses: TextTotals
    unlinked_responses: TextTotals
    summary_kinds: dict[str, SummaryKindStats]
    teams: tuple[str, ...]
    time_filtered: bool
    rollup_kinds: tuple[str, ...]

    @property
    def total_responses(self) -> TextTotals:
        """Return linked and unlinked response totals."""

        return self.linked_responses + self.unlinked_responses

    @property
    def all_prompts(self) -> TextTotals:
        """Return human, bot, and unattributed prompt totals."""

        return self.human_prompts + self.bot_prompts + self.unattributed_prompts

    @property
    def generated_summaries(self) -> TextTotals:
        """Return content totals across every summary surface."""

        total = TextTotals()
        for item in self.summary_kinds.values():
            total += item.content
        return total

    @property
    def prompts_and_responses(self) -> TextTotals:
        """Return all prompt and response content totals."""

        return self.all_prompts + self.total_responses

    @property
    def all_counted_content(self) -> TextTotals:
        """Return prompts, responses, and generated summaries together."""

        return self.prompts_and_responses + self.generated_summaries

    def to_mapping(self) -> dict[str, JsonValue]:
        """Return the full accounting report as a JSON-compatible object."""

        content: dict[str, JsonValue] = {
            "human_prompts": self.human_prompts.to_mapping(),
            "bot_prompts": self.bot_prompts.to_mapping(),
            "unattributed_prompts": self.unattributed_prompts.to_mapping(),
            "all_prompts": self.all_prompts.to_mapping(),
            "mechanically_linked_responses": self.linked_responses.to_mapping(),
            "unlinked_responses": self.unlinked_responses.to_mapping(),
            "total_responses": self.total_responses.to_mapping(),
            "generated_summaries": self.generated_summaries.to_mapping(),
            "prompts_and_responses": self.prompts_and_responses.to_mapping(),
            "all_counted_content": self.all_counted_content.to_mapping(),
        }
        by_kind: dict[str, JsonValue] = {
            key: value.to_mapping() for key, value in self.summary_kinds.items()
        }
        available = sum(item.available for item in self.summary_kinds.values())
        unavailable = sum(item.unavailable for item in self.summary_kinds.values())
        scope: dict[str, JsonValue] = {
            "teams": list(self.teams),
            "all_teams": not self.teams,
            "time_filtered": self.time_filtered,
            "rollup_kinds": list(self.rollup_kinds),
        }
        return {
            "schema_version": QUERY_SCHEMA_VERSION,
            "command": "stats",
            "scope": scope,
            "content": content,
            "summary_availability": {
                "available": available,
                "unavailable": unavailable,
                "total": available + unavailable,
                "by_kind": by_kind,
            },
            "counting_contract": {
                "words": "Unicode text split on whitespace",
                "utf8_bytes": "UTF-8 encoded text only; serialization framing excluded",
                "rollup_text": "referenced generated Markdown counted verbatim",
                "phase_summary_text": "phrase plus newline plus paragraph",
                "time_unbounded_project_overviews": (
                    "included only when no time filter is active"
                ),
                "human_prompts": "records labeled owner_human or other_human",
                "bot_prompts": "records labeled agent or system",
                "unattributed_prompts": "all remaining prompt authorship labels",
            },
        }


def archive_stats(root: Path, filters: QueryFilters) -> ArchiveStats:
    """Read verified projections and compute zero-model archive content totals."""

    transcripts = TranscriptQuery(root)
    (
        human_prompts,
        bot_prompts,
        unattributed_prompts,
        linked_responses,
        unlinked_responses,
    ) = transcripts.content_stats(filters)
    summaries = TimelineQuery(root).summary_stats(filters)
    return ArchiveStats(
        human_prompts=human_prompts,
        bot_prompts=bot_prompts,
        unattributed_prompts=unattributed_prompts,
        linked_responses=linked_responses,
        unlinked_responses=unlinked_responses,
        summary_kinds=summaries,
        teams=filters.teams,
        time_filtered=filters.window is not None,
        rollup_kinds=filters.rollup_kinds,
    )


def query_envelope(
    command: str, items: list[dict[str, JsonValue]]
) -> dict[str, JsonValue]:
    """Wrap records in a small versioned response for default JSON output."""

    widened_items: list[JsonValue] = [item for item in items]
    return {
        "schema_version": QUERY_SCHEMA_VERSION,
        "command": command,
        "count": len(items),
        "items": widened_items,
    }


def _markdown_title(item: dict[str, JsonValue]) -> str:
    for key in ("short_name", "label", "phrase", "slug", "ref"):
        value = _optional_string(item, key)
        if value:
            return value.replace("\n", " ")
    return "Result"


def _markdown_item(item: dict[str, JsonValue]) -> str:
    lines = [f"## {_markdown_title(item)}", ""]
    for key in (
        "ref",
        "record_type",
        "team",
        "provider",
        "kind",
        "agent_ref",
        "parent_ref",
        "start_time",
        "end_time",
        "at_time",
        "field",
        "summary_available",
        "technical_summary_available",
        "plain_language_summary_available",
    ):
        value = item.get(key)
        if isinstance(value, (str, int, float, bool)):
            lines.append(f"- {key.replace('_', ' ').title()}: `{value}`")
    lines.append("")
    for key in (
        "lifetime_summary",
        "paragraph",
        "excerpt",
        "technical_markdown",
        "plain_language_markdown",
    ):
        text = _optional_string(item, key)
        if not text:
            continue
        if key in {"technical_markdown", "plain_language_markdown"}:
            lines.extend((f"### {key.replace('_', ' ').title()}", ""))
        lines.extend((text.rstrip(), ""))
    detail_value = item.get("detail")
    if isinstance(detail_value, dict):
        transcript_value = detail_value.get("transcript")
        if isinstance(transcript_value, list):
            lines.extend(("### Transcript", ""))
            for index, raw_message in enumerate(transcript_value):
                message = as_object(raw_message, f"detail.transcript[{index}]")
                role = as_string(
                    message.get("role"), f"detail.transcript[{index}].role"
                )
                at_ms = as_int(
                    message.get("at_ms"), f"detail.transcript[{index}].at_ms"
                )
                text = as_string(
                    message.get("text"), f"detail.transcript[{index}].text"
                )
                lines.extend(
                    (f"#### {role} · {_timestamp(at_ms)}", "", text.rstrip(), "")
                )
    return "\n".join(lines).rstrip()


def _text_item(item: dict[str, JsonValue]) -> str:
    record_type = _optional_string(item, "record_type") or "record"
    ordinal = _optional_int(item, "ordinal")
    if ordinal is None:
        ordinal = _optional_int(item, "prompt_ordinal")
    timestamp = (
        _optional_string(item, "timestamp_local")
        or _optional_string(item, "at_time")
        or _optional_string(item, "start_time")
        or "unknown time"
    )
    team = (
        _optional_string(item, "team_slug")
        or _optional_string(item, "team")
        or "unknown team"
    )
    number = f" #{ordinal}" if ordinal is not None else ""
    header = f"[{record_type}{number} · {timestamp} · {team}]"
    body = (
        _optional_string(item, "text")
        or _optional_string(item, "excerpt")
        or _optional_string(item, "paragraph")
        or canonical_json(item).rstrip()
    )
    return f"{header}\n{body.rstrip()}"


def format_query(
    command: str, items: list[dict[str, JsonValue]], output_format: str
) -> str:
    """Render query records as JSON, JSONL, Markdown, or scan-friendly plain text."""

    if output_format == "json":
        return canonical_json(query_envelope(command, items))
    if output_format == "jsonl":
        return "".join(
            json.dumps(item, ensure_ascii=False, sort_keys=True) + "\n"
            for item in items
        )
    if output_format == "markdown":
        body = "\n\n".join(_markdown_item(item) for item in items)
        suffix = f"\n\n_{len(items)} result(s)._\n"
        return f"# wrkviz query: {command}\n\n{body}{suffix}"
    if output_format == "text":
        body = "\n\n".join(_text_item(item) for item in items)
        return body + ("\n" if body else "")
    raise ValueError(f"unsupported query output format {output_format!r}")


def format_search_results(
    command: str, results: SearchResults, output_format: str
) -> str:
    """Render a search-v2 page while preserving total/truncation metadata."""

    items = list(results.items)
    if output_format == "json":
        values: list[JsonValue] = [item for item in items]
        return canonical_json(
            {
                "schema_version": QUERY_SCHEMA_VERSION,
                "search_schema_version": 1,
                "command": command,
                "corpus": results.corpus,
                "match_mode": results.match_mode,
                "sort": results.sort,
                "total_matches": results.total_matches,
                "offset": results.offset,
                "returned": len(items),
                "truncated": results.truncated,
                "items": values,
            }
        )
    if output_format == "jsonl":
        return "".join(
            json.dumps(item, ensure_ascii=False, sort_keys=True) + "\n"
            for item in items
        )
    if output_format == "markdown":
        body = "\n\n".join(_markdown_item(item) for item in items)
        status = (
            f"Showing {len(items)} of {results.total_matches} result(s) "
            f"from offset {results.offset}."
        )
        return f"# wrkviz query: {command}\n\n{body}\n\n_{status}_\n"
    if output_format == "text":
        status = (
            f"Showing {len(items)} of {results.total_matches} result(s) "
            f"from offset {results.offset}."
        )
        body = "\n\n".join(_text_item(item) for item in items)
        return status + "\n\n" + body + ("\n" if body else "")
    raise ValueError(f"unsupported query output format {output_format!r}")


def _stats_content_rows(stats: ArchiveStats) -> tuple[tuple[str, TextTotals], ...]:
    return (
        ("Identified human prompts", stats.human_prompts),
        ("Identified bot/agent prompts", stats.bot_prompts),
        ("Unattributed prompt records", stats.unattributed_prompts),
        ("All prompt records", stats.all_prompts),
        ("Mechanically linked responses", stats.linked_responses),
        ("Unlinked responses", stats.unlinked_responses),
        ("Total responses", stats.total_responses),
        ("Generated summaries", stats.generated_summaries),
        ("Prompts + responses", stats.prompts_and_responses),
        ("All counted content", stats.all_counted_content),
    )


def _stats_scope(stats: ArchiveStats) -> str:
    teams = ", ".join(stats.teams) if stats.teams else "all teams"
    time_scope = "filtered time window" if stats.time_filtered else "all time"
    rollups = (
        ", ".join(stats.rollup_kinds) if stats.rollup_kinds else "all rollup kinds"
    )
    return f"{teams}; {time_scope}; {rollups}"


def _format_stats_text(stats: ArchiveStats) -> str:
    lines = [
        "Archive content statistics",
        f"Scope: {_stats_scope(stats)}",
        "",
        f"{'Content':<34} {'Records':>12} {'Words':>14} {'UTF-8 bytes':>16}",
        f"{'-' * 34} {'-' * 12} {'-' * 14} {'-' * 16}",
    ]
    for label, totals in _stats_content_rows(stats):
        lines.append(
            f"{label:<34} {totals.records:>12,} {totals.words:>14,} "
            f"{totals.utf8_bytes:>16,}"
        )
    lines.extend(
        (
            "",
            "Summary availability",
            f"{'Kind':<34} {'Available':>12} {'Unavailable':>14} {'Total':>16}",
            f"{'-' * 34} {'-' * 12} {'-' * 14} {'-' * 16}",
        )
    )
    for key, item in stats.summary_kinds.items():
        label = key.replace("_", " ").title()
        lines.append(
            f"{label:<34} {item.available:>12,} {item.unavailable:>14,} "
            f"{item.available + item.unavailable:>16,}"
        )
    available = sum(item.available for item in stats.summary_kinds.values())
    unavailable = sum(item.unavailable for item in stats.summary_kinds.values())
    lines.append(
        f"{'All summary slots':<34} {available:>12,} {unavailable:>14,} "
        f"{available + unavailable:>16,}"
    )
    lines.extend(
        (
            "",
            "Words are whitespace-delimited Unicode text. Byte totals are UTF-8 text "
            "bytes and exclude CLI/JSON serialization framing; generated rollup Markdown "
            "is counted verbatim.",
        )
    )
    return "\n".join(lines) + "\n"


def _format_stats_markdown(stats: ArchiveStats) -> str:
    lines = [
        "# Archive content statistics",
        "",
        f"Scope: {_stats_scope(stats)}.",
        "",
        "| Content | Records | Words | UTF-8 bytes |",
        "| --- | ---: | ---: | ---: |",
    ]
    for label, totals in _stats_content_rows(stats):
        lines.append(
            f"| {label} | {totals.records:,} | {totals.words:,} | "
            f"{totals.utf8_bytes:,} |"
        )
    lines.extend(
        (
            "",
            "## Summary availability",
            "",
            "| Kind | Available | Unavailable | Total |",
            "| --- | ---: | ---: | ---: |",
        )
    )
    for key, item in stats.summary_kinds.items():
        label = key.replace("_", " ").title()
        lines.append(
            f"| {label} | {item.available:,} | {item.unavailable:,} | "
            f"{item.available + item.unavailable:,} |"
        )
    available = sum(item.available for item in stats.summary_kinds.values())
    unavailable = sum(item.unavailable for item in stats.summary_kinds.values())
    lines.extend(
        (
            f"| **All summary slots** | **{available:,}** | **{unavailable:,}** | "
            f"**{available + unavailable:,}** |",
            "",
            "Words are whitespace-delimited Unicode text. Byte totals are UTF-8 text "
            "bytes and exclude CLI/JSON serialization framing; generated rollup Markdown "
            "is counted verbatim.",
        )
    )
    return "\n".join(lines) + "\n"


def format_stats(stats: ArchiveStats, output_format: str) -> str:
    """Render content statistics without entering the generic record-list envelope."""

    if output_format == "json":
        return canonical_json(stats.to_mapping())
    if output_format == "jsonl":
        return json.dumps(stats.to_mapping(), ensure_ascii=False, sort_keys=True) + "\n"
    if output_format == "markdown":
        return _format_stats_markdown(stats)
    if output_format == "text":
        return _format_stats_text(stats)
    raise ValueError(f"unsupported query output format {output_format!r}")


@dataclass(frozen=True)
class _StandaloneWindow:
    start_ms: int | None
    end_ms: int | None

    def contains(self, timestamp_ms: int) -> bool:
        if self.start_ms is not None and timestamp_ms < self.start_ms:
            return False
        return self.end_ms is None or timestamp_ms < self.end_ms

    def overlaps(self, start_ms: int, end_ms: int | None) -> bool:
        effective_end = end_ms if end_ms is not None else start_ms + 1
        if effective_end <= start_ms:
            effective_end = start_ms + 1
        if self.end_ms is not None and start_ms >= self.end_ms:
            return False
        return self.start_ms is None or effective_end > self.start_ms


def _parse_instant(raw: str, label: str) -> int:
    if _RFC3339_INSTANT.fullmatch(raw) is None:
        raise ValueError(
            f"{label} must be an RFC3339 timestamp with an explicit offset or Z"
        )
    normalized = raw[:-1] + "+00:00" if raw.endswith("Z") else raw
    try:
        value = datetime.fromisoformat(normalized)
    except ValueError as error:
        raise ValueError(
            f"{label} must be an RFC3339 timestamp with an explicit offset or Z"
        ) from error
    if value.tzinfo is None or value.utcoffset() is None:
        raise ValueError(f"{label} must include an explicit offset or Z")
    return int(value.timestamp() * 1000)


def _standalone_add_filters(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--team",
        action="append",
        default=[],
        metavar="SLUG",
        help="include one team slug; repeat to include multiple teams",
    )
    parser.add_argument(
        "--start-time",
        metavar="RFC3339",
        help="include records at or after this timestamp",
    )
    parser.add_argument(
        "--end-time",
        metavar="RFC3339",
        help="exclude records at or after this timestamp",
    )
    parser.add_argument(
        "--kind",
        action="append",
        choices=("hourly", "daily", "weekly", "monthly", "quarterly"),
        default=[],
        help="include one rollup kind; repeat to include several",
    )
    parser.add_argument(
        "--agent",
        metavar="REF",
        help="restrict results to one canonical agent:TEAM::ID reference",
    )


def _standalone_add_transcript_filters(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--team",
        action="append",
        default=[],
        metavar="SLUG",
        help="include one team slug; repeat to include multiple teams",
    )
    parser.add_argument(
        "--start-time",
        metavar="RFC3339",
        help="include records at or after this timestamp",
    )
    parser.add_argument(
        "--end-time",
        metavar="RFC3339",
        help="exclude records at or after this timestamp",
    )


def _standalone_add_paging(parser: argparse.ArgumentParser) -> None:
    """Add the two paging flags and the integrity escape hatch to a transcript command."""

    parser.add_argument(
        "--limit",
        type=int,
        default=None,
        metavar="N",
        help="return at most N records from the start of the selection",
    )
    parser.add_argument(
        "--tail",
        type=int,
        default=None,
        metavar="N",
        help="return the last N records of the selection",
    )
    parser.add_argument(
        "--verify",
        action="store_true",
        help=(
            "re-read every consulted projection end to end and reproduce its manifest "
            "digest, and resolve prompt/response linkage by full scan instead of by seek"
        ),
    )


def _standalone_paging(ns: argparse.Namespace) -> tuple[int | None, int | None]:
    raw_limit: object = getattr(ns, "limit", None)
    raw_tail: object = getattr(ns, "tail", None)
    if raw_limit is not None and not isinstance(raw_limit, int):
        raise ValueError("--limit must be an integer")
    if raw_tail is not None and not isinstance(raw_tail, int):
        raise ValueError("--tail must be an integer")
    return raw_limit, raw_tail


def _standalone_add_common_options(
    parser: argparse.ArgumentParser, default_format: str
) -> None:
    parser.add_argument(
        "--archive",
        "--output",
        dest="command_archive",
        default=None,
        metavar="PATH",
        help="read a different archive instead of the one containing this executable",
    )
    parser.add_argument(
        "--format",
        dest="command_format",
        choices=("json", "jsonl", "markdown", "text"),
        default=None,
        help=f"output format (default: {default_format})",
    )
    parser.set_defaults(default_format=default_format)


def _standalone_parser() -> argparse.ArgumentParser:
    program = Path(sys.argv[0]).name
    parser = argparse.ArgumentParser(
        prog=program,
        description=(
            "Read prompts, responses, agents, and summaries from this "
            "agent-team timeline archive."
        ),
        epilog=(
            "examples:\n"
            "  ./timeline prompts\n"
            "  ./timeline prompts --range 200-300\n"
            "  ./timeline prompts --which all --format jsonl > all-prompts.jsonl\n"
            "  ./timeline prompts --format jsonl > prompts.jsonl\n"
            "  ./timeline messages --range 200-300\n"
            "  ./timeline stats\n"
            "  ./timeline agents --team codex-coord-030\n"
            "  ./timeline search 'reproducible build' --scope all\n\n"
            "Prompt ranges are 1-based and inclusive. The default archive is the "
            "directory containing this executable."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--archive",
        "--output",
        dest="global_archive",
        default=str(Path(__file__).resolve().parent),
        metavar="PATH",
        help="archive directory (default: directory containing this executable)",
    )
    parser.add_argument(
        "--format",
        dest="global_format",
        choices=("json", "jsonl", "markdown", "text"),
        default=None,
        help=argparse.SUPPRESS,
    )
    parser.add_argument(
        "--version",
        action="version",
        version=f"%(prog)s archive-query schema {QUERY_SCHEMA_VERSION}",
    )
    sub = parser.add_subparsers(dest="action", required=True, metavar="COMMAND")

    for resource, help_text in (
        ("teams", "list teams represented in the archive"),
        ("agents", "list coordinator and subagent lifetimes"),
        ("phases", "list summarized agent work phases"),
        ("rollups", "list hourly, daily, weekly, and longer summaries"),
    ):
        resource_parser = sub.add_parser(
            resource, help=help_text, description=help_text
        )
        _standalone_add_filters(resource_parser)
        _standalone_add_common_options(resource_parser, "json")

    prompts = sub.add_parser(
        "prompts",
        help="print chronological verbatim prompts",
        description=(
            "Print chronological verbatim prompts across the selected teams. "
            "Human-authored prompts are the default."
        ),
        epilog=(
            "examples:\n"
            "  ./timeline prompts\n"
            "  ./timeline prompts --range 200-300\n"
            "  ./timeline prompts --tail 20\n"
            "  ./timeline prompts --which all --format jsonl\n"
            "  ./timeline prompts --team orc-coord-014 --format jsonl"
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    prompts.add_argument(
        "--range",
        dest="ordinal_range",
        metavar="N[-M]",
        help="one prompt ordinal or an inclusive range, for example 200-300",
    )
    prompts.add_argument(
        "--which",
        choices=PROMPT_SELECTIONS,
        default="human",
        help="select human, bot, or all prompt authorship (default: %(default)s)",
    )
    _standalone_add_paging(prompts)
    _standalone_add_transcript_filters(prompts)
    _standalone_add_common_options(prompts, "text")

    messages = sub.add_parser(
        "messages",
        help="print prompts and their mechanically linked responses",
        description=(
            "Print chronological prompts plus coordinator responses mechanically "
            "associated with the selected prompts. Human-authored prompts are the default."
        ),
    )
    messages.add_argument(
        "--range",
        dest="ordinal_range",
        metavar="N[-M]",
        help="one prompt ordinal or an inclusive range, for example 200-300",
    )
    messages.add_argument(
        "--which",
        choices=PROMPT_SELECTIONS,
        default="human",
        help="select human, bot, or all prompt authorship (default: %(default)s)",
    )
    _standalone_add_paging(messages)
    _standalone_add_transcript_filters(messages)
    _standalone_add_common_options(messages, "text")

    stats = sub.add_parser(
        "stats",
        help="count prompt, response, and generated-summary text",
        description=(
            "Count records, whitespace-delimited words, and UTF-8 text bytes for "
            "owner prompts, responses, and generated summaries. This is a read-only, "
            "zero-model operation."
        ),
        epilog=(
            "examples:\n"
            "  ./timeline stats\n"
            "  ./timeline stats --team codex-coord-030\n"
            "  ./timeline stats --start-time 2026-08-11T00:00:00-04:00 "
            "--end-time 2026-08-12T00:00:00-04:00\n"
            "  ./timeline stats --format json"
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    _standalone_add_transcript_filters(stats)
    stats.add_argument(
        "--kind",
        action="append",
        choices=("hourly", "daily", "weekly", "monthly", "quarterly"),
        default=[],
        help="count one rollup kind; repeat to include several",
    )
    _standalone_add_common_options(stats, "text")

    listing = sub.add_parser(
        "list",
        help="compatibility form of teams/agents/phases/rollups",
        description="List one class of timeline record.",
    )
    listing.add_argument(
        "resource",
        choices=("teams", "agents", "phases", "rollups"),
        help="record class to list",
    )
    _standalone_add_filters(listing)
    _standalone_add_common_options(listing, "json")

    showing = sub.add_parser(
        "show",
        help="resolve one stable timeline or transcript reference",
        description="Show one record selected by a stable reference from a list or search.",
    )
    showing.add_argument(
        "reference",
        help="team:, agent:, phase:, rollup:, message:, or tool: reference",
    )
    showing.add_argument(
        "--transcript",
        action="store_true",
        help="include condensed transcript messages for a work phase",
    )
    _standalone_add_common_options(showing, "json")

    searching = sub.add_parser(
        "search",
        help="search prompts, responses, full transcript text, and summaries",
        description=(
            "Search the canonical transcript corpus with stable message references, or use "
            "--scope for the compatibility phase-transcript search."
        ),
    )
    searching.add_argument("text", help="text or pattern to find")
    searching.add_argument(
        "--in",
        dest="search_corpus",
        choices=SEARCH_CORPORA,
        help="search-v2 corpus: owner prompts, agent responses, or all transcript",
    )
    searching.add_argument(
        "--scope",
        choices=("summaries", "transcripts", "all"),
        default=None,
        help="compatibility search scope (default when --in is absent: summaries)",
    )
    searching.add_argument("--match", choices=SEARCH_MATCH_MODES, default=None)
    searching.add_argument("--sort", choices=SEARCH_SORTS, default=None)
    searching.add_argument(
        "--prompt-author", choices=SEARCH_PROMPT_AUTHORS, default=None
    )
    searching.add_argument("--linkage", choices=SEARCH_LINKAGES, default=None)
    searching.add_argument("--role", action="append", choices=SEARCH_ROLES, default=[])
    searching.add_argument(
        "--case-sensitive", action="store_true", help="preserve letter case"
    )
    searching.add_argument("--offset", type=int, default=None)
    searching.add_argument(
        "--limit", type=int, default=50, help="maximum matches (default: %(default)s)"
    )
    _standalone_add_filters(searching)
    _standalone_add_common_options(searching, "json")
    return parser


def _standalone_output_format(ns: argparse.Namespace) -> str:
    command_format: object = getattr(ns, "command_format", None)
    global_format: object = getattr(ns, "global_format", None)
    default_format: object = getattr(ns, "default_format", "json")
    selected = command_format or global_format or default_format
    if not isinstance(selected, str) or selected not in {
        "json",
        "jsonl",
        "markdown",
        "text",
    }:
        raise ValueError("output format must be json, jsonl, markdown, or text")
    return selected


def _standalone_filters(ns: argparse.Namespace) -> QueryFilters:
    raw_teams: object = ns.team
    raw_kinds: object = getattr(ns, "kind", [])
    raw_agent: object = getattr(ns, "agent", None)
    if not isinstance(raw_teams, list) or not all(
        isinstance(item, str) for item in raw_teams
    ):
        raise ValueError("--team values must be strings")
    if not isinstance(raw_kinds, list) or not all(
        isinstance(item, str) for item in raw_kinds
    ):
        raise ValueError("--kind values must be strings")
    if raw_agent is not None and not isinstance(raw_agent, str):
        raise ValueError("--agent must be a string")
    start_ms = (
        _parse_instant(str(ns.start_time), "start time")
        if ns.start_time is not None
        else None
    )
    end_ms = (
        _parse_instant(str(ns.end_time), "end time")
        if ns.end_time is not None
        else None
    )
    if start_ms is not None and end_ms is not None and start_ms >= end_ms:
        raise ValueError("start bound must be earlier than end bound")
    window = (
        None
        if start_ms is None and end_ms is None
        else _StandaloneWindow(start_ms, end_ms)
    )
    return QueryFilters(tuple(raw_teams), window, tuple(raw_kinds), raw_agent)


def _standalone_main(argv: Sequence[str] | None = None) -> int:
    parser = _standalone_parser()
    ns = parser.parse_args(list(argv) if argv is not None else None)
    try:
        command_archive: object = getattr(ns, "command_archive", None)
        global_archive: object = getattr(ns, "global_archive", None)
        if command_archive is not None and not isinstance(command_archive, str):
            raise ValueError("--archive must be a path")
        if not isinstance(global_archive, str):
            raise ValueError("--archive must be a path")
        output = Path(command_archive or global_archive).expanduser()
        action = str(ns.action)
        if action in {"teams", "agents", "phases", "rollups"}:
            query = TimelineQuery(output)
            command = f"list {action}"
            items = query.list_records(action, _standalone_filters(ns))
        elif action == "list":
            query = TimelineQuery(output)
            resource = str(ns.resource)
            command = f"list {resource}"
            items = query.list_records(resource, _standalone_filters(ns))
        elif action == "show":
            query = TimelineQuery(output)
            reference = str(ns.reference)
            command = f"show {reference}"
            items = [query.show(reference, transcript=bool(ns.transcript))]
        elif action == "search":
            query = TimelineQuery(output)
            needle = str(ns.text)
            command = f"search {needle}"
            raw_corpus: object = ns.search_corpus
            raw_scope: object = ns.scope
            if raw_corpus is not None:
                if raw_scope is not None:
                    raise ValueError("--in and --scope cannot be combined")
                raw_roles: object = ns.role
                if not isinstance(raw_roles, list) or not all(
                    isinstance(role, str) for role in raw_roles
                ):
                    raise ValueError("--role values must be strings")
                search_results = query.search_v2(
                    needle,
                    corpus=str(raw_corpus),
                    filters=_standalone_filters(ns),
                    case_sensitive=bool(ns.case_sensitive),
                    match_mode=str(ns.match or "smart"),
                    sort=str(ns.sort or "relevance"),
                    prompt_author=str(ns.prompt_author or "any"),
                    linkage=str(ns.linkage or "any"),
                    roles=tuple(raw_roles),
                    offset=int(ns.offset or 0),
                    limit=int(ns.limit),
                )
                print(
                    format_search_results(
                        command, search_results, _standalone_output_format(ns)
                    ),
                    end="",
                )
                return 0
            if (
                ns.match is not None
                or ns.sort is not None
                or ns.prompt_author is not None
                or ns.linkage is not None
                or ns.offset is not None
                or ns.role
            ):
                raise ValueError("search-v2 options require --in")
            items = query.search(
                needle,
                scope=str(raw_scope or "summaries"),
                filters=_standalone_filters(ns),
                case_sensitive=bool(ns.case_sensitive),
                limit=int(ns.limit),
            )
        elif action in {"prompts", "messages"}:
            limit, tail = _standalone_paging(ns)
            _check_page(limit, tail)
            query_transcripts = TranscriptQuery(output, verify=bool(ns.verify))
            raw_range: object = ns.ordinal_range
            ordinal_range = (
                parse_ordinal_range(raw_range) if isinstance(raw_range, str) else None
            )
            command = action
            items = (
                query_transcripts.list_prompts(
                    _standalone_filters(ns),
                    ordinal_range,
                    str(ns.which),
                    limit=limit,
                    tail=tail,
                )
                if action == "prompts"
                else query_transcripts.list_messages(
                    _standalone_filters(ns),
                    ordinal_range,
                    str(ns.which),
                    limit=limit,
                    tail=tail,
                )
            )
        elif action == "stats":
            stats_result = archive_stats(output, _standalone_filters(ns))
            print(
                format_stats(stats_result, _standalone_output_format(ns)),
                end="",
            )
            return 0
        else:
            raise ValueError(f"unsupported query action {action!r}")
        print(format_query(command, items, _standalone_output_format(ns)), end="")
        return 0
    except (OSError, ValueError) as error:
        print(f"{Path(sys.argv[0]).name}: {error}", file=sys.stderr)
        return 2


__all__ = [
    "ArchiveStats",
    "OrdinalRange",
    "QueryFilters",
    "SEARCH_CORPORA",
    "SEARCH_LINKAGES",
    "SEARCH_MATCH_MODES",
    "SEARCH_PROMPT_AUTHORS",
    "SEARCH_ROLES",
    "SEARCH_SORTS",
    "SearchResults",
    "TimelineQuery",
    "TranscriptQuery",
    "archive_stats",
    "agent_ref",
    "format_query",
    "format_search_results",
    "format_stats",
    "phase_ref",
    "parse_ordinal_range",
    "query_envelope",
    "rollup_ref",
    "schema_3_completeness",
    "schema_3_record_counts",
    "team_ref",
]


if __name__ == "__main__":
    raise SystemExit(_standalone_main())
