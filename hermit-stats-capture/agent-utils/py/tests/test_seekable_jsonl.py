"""Contract tests for the chunked, seekable JSONL codec.

Two properties are load-bearing and are therefore asserted on bytes rather than on behaviour:

*The container is an ordinary gzip file.* Every round trip is checked against `gzip.open` and
`gzip.decompress` on the same path, and against the system `gzip -t` where one exists, because the
whole reason for choosing multi-member gzip over a smaller codec is that losing the index leaves
something every tool already reads.

*Seeking is O(result).* The seek tests count bytes read from the `.gz` and compare them to the
file's size. Timing would prove nothing here -- a fast machine makes a full scan look like a seek
-- so `SeekableJsonlReader.data_bytes_read` is the instrument, and the assertions are exact where
the answer is exact (tail touches precisely the final member) rather than merely "less than".
"""

from __future__ import annotations

import gzip
import hashlib
import json
import shutil
import subprocess
from collections.abc import Iterator
from pathlib import Path

import pytest

from wrkviz.archive import JsonValue
from wrkviz.seekable_jsonl import (
    DEFAULT_TARGET_CHUNK_BYTES,
    INDEX_FORMAT,
    ChunkIndex,
    IndexMismatchError,
    SeekableJsonlError,
    SeekableJsonlReader,
    TruncatedArchiveError,
    UnsupportedCodecError,
    index_path_for,
    rebuild_index,
    write_rebuilt_index,
    write_seekable_jsonl,
    write_seekable_jsonl_lines,
)


_TARGET = 16 * 1024


def _filler(index: int, length: int) -> str:
    """Deterministic text with realistic entropy.

    Repetitive filler ("the quick brown fox" over and over) would compress ~30:1 and make every
    ratio in this file a lie: the compressed archive would be so much smaller than the real one
    that a single member would look like a tenth of the file. Hex from a counter-seeded digest
    compresses at roughly 2:1, which brackets the real transcript's 4.7:1 from the pessimistic
    side, and it is reproducible across runs and hosts.
    """

    out: list[str] = []
    seed = index
    while sum(len(part) for part in out) < length:
        out.append(hashlib.sha256(f"{index}:{seed}".encode("utf-8")).hexdigest())
        seed += 1
    return "".join(out)[:length]


def _record(index: int, *, at: int | None = None, filler: int = 160) -> dict[str, JsonValue]:
    record: dict[str, JsonValue] = {
        "ordinal": index,
        "record_id": f"record-{index:06d}",
        "text": _filler(index, filler),
    }
    if at is not None:
        record["timestamp_ms"] = at
    return record


def _records(count: int, *, start_ms: int = 1_700_000_000_000, step: int = 1000) -> list[
    dict[str, JsonValue]
]:
    return [_record(index, at=start_ms + index * step) for index in range(count)]


def _encoded(records: list[dict[str, JsonValue]]) -> bytes:
    return b"".join(
        json.dumps(record, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode(
            "utf-8"
        )
        + b"\n"
        for record in records
    )


@pytest.fixture(scope="module")
def large_archive(
    tmp_path_factory: pytest.TempPathFactory,
) -> tuple[Path, list[dict[str, JsonValue]]]:
    """A 50,000-record archive at the production chunk size, built once for the whole module.

    Fifty thousand is the floor the measurement is required to clear; at ~250 bytes a record it
    also produces a file large enough (>12 MB) that a full scan and a seek are separated by three
    orders of magnitude, which is the separation the O(result) claim is about.
    """

    root = tmp_path_factory.mktemp("seekable-jsonl-large")
    path = root / "messages.jsonl.gz"
    records = _records(50_000)
    write_seekable_jsonl(path, records, target_chunk_bytes=DEFAULT_TARGET_CHUNK_BYTES)
    return path, records


def test_round_trip_is_byte_identical(tmp_path: Path) -> None:
    """Chunking must not change one byte of the uncompressed stream."""

    records = _records(2_000)
    path = tmp_path / "messages.jsonl.gz"
    report = write_seekable_jsonl(path, records, target_chunk_bytes=_TARGET)
    expected = _encoded(records)

    assert report.member_count > 1, "test is meaningless unless the file really is chunked"
    assert gzip.decompress(path.read_bytes()) == expected
    assert hashlib.sha256(expected).hexdigest() == report.index.u_sha256
    assert hashlib.sha256(path.read_bytes()).hexdigest() == report.index.c_sha256


def test_gzip_open_agrees_with_the_reader(tmp_path: Path) -> None:
    """The reader is not a second, privately-agreeing interpretation of the file."""

    records = _records(2_000)
    path = tmp_path / "messages.jsonl.gz"
    write_seekable_jsonl(path, records, target_chunk_bytes=_TARGET)

    with gzip.open(path, "rb") as handle:
        through_gzip = [line.rstrip(b"\n") for line in handle]
    with SeekableJsonlReader(path) as reader:
        through_index = list(reader.iter_lines())
    assert through_index == through_gzip


@pytest.mark.skipif(shutil.which("gzip") is None, reason="no system gzip(1)")
def test_the_file_passes_system_gzip_integrity_check(tmp_path: Path) -> None:
    """`gzip -t` is the property that justified paying 24% more bytes than zstd."""

    path = tmp_path / "messages.jsonl.gz"
    write_seekable_jsonl(path, _records(2_000), target_chunk_bytes=_TARGET)

    assert subprocess.run(["gzip", "-t", str(path)], check=False).returncode == 0


def test_two_writes_are_byte_identical(tmp_path: Path) -> None:
    """Idempotence: a rebuild that changed nothing must not touch the file.

    `write_text_if_changed` is the archive's whole story for a checkout that does not churn, and
    it can only work if the producer is deterministic. Both the data and the sidecar are checked,
    and so is the *report*, because a caller counting changed files is the thing that would
    otherwise silently start reporting churn.
    """

    records = _records(2_000)
    first = tmp_path / "a" / "messages.jsonl.gz"
    second = tmp_path / "b" / "messages.jsonl.gz"
    write_seekable_jsonl(first, records, target_chunk_bytes=_TARGET)
    write_seekable_jsonl(second, records, target_chunk_bytes=_TARGET)
    assert first.read_bytes() == second.read_bytes()

    before = first.stat().st_mtime_ns
    again = write_seekable_jsonl(first, records, target_chunk_bytes=_TARGET)
    assert not again.data_changed
    assert not again.index_changed
    assert first.stat().st_mtime_ns == before


def test_members_end_at_line_boundaries(tmp_path: Path) -> None:
    """No member may contain a partial line, whatever the target says."""

    records = _records(2_000)
    path = tmp_path / "messages.jsonl.gz"
    report = write_seekable_jsonl(path, records, target_chunk_bytes=_TARGET)
    blob = path.read_bytes()

    lines_seen = 0
    for position, entry in enumerate(report.index.members):
        member = gzip.decompress(blob[entry.c_off : entry.c_off + entry.c_len])
        assert member.endswith(b"\n")
        assert len(member) == entry.u_len
        assert member.count(b"\n") == entry.n
        assert entry.l0 == lines_seen
        lines_seen += entry.n
        if position < len(report.index.members) - 1:
            # Cut at the FIRST boundary at or after the target, so every member but the last
            # overshoots -- never undershoots, which would mean the writer cut early and made
            # more members than the measured design calls for.
            assert entry.u_len >= _TARGET
    assert lines_seen == len(records)


def test_index_header_describes_the_file(tmp_path: Path) -> None:
    """The sidecar is a durable format; pin its shape, not just its behaviour."""

    path = tmp_path / "messages.jsonl.gz"
    report = write_seekable_jsonl(path, _records(500), target_chunk_bytes=_TARGET)
    lines = index_path_for(path).read_text(encoding="utf-8").splitlines()
    header: object = json.loads(lines[0])
    assert isinstance(header, dict)

    assert lines[0].startswith(f'{{"format":"{INDEX_FORMAT}","version":1,')
    assert header["codec"] == "gzip"
    assert header["codec_level"] == 6
    assert header["timestamp_key"] == "timestamp_ms"
    assert header["timestamps_sorted"] is True
    assert header["data_file"] == "messages.jsonl.gz"
    assert header["member_count"] == len(lines) - 1
    assert header["c_size"] == path.stat().st_size
    member: object = json.loads(lines[1])
    assert isinstance(member, dict)
    assert sorted(member) == ["c_len", "c_off", "l0", "n", "t0", "t1", "u_len", "u_off"]
    # Round-tripping the text back through the parser is what a future Rust reader will do; if
    # the writer and the parser ever disagree about the shape, this is where it shows.
    assert ChunkIndex.from_text("".join(f"{line}\n" for line in lines), "index") == report.index


def test_read_range_reads_only_the_overlapping_members(
    large_archive: tuple[Path, list[dict[str, JsonValue]]]
) -> None:
    """A seek must cost the size of the answer, not the size of the file."""

    path, records = large_archive
    start = int(records[20_000]["timestamp_ms"])  # type: ignore[arg-type]
    end = int(records[20_050]["timestamp_ms"])  # type: ignore[arg-type]

    with SeekableJsonlReader(path) as reader:
        selected = list(reader.read_range(start, end))
        read = reader.data_bytes_read
        overlapping = [
            entry for entry in reader.index.members if entry.overlaps(start, end)
        ]

    assert [record["ordinal"] for record in selected] == list(range(20_000, 20_050))
    assert read == sum(entry.c_len for entry in overlapping)
    assert read < path.stat().st_size // 8, (
        f"read {read} of {path.stat().st_size} bytes for 50 records out of 50,000"
    )


def test_tail_touches_only_the_final_member(
    large_archive: tuple[Path, list[dict[str, JsonValue]]]
) -> None:
    """`tail(20)` is the "recent N" case the owner named; it must open one member."""

    path, records = large_archive
    with SeekableJsonlReader(path) as reader:
        recent = reader.tail(20)
        read = reader.data_bytes_read
        final = reader.index.members[-1]

    assert [record["ordinal"] for record in recent] == list(range(49_980, 50_000))
    assert read == final.c_len
    assert read < path.stat().st_size // 10


def test_tail_spans_members_when_it_has_to(tmp_path: Path) -> None:
    """A tail longer than the final member walks backwards, and only backwards."""

    records = _records(2_000)
    path = tmp_path / "messages.jsonl.gz"
    report = write_seekable_jsonl(path, records, target_chunk_bytes=_TARGET)
    wanted = report.index.members[-1].n + 5

    with SeekableJsonlReader(path) as reader:
        recent = reader.tail(wanted)
        read = reader.data_bytes_read

    assert [record["ordinal"] for record in recent] == list(range(2_000 - wanted, 2_000))
    assert read == sum(entry.c_len for entry in report.index.members[-2:])


def test_read_lines_by_number(tmp_path: Path) -> None:
    """Line-number reads use the same mechanism and get the same guarantee."""

    records = _records(2_000)
    path = tmp_path / "messages.jsonl.gz"
    report = write_seekable_jsonl(path, records, target_chunk_bytes=_TARGET)
    boundary = report.index.members[0].n

    with SeekableJsonlReader(path) as reader:
        assert [r["ordinal"] for r in reader.read_lines(10, 14)] == [10, 11, 12, 13]
        first_member_only = reader.data_bytes_read
        reader.reset_counters()
        # Straddling a member boundary must return the join, not two disjoint halves.
        spanning = [r["ordinal"] for r in reader.read_lines(boundary - 2, boundary + 2)]
        assert spanning == [boundary - 2, boundary - 1, boundary, boundary + 1]
        assert reader.data_bytes_read == sum(e.c_len for e in report.index.members[:2])
        reader.reset_counters()
        assert list(reader.read_lines(7, 7)) == []
        assert reader.data_bytes_read == 0

    assert first_member_only == report.index.members[0].c_len
    with SeekableJsonlReader(path) as reader:
        with pytest.raises(ValueError):
            list(reader.read_lines(5, 4))


def test_read_range_open_ended_and_empty(tmp_path: Path) -> None:
    """Unbounded on either side, and a range that selects nothing."""

    records = _records(500)
    path = tmp_path / "messages.jsonl.gz"
    write_seekable_jsonl(path, records, target_chunk_bytes=_TARGET)
    first = int(records[0]["timestamp_ms"])  # type: ignore[arg-type]

    with SeekableJsonlReader(path) as reader:
        assert len(list(reader.read_range(None, None))) == 500
        assert len(list(reader.read_range(None, first + 3_000))) == 3
        assert len(list(reader.read_range(first + 497_000, None))) == 3
        assert list(reader.read_range(first - 10_000, first)) == []
        assert list(reader.read_range(first + 10_000_000, None)) == []


def test_empty_input_still_writes_a_valid_gzip(tmp_path: Path) -> None:
    """Zero records is a file, not a special case: readers must not have to know."""

    path = tmp_path / "messages.jsonl.gz"
    report = write_seekable_jsonl(path, [], target_chunk_bytes=_TARGET)

    assert gzip.decompress(path.read_bytes()) == b""
    assert report.record_count == 0
    assert report.member_count == 1
    assert report.index.members[0].t0 is None
    with SeekableJsonlReader(path) as reader:
        assert list(reader.iter_records()) == []
        assert reader.tail(5) == []
        assert list(reader.read_range(0, 10**15)) == []
        assert list(reader.read_lines(0, 10)) == []
        reader.verify(deep=True)
    assert rebuild_index(path, target_chunk_bytes=_TARGET) == report.index


def test_single_record(tmp_path: Path) -> None:
    path = tmp_path / "messages.jsonl.gz"
    records = _records(1)
    report = write_seekable_jsonl(path, records, target_chunk_bytes=_TARGET)

    assert report.member_count == 1
    assert report.index.members[0].t0 == report.index.members[0].t1
    with SeekableJsonlReader(path) as reader:
        assert reader.tail(20) == records
        assert list(reader.iter_records()) == records


def test_a_record_larger_than_the_target_lands_whole_in_its_own_member(
    tmp_path: Path,
) -> None:
    """The cut is at or AFTER the target, so an oversize record is never split."""

    giant = _record(1, at=2_000, filler=4 * _TARGET)
    records: list[dict[str, JsonValue]] = [
        _record(0, at=1_000),
        giant,
        _record(2, at=3_000),
    ]
    path = tmp_path / "messages.jsonl.gz"
    report = write_seekable_jsonl(path, records, target_chunk_bytes=_TARGET)

    assert [entry.n for entry in report.index.members] == [2, 1]
    assert report.index.members[0].u_len > _TARGET
    assert gzip.decompress(path.read_bytes()) == _encoded(records)
    with SeekableJsonlReader(path) as reader:
        assert list(reader.read_range(2_000, 2_001)) == [giant]
        assert reader.data_bytes_read == report.index.members[0].c_len


def test_identical_timestamps_are_all_returned(tmp_path: Path) -> None:
    """Duplicate instants must not be lost to a bisect that stops at the first hit.

    Every record here shares one timestamp, so the range query has to return the whole file --
    including the members a `t1 >= start` bisect lands on the near edge of.
    """

    records = [_record(index, at=4_242) for index in range(2_000)]
    path = tmp_path / "messages.jsonl.gz"
    report = write_seekable_jsonl(path, records, target_chunk_bytes=_TARGET)

    assert report.member_count > 1
    assert report.index.timestamps_sorted is True
    with SeekableJsonlReader(path) as reader:
        assert [r["ordinal"] for r in reader.read_range(4_242, 4_243)] == list(range(2_000))
        assert list(reader.read_range(4_243, 4_244)) == []
        assert list(reader.read_range(4_241, 4_242)) == []


def test_unsorted_timestamps_are_declared_and_still_answered_correctly(
    tmp_path: Path,
) -> None:
    """An out-of-order stream disables the bisect rather than silently mis-answering."""

    records = _records(2_000)
    records[900], records[1_100] = records[1_100], records[900]
    path = tmp_path / "messages.jsonl.gz"
    report = write_seekable_jsonl(path, records, target_chunk_bytes=_TARGET)
    assert report.index.timestamps_sorted is False

    moved = int(records[900]["timestamp_ms"])  # type: ignore[arg-type]
    with SeekableJsonlReader(path) as reader:
        found = list(reader.read_range(moved, moved + 1))
    assert [record["ordinal"] for record in found] == [1_100]


def test_records_without_a_timestamp_are_never_selected_by_a_range(
    tmp_path: Path,
) -> None:
    """A record off the time axis is absent from it, not at the beginning of it."""

    records: list[dict[str, JsonValue]] = [
        _record(0, at=1_000),
        _record(1),
        _record(2, at=3_000),
    ]
    path = tmp_path / "messages.jsonl.gz"
    write_seekable_jsonl(path, records, target_chunk_bytes=_TARGET)

    with SeekableJsonlReader(path) as reader:
        assert [r["ordinal"] for r in reader.read_range(None, None)] == [0, 2]
        # It is still a record: it has a line number, and line reads return it.
        assert [r["ordinal"] for r in reader.read_lines(0, 3)] == [0, 1, 2]


def test_a_member_with_no_timestamps_declares_the_file_unsorted(tmp_path: Path) -> None:
    """A member with no position on the time axis cannot be bisected past."""

    records = [_record(index) for index in range(2_000)]
    path = tmp_path / "messages.jsonl.gz"
    report = write_seekable_jsonl(path, records, target_chunk_bytes=_TARGET)

    assert report.index.timestamps_sorted is False
    assert all(entry.t0 is None for entry in report.index.members)
    with SeekableJsonlReader(path) as reader:
        assert list(reader.read_range(None, None)) == []
        assert reader.data_bytes_read == 0
        assert [r["ordinal"] for r in reader.tail(2)] == [1_998, 1_999]


def test_pre_encoded_lines_are_preserved_byte_for_byte(tmp_path: Path) -> None:
    """The migration door: re-chunking must not disturb the recorded sha256.

    The lines here are deliberately not what this module's own encoder would produce -- keys out
    of alphabetical order, spaces after the separators -- because that is exactly the difference
    that would break `manifest.json` while leaving every record semantically intact.
    """

    lines = [
        b'{"zeta": 1, "timestamp_ms": %d, "alpha": "\xc3\xa9"}' % (1_000 + index)
        for index in range(500)
    ]
    original = b"".join(line + b"\n" for line in lines)
    path = tmp_path / "messages.jsonl.gz"
    report = write_seekable_jsonl_lines(path, lines, target_chunk_bytes=2_048)

    assert gzip.decompress(path.read_bytes()) == original
    assert report.index.u_sha256 == hashlib.sha256(original).hexdigest()
    with SeekableJsonlReader(path) as reader:
        assert list(reader.iter_lines()) == lines
        assert [r["zeta"] for r in reader.read_range(1_010, 1_012)] == [1, 1]


def test_missing_index_degrades_to_a_correct_full_scan(tmp_path: Path) -> None:
    """The point of the container: the `.gz` alone still answers every question.

    Correctness is asserted against the indexed answers, and the *cost* is asserted too -- the
    fallback must read the whole file, because a fallback that quietly read only part of it would
    be returning a truncated answer rather than a slow one.
    """

    records = _records(2_000)
    path = tmp_path / "messages.jsonl.gz"
    write_seekable_jsonl(path, records, target_chunk_bytes=_TARGET)
    with SeekableJsonlReader(path) as indexed:
        expected_range = list(indexed.read_range(1_700_000_000_000, 1_700_000_010_000))
    index_path_for(path).unlink()
    size = path.stat().st_size

    with SeekableJsonlReader(path) as reader:
        assert reader.has_index is False
        assert list(reader.read_range(1_700_000_000_000, 1_700_000_010_000)) == expected_range
        assert reader.data_bytes_read >= size
        reader.reset_counters()
        assert [r["ordinal"] for r in reader.tail(20)] == list(range(1_980, 2_000))
        assert reader.data_bytes_read >= size
        reader.reset_counters()
        assert [r["ordinal"] for r in reader.read_lines(10, 13)] == [10, 11, 12]
        assert list(reader.iter_records()) == records
        reader.verify()


def test_use_index_false_forces_the_scan_path(tmp_path: Path) -> None:
    """A caller who distrusts a present index can still read the file."""

    records = _records(500)
    path = tmp_path / "messages.jsonl.gz"
    write_seekable_jsonl(path, records, target_chunk_bytes=_TARGET)

    with SeekableJsonlReader(path, use_index=False) as reader:
        assert reader.has_index is False
        assert reader.tail(2) == records[-2:]
        assert reader.data_bytes_read >= path.stat().st_size


def test_a_lost_index_can_be_rebuilt_from_the_data_alone(tmp_path: Path) -> None:
    """Losing the index costs speed and nothing else -- including permanently."""

    records = _records(2_000)
    path = tmp_path / "messages.jsonl.gz"
    report = write_seekable_jsonl(path, records, target_chunk_bytes=_TARGET)
    index_path_for(path).unlink()

    assert write_rebuilt_index(path, target_chunk_bytes=_TARGET) == report.index
    assert index_path_for(path).read_text(encoding="utf-8") == report.index.to_text()
    with SeekableJsonlReader(path) as reader:
        assert reader.has_index is True
        assert [r["ordinal"] for r in reader.tail(3)] == [1_997, 1_998, 1_999]
        assert reader.data_bytes_read == report.index.members[-1].c_len


def test_a_truncated_final_member_is_reported(tmp_path: Path) -> None:
    """Truncation must be an error on both paths, indexed and scanning."""

    records = _records(2_000)
    path = tmp_path / "messages.jsonl.gz"
    report = write_seekable_jsonl(path, records, target_chunk_bytes=_TARGET)
    blob = path.read_bytes()
    path.write_bytes(blob[: len(blob) - 40])

    # The size check fires first, because a short file is also an index disagreement.
    with pytest.raises(IndexMismatchError):
        SeekableJsonlReader(path)
    with SeekableJsonlReader(path, use_index=False) as reader:
        with pytest.raises(TruncatedArchiveError):
            list(reader.iter_lines())
    with pytest.raises(TruncatedArchiveError):
        rebuild_index(path, target_chunk_bytes=_TARGET)

    # ... and with the stale sidecar removed from the picture, the earlier members still read.
    # A truncated tail does not cost you the head of the archive.
    with SeekableJsonlReader(path, use_index=False) as reader:
        head = []
        try:
            for record in reader.iter_records():
                head.append(record)
        except TruncatedArchiveError:
            pass
    assert len(head) >= sum(entry.n for entry in report.index.members[:-1])


def test_an_index_that_disagrees_with_the_file_is_reported(tmp_path: Path) -> None:
    """A stale index is refused, not obeyed: a wrong answer is worse than an error."""

    records = _records(2_000)
    path = tmp_path / "messages.jsonl.gz"
    write_seekable_jsonl(path, records, target_chunk_bytes=_TARGET)

    # (a) The data file grew. The O(1) open-time check catches it.
    path.write_bytes(path.read_bytes() + b"\x00")
    with pytest.raises(IndexMismatchError, match="compressed bytes"):
        SeekableJsonlReader(path)

    # (b) The data changed without changing length. The size check cannot see this; the
    # per-member inflate does, at the first member it touches, because gzip carries a CRC.
    write_seekable_jsonl(path, records, target_chunk_bytes=_TARGET)
    blob = bytearray(path.read_bytes())
    blob[len(blob) // 2] ^= 0xFF
    path.write_bytes(bytes(blob))
    with SeekableJsonlReader(path) as reader:
        with pytest.raises((TruncatedArchiveError, IndexMismatchError)):
            list(reader.iter_lines())
        with pytest.raises((TruncatedArchiveError, IndexMismatchError)):
            reader.verify(deep=True)


def test_an_index_describing_a_different_member_table_is_rejected(tmp_path: Path) -> None:
    """Internal consistency is checked at parse time, before any read trusts it."""

    path = tmp_path / "messages.jsonl.gz"
    write_seekable_jsonl(path, _records(500), target_chunk_bytes=_TARGET)
    sidecar = index_path_for(path)
    lines = sidecar.read_text(encoding="utf-8").splitlines()

    # Drop a member from the middle: the table stops being contiguous.
    sidecar.write_text(
        "".join(f"{line}\n" for line in lines[:2] + lines[3:]), encoding="utf-8"
    )
    with pytest.raises(IndexMismatchError, match="not contiguous"):
        SeekableJsonlReader(path)

    sidecar.write_text("", encoding="utf-8")
    with pytest.raises(IndexMismatchError, match="empty"):
        SeekableJsonlReader(path)

    sidecar.write_text('{"format":"something-else","version":1}\n', encoding="utf-8")
    with pytest.raises(IndexMismatchError, match="unknown index format"):
        SeekableJsonlReader(path)


def test_deep_verify_accepts_a_sound_archive(tmp_path: Path) -> None:
    path = tmp_path / "messages.jsonl.gz"
    write_seekable_jsonl(path, _records(2_000), target_chunk_bytes=_TARGET)

    with SeekableJsonlReader(path) as reader:
        reader.verify()
        reader.verify(deep=True)
        assert reader.data_bytes_read == path.stat().st_size


def test_writer_refuses_values_no_other_parser_could_read(tmp_path: Path) -> None:
    """`NaN` is Python-only. A lossless format may not emit a token only it can read back."""

    path = tmp_path / "messages.jsonl.gz"
    with pytest.raises(ValueError):
        write_seekable_jsonl(path, [{"timestamp_ms": 1, "value": float("nan")}])
    with pytest.raises(ValueError):
        write_seekable_jsonl_lines(path, [b'{"timestamp_ms":1}\n'])
    with pytest.raises(ValueError):
        write_seekable_jsonl(path, _records(2), target_chunk_bytes=0)


def test_timestamp_key_is_configurable_and_recorded(tmp_path: Path) -> None:
    """The index says which field it indexed, so a reader cannot guess wrong."""

    records: list[dict[str, JsonValue]] = [
        {"ordinal": index, "at_ms": 1_000 + index} for index in range(200)
    ]
    path = tmp_path / "events.jsonl.gz"
    report = write_seekable_jsonl(path, records, timestamp_key="at_ms", target_chunk_bytes=1_024)

    assert report.index.timestamp_key == "at_ms"
    assert report.index.members[0].t0 == 1_000
    with SeekableJsonlReader(path) as reader:
        # The reader was given no key at all and still filters on the right field, because the
        # header outranks the constructor default.
        assert [r["ordinal"] for r in reader.read_range(1_010, 1_013)] == [10, 11, 12]


def test_index_path_naming(tmp_path: Path) -> None:
    assert index_path_for(tmp_path / "m.jsonl.gz") == tmp_path / "m.jsonl.gz.index.jsonl"


def test_reader_is_reusable_and_closes_its_handle(tmp_path: Path) -> None:
    path = tmp_path / "messages.jsonl.gz"
    write_seekable_jsonl(path, _records(500), target_chunk_bytes=_TARGET)

    reader = SeekableJsonlReader(path)
    try:
        assert len(reader.tail(1)) == 1
        assert len(reader.tail(1)) == 1
    finally:
        reader.close()
    reader.close()


def _iter_ordinals(reader: SeekableJsonlReader) -> Iterator[int]:
    for record in reader.iter_records():
        value = record["ordinal"]
        assert isinstance(value, int)
        yield value


def test_large_archive_streams_in_order(
    large_archive: tuple[Path, list[dict[str, JsonValue]]]
) -> None:
    """The whole-file read is still the whole file, in order, at production chunk size."""

    path, records = large_archive
    with SeekableJsonlReader(path) as reader:
        assert len(reader.index.members) > 1
        assert list(_iter_ordinals(reader)) == list(range(len(records)))
        assert reader.data_bytes_read == path.stat().st_size


# --------------------------------------------------------------------------------------------
# The container property, and the two ways the pair can disagree.
#
# Everything above this line exercises the writer's own output read back by its own reader, which
# is the easy half. These tests exercise the half the module's docstring actually stakes its design
# on: that members concatenate and every tool can read the result, that the index is recoverable
# from the `.gz` alone *including* for a file this writer did not produce, and that a sidecar which
# disagrees with the data or with the caller is refused rather than obeyed.
# --------------------------------------------------------------------------------------------


def _concatenate(target: Path, *parts: Path) -> None:
    target.write_bytes(b"".join(part.read_bytes() for part in parts))


def test_concatenated_archives_rebuild_into_a_readable_index(tmp_path: Path) -> None:
    """Members concatenate -- and an empty one in the middle must not eat the head of the file.

    `cat a.gz b.gz > ab.gz` is the property the whole container choice rests on, and
    `rebuild_index` is what makes the result queryable. The empty member in the middle is not an
    exotic input: it is precisely what `cat`-ing an archive that happened to have no records
    produces, and it is the one shape that puts a -1 into the bisect key.
    """

    first = tmp_path / "a.jsonl.gz"
    empty = tmp_path / "e.jsonl.gz"
    second = tmp_path / "b.jsonl.gz"
    write_seekable_jsonl(first, [_record(0, at=10), _record(1, at=20)])
    write_seekable_jsonl(empty, [])
    write_seekable_jsonl(second, [_record(2, at=30), _record(3, at=40)])

    joined = tmp_path / "joined.jsonl.gz"
    _concatenate(joined, first, empty, second)
    index = write_rebuilt_index(joined)

    assert [entry.n for entry in index.members] == [2, 0, 2]
    assert index.timestamps_sorted is False
    assert gzip.decompress(joined.read_bytes()).count(b"\n") == 4
    with SeekableJsonlReader(joined) as reader:
        assert [r["ordinal"] for r in reader.iter_records()] == [0, 1, 2, 3]
        assert [r["ordinal"] for r in reader.read_range(None, None)] == [0, 1, 2, 3]
        assert [r["ordinal"] for r in reader.read_range(5, 25)] == [0, 1]
        assert [r["ordinal"] for r in reader.read_range(25, None)] == [2, 3]
        assert [r["ordinal"] for r in reader.read_lines(1, 3)] == [1, 2]
        assert [r["ordinal"] for r in reader.tail(3)] == [1, 2, 3]


def test_a_trailing_empty_member_does_not_lose_the_bisect(tmp_path: Path) -> None:
    """The sorted case still bisects: `_bisectable` is a precondition, not a blanket refusal."""

    path = tmp_path / "messages.jsonl.gz"
    report = write_seekable_jsonl(path, _records(2_000), target_chunk_bytes=_TARGET)
    assert report.index.timestamps_sorted is True
    assert len(report.index.members) > 2


def test_records_out_of_order_inside_one_member_still_bisect(tmp_path: Path) -> None:
    """Only member-level order is a precondition; a swap inside one member is invisible to it."""

    records = [_record(index, at=1_000 + index) for index in range(20)]
    records[3], records[7] = records[7], records[3]
    path = tmp_path / "messages.jsonl.gz"
    report = write_seekable_jsonl(path, records, target_chunk_bytes=1 << 20)

    assert report.member_count == 1
    assert report.index.timestamps_sorted is True
    with SeekableJsonlReader(path) as reader:
        assert sorted(int(str(r["ordinal"])) for r in reader.read_range(1_003, 1_008)) == [
            3,
            4,
            5,
            6,
            7,
        ]


def test_a_sidecar_claiming_an_order_the_table_does_not_have_is_downgraded(
    tmp_path: Path,
) -> None:
    """A hand-edited `timestamps_sorted: true` buys a wrong answer unless the parse checks it."""

    path = tmp_path / "messages.jsonl.gz"
    write_seekable_jsonl(path, _records(2_000), target_chunk_bytes=_TARGET)
    sidecar = index_path_for(path)
    lines = sidecar.read_text(encoding="utf-8").split("\n")
    header = json.loads(lines[0])
    members = [json.loads(line) for line in lines[1:] if line]
    # Blank the timestamps on one interior member, exactly as a rebuild over a concatenation
    # would, and leave the header still claiming the file is sorted.
    members[1]["t0"] = None
    members[1]["t1"] = None
    sidecar.write_text(
        "".join(
            json.dumps(obj, separators=(",", ":")) + "\n" for obj in [header, *members]
        ),
        encoding="utf-8",
    )

    with SeekableJsonlReader(path) as reader:
        assert reader.index.timestamps_sorted is False
        # The first member's records are still reachable, which is the whole point.
        assert [r["ordinal"] for r in reader.read_range(None, 1_700_000_003_000)] == [0, 1, 2]


def test_two_concurrent_scans_of_one_reader_do_not_corrupt_each_other(
    tmp_path: Path,
) -> None:
    """Interleaved fallback walks are a caller's right, not a caller's bug.

    Below one scan block this passes whatever the implementation does, so the archive here is
    deliberately several blocks long: a shared read cursor only destroys the stream once the walk
    has to come back for more bytes.
    """

    path = tmp_path / "messages.jsonl.gz"
    write_seekable_jsonl(path, _records(6_000), target_chunk_bytes=_TARGET)
    index_path_for(path).unlink()
    assert path.stat().st_size > 2 * (1 << 18)

    with SeekableJsonlReader(path) as reader:
        left = reader.iter_records()
        right = reader.iter_records()
        interleaved = [
            (int(str(next(left)["ordinal"])), int(str(next(right)["ordinal"])))
            for _ in range(3_000)
        ]
    assert interleaved == [(index, index) for index in range(3_000)]


def test_an_explicit_timestamp_key_is_honoured_or_refused_never_ignored(
    tmp_path: Path,
) -> None:
    """The same query may not mean two things depending on whether the sidecar exists."""

    records: list[dict[str, JsonValue]] = [
        {"ordinal": index, "timestamp_ms": 100 + index, "started_ms": 900 - index}
        for index in range(10)
    ]
    path = tmp_path / "events.jsonl.gz"
    write_seekable_jsonl(path, records, target_chunk_bytes=1 << 20)

    with pytest.raises(IndexMismatchError, match="started_ms"):
        SeekableJsonlReader(path, timestamp_key="started_ms")
    # Naming the field the index actually used is fine, and so is naming none at all.
    with SeekableJsonlReader(path, timestamp_key="timestamp_ms") as reader:
        assert [r["ordinal"] for r in reader.read_range(101, 104)] == [1, 2, 3]

    index_path_for(path).unlink()
    with SeekableJsonlReader(path, timestamp_key="started_ms") as reader:
        assert [r["ordinal"] for r in reader.read_range(895, 900)] == [1, 2, 3, 4, 5]


def _rewrite_header(path: Path, **fields: object) -> None:
    sidecar = index_path_for(path)
    lines = sidecar.read_text(encoding="utf-8").split("\n")
    header = json.loads(lines[0])
    header.update(fields)
    sidecar.write_text(
        "\n".join([json.dumps(header, separators=(",", ":")), *lines[1:]]), encoding="utf-8"
    )


def test_an_index_naming_a_codec_this_reader_lacks_says_so(tmp_path: Path) -> None:
    """"Unsupported codec" and "damaged data" have opposite remedies and must not be one error."""

    path = tmp_path / "messages.jsonl.gz"
    write_seekable_jsonl(path, _records(200), target_chunk_bytes=_TARGET)
    _rewrite_header(path, codec="zstd", codec_level=19)

    with pytest.raises(UnsupportedCodecError, match="zstd"):
        SeekableJsonlReader(path)
    assert issubclass(UnsupportedCodecError, SeekableJsonlError)
    # `use_index=False` is not a way around it: the fallback really is gzip-only, and it says so
    # by walking the file, which for a genuinely gzip file simply works.
    with SeekableJsonlReader(path, use_index=False) as reader:
        assert len(list(reader.iter_records())) == 200


def test_an_index_describing_a_different_data_file_is_refused(tmp_path: Path) -> None:
    """The header names its file; pairing it with a same-sized stranger is silent otherwise."""

    path = tmp_path / "messages.jsonl.gz"
    write_seekable_jsonl(path, _records(200), target_chunk_bytes=_TARGET)
    _rewrite_header(path, data_file="somebody-elses.jsonl.gz")

    with pytest.raises(IndexMismatchError, match="somebody-elses"):
        SeekableJsonlReader(path)


def test_a_damaged_sidecar_is_a_seekable_jsonl_error_and_never_a_stdlib_one(
    tmp_path: Path,
) -> None:
    """A caller degrades to a scan on `SeekableJsonlError`; nothing may escape past it."""

    path = tmp_path / "messages.jsonl.gz"
    write_seekable_jsonl(path, _records(200), target_chunk_bytes=_TARGET)
    sidecar = index_path_for(path)
    good = sidecar.read_text(encoding="utf-8")

    broken_type = good.replace('"c_len":', '"c_len":"')
    for damaged in ("not json at all\n", '["an","array"]\n', broken_type):
        sidecar.write_text(damaged, encoding="utf-8")
        with pytest.raises(SeekableJsonlError):
            SeekableJsonlReader(path)

    sidecar.write_bytes(b"\xff\xfe\x00")
    with pytest.raises(IndexMismatchError, match="UTF-8"):
        SeekableJsonlReader(path)

    # A stray trailing newline is not damage. It carries no record, and refusing it would make an
    # archive unreadable for the sake of a byte an editor added.
    sidecar.write_text(good + "\n", encoding="utf-8")
    with SeekableJsonlReader(path) as reader:
        assert len(list(reader.iter_records())) == 200


def test_bytes_after_the_final_member_are_reported_with_an_index_present(
    tmp_path: Path,
) -> None:
    """Trailing garbage is caught on both paths, and named as data damage on both."""

    path = tmp_path / "messages.jsonl.gz"
    write_seekable_jsonl(path, _records(500), target_chunk_bytes=_TARGET)
    path.write_bytes(path.read_bytes() + b"not a gzip member")

    with pytest.raises(IndexMismatchError, match="compressed bytes"):
        SeekableJsonlReader(path)
    with SeekableJsonlReader(path, use_index=False) as reader:
        with pytest.raises(TruncatedArchiveError, match="not part of a gzip member"):
            list(reader.iter_records())


def test_a_failed_index_write_leaves_new_data_with_no_sidecar_rather_than_a_stale_one(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The two files cannot be published atomically, so the surviving pair must be the safe one.

    A stale sidecar of coincidentally equal size passes the O(1) open-time check and then fails
    inside a member, which reads as damaged data. No sidecar at all reads as exactly what it is.
    """

    path = tmp_path / "messages.jsonl.gz"
    write_seekable_jsonl(path, _records(400), target_chunk_bytes=_TARGET)
    sidecar = index_path_for(path)
    assert sidecar.is_file()

    def explode(target: Path, text: str, *, executable: bool = False) -> bool:
        raise OSError("no space left on device")

    monkeypatch.setattr("wrkviz.seekable_jsonl.write_text_if_changed", explode)
    shifted = [_record(index, at=2_000 + index) for index in range(400)]
    with pytest.raises(OSError):
        write_seekable_jsonl(path, shifted, target_chunk_bytes=_TARGET)

    assert not sidecar.exists()
    monkeypatch.undo()
    with SeekableJsonlReader(path) as reader:
        assert reader.has_index is False
        assert [r["ordinal"] for r in reader.tail(2)] == [398, 399]
    assert write_rebuilt_index(path, target_chunk_bytes=_TARGET).record_count == 400
