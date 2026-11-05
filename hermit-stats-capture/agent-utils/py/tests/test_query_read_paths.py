"""Contract tests for the slicing read paths in :mod:`wrkviz.query`.

The read path was rewritten to stop materializing the transcript projections, and a rewrite
whose entire point is "it costs less" needs its cost asserted rather than described. Three
instruments do that here, and none of them is a clock:

*``TranscriptQuery.bytes_read``.* A byte count is a proof; a timing assertion is a flake on a
loaded build host. Every laziness claim below is a bound on this counter, including the one
that matters most -- that constructing the object reads nothing at all.

*A full-scan reference.* Every seek, bisect and backwards read is checked against the obvious
implementation over the same records. The seek is only interesting if it returns the same
answer, so the answer is computed both ways and compared, over a sweep of window bounds that
deliberately lands on, one before, and one after instants shared by several records.

*A differential against the package reader.* ``_ChunkedJsonlReader`` is a standalone-library
copy of :class:`wrkviz.seekable_jsonl.SeekableJsonlReader`, kept separate because
this file is copied verbatim into every generated archive as the ``./timeline`` executable and
may import nothing but the standard library. The duplication is pinned here rather than by a
comment, because a comment does not fail.
"""

from __future__ import annotations

import base64
import hashlib
import json
import random
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path

import pytest

from wrkviz.archive import JsonValue
from wrkviz.query import (
    ArchiveReadError,
    OrdinalRange,
    QueryFilters,
    TranscriptQuery,
    _ChunkedJsonlReader,
)
from wrkviz.seekable_jsonl import (
    SeekableJsonlReader,
    write_seekable_jsonl,
)


@dataclass(frozen=True)
class _Window:
    """The half-open window the query filters accept, spelled out for the fixtures."""

    start_ms: int | None
    end_ms: int | None

    def contains(self, timestamp_ms: int) -> bool:
        if self.start_ms is not None and timestamp_ms < self.start_ms:
            return False
        return self.end_ms is None or timestamp_ms < self.end_ms

    def overlaps(self, start_ms: int, end_ms: int | None) -> bool:
        upper = start_ms if end_ms is None else end_ms
        if self.start_ms is not None and upper < self.start_ms:
            return False
        return self.end_ms is None or start_ms < self.end_ms


# ---------------------------------------------------------------------------------------
# A synthetic transcript export
# ---------------------------------------------------------------------------------------
#
# Built here rather than borrowed from `transcript_export` on purpose. What is under test is
# the reader's behaviour at boundaries -- duplicate instants, a window bound landing exactly
# on one, a selection that ends early in the file -- and those are conditions a real export
# happens to contain or happens not to. A fixture that states them cannot stop containing
# them.


def _prompt(
    ordinal: int, timestamp_ms: int, team: str, author_kind: str, text: str
) -> dict[str, object]:
    return {
        "record_id": f"logical-prompt:{ordinal:04d}",
        "record_type": "prompt",
        "ordinal": ordinal,
        "timestamp_ms": timestamp_ms,
        "team_slug": team,
        "author_kind": author_kind,
        "text": text,
    }


def _response(
    prompt_id: str | None, timestamp_ms: int, team: str, index: int, text: str
) -> dict[str, object]:
    return {
        "record_id": f"response:{index:04d}",
        "record_type": "response",
        "in_reply_to_prompt_id": prompt_id,
        "timestamp_ms": timestamp_ms,
        "team_slug": team,
        "text": text,
    }


def _sort_key(record: dict[str, object]) -> tuple[int, str]:
    timestamp = record["timestamp_ms"]
    assert isinstance(timestamp, int)
    record_id = record["record_id"]
    assert isinstance(record_id, str)
    return (timestamp, record_id)


def _write_export(
    root: Path,
    prompts: Sequence[dict[str, object]],
    messages: Sequence[dict[str, object]],
    *,
    declare_bytes: bool = True,
) -> Path:
    """Lay out an ``extracted/transcripts`` tree the reader will accept."""

    transcripts = root / "extracted" / "transcripts"
    transcripts.mkdir(parents=True, exist_ok=True)
    payload = {
        "prompts.jsonl": prompts,
        "messages.jsonl": messages,
        # Declared, checked for presence and size, and opened by no query. It is here
        # because the constructor's contract covers every projection the archive carries,
        # which is what `test_a_truncated_unconsulted_projection_is_still_refused_at_open`
        # pins. The monotonic baseline is not among them: it is rerun state and lives in the
        # build store, so an archive that does not carry it is correct rather than damaged.
        "system-inputs.jsonl": [{"record_id": "system-input:1"}],
    }
    files: dict[str, object] = {}
    for name, records in payload.items():
        text = "".join(
            json.dumps(record, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
            + "\n"
            for record in records
        )
        (transcripts / name).write_text(text, encoding="utf-8")
        entry: dict[str, object] = {
            "sha256": hashlib.sha256(text.encode("utf-8")).hexdigest(),
            "records": len(records),
        }
        if declare_bytes:
            entry["bytes"] = len(text.encode("utf-8"))
        files[name] = entry
    (transcripts / "manifest.json").write_text(
        json.dumps({"schema_version": 1, "files": files}, indent=1), encoding="utf-8"
    )
    return root


#: Instants deliberately repeated 2, 3 and 4 times, so a window bound landing on a shared
#: instant is exercised rather than hoped for.
_INSTANTS = (100, 100, 100, 200, 200, 300, 300, 300, 300, 400, 500, 500)
_TEAMS = ("team-a", "team-b")
_AUTHORS = ("owner_human", "agent", "someone_else")


@pytest.fixture
def export(tmp_path: Path) -> Path:
    prompts = [
        _prompt(
            ordinal,
            instant,
            _TEAMS[ordinal % len(_TEAMS)],
            _AUTHORS[ordinal % len(_AUTHORS)],
            f"prompt body {ordinal}",
        )
        for ordinal, instant in enumerate(_INSTANTS, start=1)
    ]
    messages: list[dict[str, object]] = list(prompts)
    for index, prompt in enumerate(prompts, start=1):
        prompt_id = prompt["record_id"]
        assert isinstance(prompt_id, str)
        timestamp = prompt["timestamp_ms"]
        assert isinstance(timestamp, int)
        team = prompt["team_slug"]
        assert isinstance(team, str)
        messages.append(
            _response(prompt_id, timestamp + 1, team, index, f"reply to {index}")
        )
    # One response linked to nothing, to prove `_message_ordinal` drops it rather than
    # tripping over a null link.
    messages.append(_response(None, 601, "team-a", 999, "orphaned reply"))
    return _write_export(tmp_path, prompts, sorted(messages, key=_sort_key))


def _reference_prompts(
    export_root: Path, filters: QueryFilters, which: str
) -> list[dict[str, object]]:
    """The obvious implementation: read everything, filter, return it."""

    human = {"owner_human", "other_human"}
    bot = {"agent", "system"}
    kept: list[dict[str, object]] = []
    path = export_root / "extracted" / "transcripts" / "prompts.jsonl"
    for line in path.read_text(encoding="utf-8").splitlines():
        record = json.loads(line)
        author = record["author_kind"]
        klass = "human" if author in human else "bot" if author in bot else "unclassified"
        if which != "all" and klass != which:
            continue
        if filters.teams and record["team_slug"] not in filters.teams:
            continue
        if filters.window is not None and not filters.window.contains(
            record["timestamp_ms"]
        ):
            continue
        kept.append(record)
    return kept


# ---------------------------------------------------------------------------------------
# Laziness
# ---------------------------------------------------------------------------------------


def test_constructing_a_transcript_query_reads_no_projection_bytes(
    export: Path,
) -> None:
    """The constructor's digest loop is gone, and nothing quietly replaced it.

    This is the headline of the rewrite -- it used to hash 220,026,263 bytes before answering
    anything -- so it is asserted as an exact zero rather than a bound.
    """

    assert TranscriptQuery(export).bytes_read == 0


def test_a_small_page_reads_far_less_than_the_projection(tmp_path: Path) -> None:
    """``--limit 1`` costs one ramped read, not the file.

    The projection has to be comfortably larger than the first read window for the claim to
    mean anything, so this one builds its own rather than using the boundary fixture, which is
    deliberately small enough to fit in a single probe.
    """

    prompts = [
        _prompt(ordinal, ordinal * 10, "team-a", "owner_human", "x" * 400)
        for ordinal in range(1, 501)
    ]
    root = _write_export(tmp_path, prompts, prompts)
    query = TranscriptQuery(root)
    records = query.list_prompts(QueryFilters(), None, "all", limit=1)
    size = (root / "extracted" / "transcripts" / "prompts.jsonl").stat().st_size
    assert len(records) == 1
    assert 0 < query.bytes_read < size // 10


def test_a_tail_reads_backwards_rather_than_forwards(export: Path) -> None:
    query = TranscriptQuery(export)
    records = query.list_prompts(QueryFilters(), None, "all", tail=2)
    assert [record["ordinal"] for record in records] == [11, 12]
    size = (export / "extracted" / "transcripts" / "prompts.jsonl").stat().st_size
    assert query.bytes_read <= size


def test_a_filtered_tail_reads_the_projection_at_most_once(export: Path) -> None:
    """The widening rounds must extend the buffer, not restart the read.

    A ``--tail`` whose filter rejects most of what it sees used to restart ``tail_lines`` from
    the end of the file on every round, so round *k* re-read everything rounds 1..k-1 read --
    measured at 1.85 times the message projection to return twenty records. The ceiling for a
    backwards read that keeps its buffer is 1.0, and the fixture is built to force several
    rounds: the accepted team's records stop well before the end of the file.
    """

    tmp = export
    prompts = [
        _prompt(ordinal, ordinal * 10, "team-a" if ordinal <= 5 else "team-b", "owner_human", "x" * 200)
        for ordinal in range(1, 401)
    ]
    root = tmp / "early-selection"
    _write_export(root, prompts, prompts)
    query = TranscriptQuery(root)
    records = query.list_prompts(
        QueryFilters(teams=("team-a",)), None, "all", tail=3
    )
    assert [record["ordinal"] for record in records] == [3, 4, 5]
    size = (root / "extracted" / "transcripts" / "prompts.jsonl").stat().st_size
    assert query.bytes_read <= size, (
        f"read {query.bytes_read} of a {size}-byte projection; the backwards read is "
        "re-reading what it already has"
    )


# ---------------------------------------------------------------------------------------
# Correctness of the seek, against a full scan
# ---------------------------------------------------------------------------------------


@pytest.mark.parametrize("which", ["human", "bot", "all"])
@pytest.mark.parametrize("teams", [(), ("team-a",), ("team-a", "team-b")])
def test_a_window_sweep_agrees_with_a_full_scan(
    export: Path, which: str, teams: tuple[str, ...]
) -> None:
    """Every window bound the fixture can produce, crossed with every selection.

    The bounds range over one before, exactly on, and one after each distinct instant, which
    is where a half-open contract goes wrong if it goes wrong at all -- and where the ties
    matter, because four records share instant 300.
    """

    probes = [None] + [
        instant + delta for instant in sorted(set(_INSTANTS)) for delta in (-1, 0, 1)
    ]
    query = TranscriptQuery(export)
    for start in probes:
        for end in probes:
            if start is not None and end is not None and end < start:
                continue
            filters = QueryFilters(teams=teams, window=_Window(start, end))
            expected = _reference_prompts(export, filters, which)
            actual = query.list_prompts(filters, None, which)
            assert [record["ordinal"] for record in actual] == [
                record["ordinal"] for record in expected
            ], f"start={start} end={end} which={which} teams={teams}"


def test_limit_and_tail_take_opposite_ends_of_the_same_selection(
    export: Path,
) -> None:
    query = TranscriptQuery(export)
    filters = QueryFilters()
    everything = query.list_prompts(filters, None, "all")
    head = query.list_prompts(filters, None, "all", limit=3)
    tail = query.list_prompts(filters, None, "all", tail=3)
    assert head == everything[:3]
    assert tail == everything[-3:]


def test_limit_and_tail_together_are_refused(export: Path) -> None:
    query = TranscriptQuery(export)
    with pytest.raises(ValueError, match="opposite ends"):
        query.list_prompts(QueryFilters(), None, "all", limit=1, tail=1)
    for bad in (0, -1):
        with pytest.raises(ValueError, match="at least 1"):
            query.list_prompts(QueryFilters(), None, "all", limit=bad)
        with pytest.raises(ValueError, match="at least 1"):
            query.list_prompts(QueryFilters(), None, "all", tail=bad)


def test_an_ordinal_range_returns_exactly_that_range(export: Path) -> None:
    query = TranscriptQuery(export)
    records = query.list_prompts(QueryFilters(), OrdinalRange(4, 6), "all")
    assert [record["ordinal"] for record in records] == [4, 5, 6]


def test_messages_return_the_selected_prompts_and_their_replies(
    export: Path,
) -> None:
    query = TranscriptQuery(export)
    records = query.list_messages(QueryFilters(), OrdinalRange(2, 3), "all")
    kinds = [record["record_type"] for record in records]
    assert kinds.count("prompt") == 2
    assert kinds.count("response") == 2
    assert all(record["record_id"] != "response:0999" for record in records)


def test_a_seeked_message_read_agrees_with_the_exhaustive_one(export: Path) -> None:
    """``verify=True`` abandons the linkage lower bound; both paths must agree.

    The lower bound rests on a writer invariant rather than on a measurement, so the two
    readings are only equivalent while that invariant holds. Comparing them is the cheapest
    way to notice that it has stopped.
    """

    for ordinals in (OrdinalRange(1, 3), OrdinalRange(5, 8), OrdinalRange(10, 12)):
        seeked = TranscriptQuery(export).list_messages(QueryFilters(), ordinals, "all")
        exhaustive = TranscriptQuery(export, verify=True).list_messages(
            QueryFilters(), ordinals, "all"
        )
        assert seeked == exhaustive


# ---------------------------------------------------------------------------------------
# The integrity contract
# ---------------------------------------------------------------------------------------


def test_a_truncated_unconsulted_projection_is_still_refused_at_open(
    tmp_path: Path,
) -> None:
    """A damaged file no query reads is still a damaged generation.

    The size check used to live in the reader, which meant it ran only for the two
    projections a query opens -- so a truncated ``system-inputs.jsonl`` was accepted in
    silence by an open that had already been told how long the file should be.
    """

    root = _write_export(tmp_path, [_prompt(1, 100, "team-a", "owner_human", "x")], [])
    for name in ("system-inputs.jsonl", "prompts.jsonl"):
        path = root / "extracted" / "transcripts" / name
        original = path.read_bytes()
        path.write_bytes(original[: len(original) // 2])
        with pytest.raises(ValueError, match="generation is incomplete"):
            TranscriptQuery(root)
        path.write_bytes(original)
    TranscriptQuery(root)


def test_an_equal_length_rewrite_is_caught_only_by_verify(tmp_path: Path) -> None:
    """The honest limit of a size check, asserted so the docstring cannot overstate it."""

    root = _write_export(
        tmp_path,
        [_prompt(1, 100, "team-a", "owner_human", "abc")],
        [_prompt(1, 100, "team-a", "owner_human", "abc")],
    )
    path = root / "extracted" / "transcripts" / "prompts.jsonl"
    path.write_bytes(path.read_bytes().replace(b'"abc"', b'"xyz"'))
    # A slice read measures the length, which is unchanged, and returns the rewritten record.
    assert TranscriptQuery(root).list_prompts(
        QueryFilters(), OrdinalRange(1, 1), "all"
    )[0]["text"] == "xyz"
    with pytest.raises(ValueError, match="generation is incomplete"):
        TranscriptQuery(root, verify=True)


def test_a_manifest_without_byte_counts_still_gets_the_digest_check(
    tmp_path: Path,
) -> None:
    """The oldest archives must not degrade to no check at all."""

    root = _write_export(
        tmp_path,
        [_prompt(1, 100, "team-a", "owner_human", "abc")],
        [_prompt(1, 100, "team-a", "owner_human", "abc")],
        declare_bytes=False,
    )
    path = root / "extracted" / "transcripts" / "prompts.jsonl"
    path.write_bytes(path.read_bytes().replace(b'"abc"', b'"xyz"'))
    with pytest.raises(ValueError, match="generation is incomplete"):
        TranscriptQuery(root).list_prompts(QueryFilters(), OrdinalRange(1, 1), "all")


def test_an_unsorted_projection_is_refused_rather_than_under_returned(
    tmp_path: Path,
) -> None:
    """A seek over an unsorted file silently omits records; this makes it fail loudly."""

    prompts = [
        _prompt(1, 100, "team-a", "owner_human", "a"),
        _prompt(2, 500, "team-a", "owner_human", "b"),
        _prompt(3, 200, "team-a", "owner_human", "c"),
    ]
    root = _write_export(tmp_path, prompts, prompts)
    query = TranscriptQuery(root)
    with pytest.raises(ValueError, match="not ordered by timestamp_ms"):
        query.list_prompts(QueryFilters(window=_Window(50, None)), None, "all")


# ---------------------------------------------------------------------------------------
# The bundled schema-3 reader, against the package one
# ---------------------------------------------------------------------------------------


def _shard(path: Path, records: Sequence[dict[str, JsonValue]], target: int) -> None:
    write_seekable_jsonl(
        path, list(records), timestamp_key="at_ms", target_chunk_bytes=target
    )


def _plain_records(count: int) -> list[dict[str, JsonValue]]:
    return [
        {"at_ms": 1_000 + index * 10, "index": index, "text": f"record {index} " * 12}
        for index in range(count)
    ]


def test_query_chunk_reader_matches_the_package_reader(tmp_path: Path) -> None:
    """The differential the section comment promises: same records, same bytes.

    ``_ChunkedJsonlReader`` is a standalone-library copy of ``SeekableJsonlReader`` and the
    copy is pinned here rather than by a comment. Bytes read are compared as well as records,
    because a copy that answers correctly by reading the whole shard has lost the only
    property the duplication was paid for.
    """

    path = tmp_path / "shard.jsonl.gz"
    records = _plain_records(1_200)
    _shard(path, records, 1 << 14)
    mine = _ChunkedJsonlReader(path)
    assert mine.has_index
    assert mine.record_count == len(records)

    with SeekableJsonlReader(path) as theirs:
        assert list(mine.iter_records()) == list(theirs.iter_records())

    random.seed(20260824)
    for _ in range(40):
        low = random.randrange(900, 14_000)
        high = low + random.randrange(1, 3_000)
        mine = _ChunkedJsonlReader(path)
        with SeekableJsonlReader(path) as theirs:
            assert list(mine.read_range(low, high)) == list(theirs.read_range(low, high))
            assert mine.data_bytes_read == theirs.data_bytes_read
    for count in (1, 3, 20, 500):
        mine = _ChunkedJsonlReader(path)
        with SeekableJsonlReader(path) as theirs:
            assert mine.tail(count) == theirs.tail(count)
            assert mine.data_bytes_read == theirs.data_bytes_read
    for first, last in ((0, 1), (5, 5), (17, 400), (0, len(records))):
        mine = _ChunkedJsonlReader(path)
        with SeekableJsonlReader(path) as theirs:
            assert list(mine.read_lines(first, last)) == list(
                theirs.read_lines(first, last)
            )
            assert mine.data_bytes_read == theirs.data_bytes_read


def test_a_windowed_read_touches_less_than_the_shard(tmp_path: Path) -> None:
    path = tmp_path / "shard.jsonl.gz"
    _shard(path, _plain_records(4_000), 1 << 14)
    reader = _ChunkedJsonlReader(path)
    records = list(reader.read_range(1_000, 1_500))
    assert records
    assert 0 < reader.data_bytes_read < path.stat().st_size


def test_losing_the_sidecar_costs_speed_and_nothing_else(tmp_path: Path) -> None:
    """The scan path must read what the index path reads, including a many-window member.

    The member size is what broke this: the scan reads a fixed number of *compressed* bytes
    per iteration, so a member that compresses to more than one window hands the splitter a
    buffer ending mid-record. Incompressible payloads at the default target reach 796,663
    compressed bytes in one member -- three windows -- so the shard here is built from random
    bytes rather than from the well-compressing fixture above, which would never have found
    it.
    """

    random.seed(99)
    records: list[dict[str, JsonValue]] = [
        {
            "at_ms": 1_000 + index,
            "index": index,
            "text": base64.b64encode(random.randbytes(9_000)).decode("ascii"),
        }
        for index in range(200)
    ]
    indexed = tmp_path / "indexed.jsonl.gz"
    _shard(indexed, records, 1 << 20)
    with SeekableJsonlReader(indexed) as packaged:
        assert max(member.c_len for member in packaged.index.members) > (1 << 18)

    bare = tmp_path / "bare.jsonl.gz"
    bare.write_bytes(indexed.read_bytes())
    scanning = _ChunkedJsonlReader(bare)
    assert not scanning.has_index
    assert [record["index"] for record in scanning.iter_records()] == list(range(200))
    assert _ChunkedJsonlReader(bare).tail(3) == _ChunkedJsonlReader(indexed).tail(3)
    assert list(_ChunkedJsonlReader(bare).read_range(1_010, 1_015)) == list(
        _ChunkedJsonlReader(indexed).read_range(1_010, 1_015)
    )
    assert list(_ChunkedJsonlReader(bare).read_lines(5, 9)) == list(
        _ChunkedJsonlReader(indexed).read_lines(5, 9)
    )


def test_a_stale_sidecar_is_refused_rather_than_believed(tmp_path: Path) -> None:
    path = tmp_path / "shard.jsonl.gz"
    _shard(path, _plain_records(400), 1 << 13)
    sidecar = path.with_name(path.name + ".index.jsonl")
    lines = sidecar.read_text(encoding="utf-8").split("\n")
    header = json.loads(lines[0])
    header["c_size"] = header["c_size"] + 1
    lines[0] = json.dumps(header, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
    sidecar.write_text("\n".join(lines), encoding="utf-8")
    with pytest.raises(ArchiveReadError):
        _ChunkedJsonlReader(path)
