"""Contract tests for the schema-3 write path.

Four properties carry the whole design, so they are asserted on bytes and on file listings rather
than on the writer's own report:

*Schema 3 loses nothing.* Every schema-1 record comes back out of the shards, byte for byte,
once the two envelope keys are removed. A projection that is 7% of its source is only interesting
if it is still the same records, so that is the first test in the file and the one the rest lean
on.

*A shard exists once.* The defect being removed is 2.42 GB of plain `.json` beside 0.19 GB of
`.gz`, so the test walks the generated tree and refuses any plain twin -- it does not ask the
writer whether it wrote one.

*The bootstrap stays small.* Schema 2's is 5,702,530 bytes because 2,059 activity bins are inlined
into the file a browser must read first. The test pins that bins are absent from the bootstrap and
present in a shard, which is the decision, rather than pinning a byte count, which is the weather.

*A refusal leaves nothing behind.* Publication is all-or-nothing, because a shard written before a
later refusal is named by no manifest and so can never be reaped. The test provokes the latest
refusal there is -- one raised in the spine pass, after every timeline shard would have been
written -- and asserts the tree is empty.

*Seeking works.* `SeekableJsonlReader.data_bytes_read` is the instrument, as in
`test_seekable_jsonl.py`: a windowed read must touch less than the shard, and timing would prove
nothing on a fast machine.
"""

from __future__ import annotations

import gzip
import json
import shutil
import subprocess
from collections.abc import Iterator, Mapping
from pathlib import Path

import pytest

from wrkviz.archive import (
    JsonValue,
    as_array,
    as_object,
    as_int,
    as_string,
    read_json,
)
from wrkviz.query import (
    agent_ref,
    is_schema_3_shard_name as query_is_schema_3_shard_name,
    phase_ref,
    rollup_ref,
)
from wrkviz.seekable_jsonl import SeekableJsonlReader, index_path_for
from wrkviz.timeline_shards import activity_bounds
from wrkviz.timeline_v3 import (
    agent_reference,
    phase_reference,
    rollup_reference,
    SCHEMA_3_BINS_PATH,
    SCHEMA_3_BOOTSTRAP_PATH,
    SCHEMA_3_RECORD_KIND_KEY,
    SCHEMA_3_ROOT,
    SCHEMA_3_TIMESTAMP_KEY,
    is_schema_3_shard_name,
    TimelineV3Error,
    utc_day_start,
    write_timeline_v3,
)


DAY_MS = 24 * 60 * 60 * 1000
#: 2026-05-04T00:00:00Z, so a fixture instant expressed as an offset reads as a time of day.
MIDNIGHT = 1777852800000


def _event(team: str, at_ms: int, kind: str = "user_prompt") -> dict[str, JsonValue]:
    return {"agent_id": f"{team}::a1", "at_ms": at_ms, "kind": kind, "team": team}


def _phase(team: str, start_ms: int, end_ms: int, ident: str) -> dict[str, JsonValue]:
    return {
        "id": f"{team}::phase-{ident}",
        "agent_id": f"{team}::a1",
        "start_ms": start_ms,
        "end_ms": end_ms,
        "phrase": "did a thing",
        "paragraph": "It did a thing, at length.",
        "summary_available": True,
        "detail_path": f"data/details/{team}/phase-{ident}.json",
        "stats": {"events": 3},
        # Real state objects, not a bare label. The activity bounds are derived from these --
        # an ``idle`` run contributes nothing -- so a fixture with placeholder states would let
        # the bounds be anything at all and still pass.
        "states": [
            {"kind": "idle", "start_ms": start_ms, "end_ms": start_ms + 600_000},
            {"kind": "active", "start_ms": start_ms + 600_000, "end_ms": start_ms + 900_000},
            {"kind": "idle", "start_ms": start_ms + 900_000, "end_ms": end_ms},
        ],
        "artifact_ids": [],
        "output_artifact_ids": [],
        "team": team,
    }


def _edge(
    team: str, source_ms: int, target_ms: int, ident: str, kind: str
) -> dict[str, JsonValue]:
    return {
        "id": f"{team}::edge-{ident}",
        "kind": kind,
        "source_id": f"{team}::a1",
        "target_id": f"{team}::a2",
        "source_ms": source_ms,
        "target_ms": target_ms,
        "phrase": "said something",
        "paragraph": "",
        "full_text": "the whole message body, repeated for bulk " * 4,
        "content_status": "",
        "team": team,
    }


def _agent(team: str, ident: str, start_ms: int, end_ms: int) -> dict[str, JsonValue]:
    return {
        "id": f"{team}::{ident}",
        "label": ident,
        "path": f"/{team}/{ident}",
        "depth": 0,
        "parent_id": None,
        "start_ms": start_ms,
        "end_ms": end_ms,
        "status": "done",
        "summary_available": True,
        "team": team,
    }


def _bin(team: str, start_ms: int) -> dict[str, JsonValue]:
    return {
        "team": team,
        "resolution": "hourly",
        "role": "coordinator",
        "start_ms": start_ms,
        "end_ms": start_ms + 3_600_000,
        "peak_concurrency": 2,
        "avg_active_concurrency": 0.5,
    }


def _timeline(teams: tuple[str, ...] = ("team-a", "team-b")) -> dict[str, JsonValue]:
    """A schema-1 timeline exercising every collection schema 3 knows how to project."""

    events: list[JsonValue] = []
    phases: list[JsonValue] = []
    edges: list[JsonValue] = []
    agents: list[JsonValue] = []
    bins: list[JsonValue] = []
    for offset, team in enumerate(teams):
        for day in range(2):
            base = MIDNIGHT + day * DAY_MS + offset * 1000
            events.extend(
                _event(team, base + step * 1000, kind)
                for step, kind in enumerate(("user_prompt", "agent_response", "tool_call"))
            )
            phases.append(_phase(team, base, base + 1_800_000, f"{team}-{day}"))
            edges.append(_edge(team, base + 10, base + 20, f"m{team}{day}", "message"))
            edges.append(_edge(team, base + 30, base + 40, f"s{team}{day}", "spawn"))
            bins.append(_bin(team, base - base % 3_600_000))
        agents.append(_agent(team, "a1", MIDNIGHT, MIDNIGHT + 2 * DAY_MS))
        agents.append(_agent(team, "a2", MIDNIGHT, MIDNIGHT + 2 * DAY_MS))
    return {
        "schema_version": 1,
        "generated_at": "2026-05-06T00:00:00Z",
        "source_digest": "0" * 64,
        "display_timezone": "America/New_York",
        "display_timezone_source": "explicit",
        "range": {"start_ms": MIDNIGHT, "end_ms": MIDNIGHT + 2 * DAY_MS},
        "stats": {"events": len(events)},
        "artifact_catalog_path": "data/artifacts.json",
        "glossary_path": "",
        "teams": [
            {"slug": team, "label": team, "provider": "codex", "projects": [], "hosts": []}
            for team in teams
        ],
        "agents": agents,
        "phases": phases,
        "edges": edges,
        "events": events,
        "rollups": [
            {
                "kind": "day",
                "label": f"{team} day",
                "path": f"teams/{team}/summaries/day.md",
                "start_ms": MIDNIGHT,
                "end_ms": MIDNIGHT + DAY_MS,
                "summary_available": True,
                "team": team,
            }
            for team in teams
        ],
        "projects": [
            {
                "project_id": f"{team}::project-1",
                "slug": "owner/repository",
                "host": "example.invalid",
                "url": "https://example.invalid/owner/repository",
                "evidence_ids": [],
                "team": team,
            }
            for team in teams
        ],
        "summary_files": [
            {
                "kind": "agents",
                "label": "a1",
                "path": f"teams/{team}/summaries/agents/a1.md",
                "period": "a1",
                "team": team,
            }
            for team in teams
        ],
        "glossary": [],
        "activity_bins": bins,
        "project_overviews": [
            {
                "team": team,
                "text": "An overview.",
                "summary_available": True,
                "evidence_status": "supported",
                "model": "a-model",
                "prompt_version": "v1",
                "input_hash": "1" * 64,
            }
            for team in teams
        ],
    }


def _records(timeline: dict[str, JsonValue], field: str) -> list[dict[str, JsonValue]]:
    """The fixture's own narrowing helper, so the assertions below stay readable under strict."""

    return [
        as_object(value, f"{field}[{index}]")
        for index, value in enumerate(as_array(timeline[field], field))
    ]


def _bootstrap(output: Path) -> dict[str, JsonValue]:
    root = read_json(output / SCHEMA_3_BOOTSTRAP_PATH)
    assert isinstance(root, dict)
    return root


def _shard_entries(output: Path, stream: str) -> list[dict[str, JsonValue]]:
    streams = _bootstrap(output)["streams"]
    assert isinstance(streams, dict)
    section = streams[stream]
    assert isinstance(section, dict)
    shards = section["shards"]
    assert isinstance(shards, list)
    return [entry for entry in shards if isinstance(entry, dict)]


def _shard_records(output: Path, relative: str) -> list[dict[str, JsonValue]]:
    with SeekableJsonlReader(output / relative) as reader:
        return list(reader.iter_records())


def _all_records(output: Path) -> Iterator[dict[str, JsonValue]]:
    for stream in ("timeline", "spine", "bins"):
        for entry in _shard_entries(output, stream):
            with SeekableJsonlReader(output / as_string(entry["path"], "shard path")) as reader:
                yield from reader.iter_records()


def _strip(record: dict[str, JsonValue]) -> tuple[str, str]:
    """Return the record's kind and its schema-1 form, canonically encoded.

    ``at_ms`` is removed for every kind but ``event``, which carried it in schema 1 and whose
    value the writer asserts it did not change.
    """

    kind = record[SCHEMA_3_RECORD_KIND_KEY]
    assert isinstance(kind, str)
    body = {key: value for key, value in record.items() if key != SCHEMA_3_RECORD_KIND_KEY}
    if kind != "event":
        body.pop(SCHEMA_3_TIMESTAMP_KEY, None)
    return kind, json.dumps(body, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def _canonical(record: JsonValue) -> str:
    return json.dumps(record, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def test_every_schema_one_record_survives_the_round_trip(tmp_path: Path) -> None:
    timeline = _timeline()
    write_timeline_v3(tmp_path, timeline)

    recovered: dict[str, set[str]] = {}
    for record in _all_records(tmp_path):
        kind, body = _strip(record)
        recovered.setdefault(kind, set()).add(body)

    structural = {"spawn", "continuation", "result"}
    expected = {
        "event": {_canonical(r) for r in _records(timeline, "events")},
        "phase": {_canonical(r) for r in _records(timeline, "phases")},
        "edge": {
            _canonical(r)
            for r in _records(timeline, "edges")
            if r["kind"] not in structural
        },
        "structural_edge": {
            _canonical(r) for r in _records(timeline, "edges") if r["kind"] in structural
        },
        "agent": {_canonical(r) for r in _records(timeline, "agents")},
        "rollup": {_canonical(r) for r in _records(timeline, "rollups")},
        "project": {_canonical(r) for r in _records(timeline, "projects")},
        "summary_file": {_canonical(r) for r in _records(timeline, "summary_files")},
        "project_overview": {_canonical(r) for r in _records(timeline, "project_overviews")},
        "activity_bin": {_canonical(r) for r in _records(timeline, "activity_bins")},
        "team": {_canonical(r) for r in _records(timeline, "teams")},
    }
    for kind, bodies in expected.items():
        assert recovered[kind] == bodies, kind
    # `phase_card` and `activity_bounds` are projections of `phases`, `agents` and `rollups`,
    # not collections of their own, so they are the two recovered kinds with no schema-1
    # counterpart. Every other kind must be accounted for above, and the fact that these two
    # are *derived* is exactly what keeps every line above byte-identical to schema 1: neither
    # adds a field to a record that came out of the timeline.
    assert set(recovered) - set(expected) == {"phase_card", "activity_bounds"}


def test_phase_cards_are_a_subset_of_their_phases(tmp_path: Path) -> None:
    timeline = _timeline()
    write_timeline_v3(tmp_path, timeline)
    phases = {str(record["id"]): record for record in _records(timeline, "phases")}
    cards = [
        record
        for record in _all_records(tmp_path)
        if record[SCHEMA_3_RECORD_KIND_KEY] == "phase_card"
    ]
    assert len(cards) == len(phases)
    for card in cards:
        source = phases[str(card["id"])]
        for key, value in card.items():
            if key == SCHEMA_3_RECORD_KIND_KEY:
                continue
            assert source[key] == value


def test_a_shard_exists_once_and_is_an_ordinary_gzip_file(tmp_path: Path) -> None:
    write_timeline_v3(tmp_path, _timeline())
    root = tmp_path / SCHEMA_3_ROOT
    data_files = sorted(
        path for path in root.rglob("*") if path.is_file() and path.suffix == ".gz"
    )
    assert data_files
    for path in data_files:
        # No plain twin: the whole point of "compressed only".
        assert not path.with_suffix("").exists()
        assert index_path_for(path).is_file()
        raw = path.read_bytes()
        assert raw[:2] == b"\x1f\x8b"
        text = gzip.decompress(raw).decode("utf-8")
        for line in text.splitlines():
            json.loads(line)
    gzip_binary = shutil.which("gzip")
    if gzip_binary is not None:
        for path in data_files:
            assert subprocess.run([gzip_binary, "-t", str(path)], check=False).returncode == 0


def test_the_only_plain_files_are_the_bootstrap_and_the_sidecars(tmp_path: Path) -> None:
    write_timeline_v3(tmp_path, _timeline())
    plain = sorted(
        path.relative_to(tmp_path).as_posix()
        for path in tmp_path.rglob("*")
        if path.is_file() and path.suffix != ".gz"
    )
    assert all(name.endswith(".index.jsonl") for name in plain if name != SCHEMA_3_BOOTSTRAP_PATH)
    assert SCHEMA_3_BOOTSTRAP_PATH in plain


def test_activity_bins_are_a_shard_and_not_inlined_in_the_bootstrap(tmp_path: Path) -> None:
    timeline = _timeline()
    write_timeline_v3(tmp_path, timeline)
    root = _bootstrap(tmp_path)
    assert "activity_bins" not in root
    # The teams ARE inlined: a first frame cannot be drawn without them.
    assert isinstance(root["teams"], list) and len(root["teams"]) == 2
    entries = _shard_entries(tmp_path, "bins")
    assert len(entries) == 1
    assert entries[0]["path"] == SCHEMA_3_BINS_PATH
    assert entries[0]["records"] == len(_records(timeline, "activity_bins"))


def test_a_build_clears_the_root_of_everything_outside_its_own_plan(tmp_path: Path) -> None:
    """The publisher owns ``data/timeline-v3/``, so nothing else has to guess about it.

    The two files planted here are what a build that died before writing its bootstrap leaves
    behind, and they are indistinguishable on disk from what a retired team leaves behind. No
    reader and no sweeper can tell those apart; only the build can, because only the build knows
    what it just published. So it removes them here, and empties the directory that held them --
    which is what lets the reader treat a leftover as an interrupted build and `gc` refuse to
    sweep one.
    """

    timeline = _timeline()
    write_timeline_v3(tmp_path, timeline)
    stray_dir = tmp_path / SCHEMA_3_ROOT / "timeline" / "retired-team"
    stray_dir.mkdir(parents=True)
    (stray_dir / "2026-05-04.jsonl.gz").write_bytes(b"\x1f\x8b" + b"0" * 32)
    (stray_dir / "2026-05-04.jsonl.gz.index.jsonl").write_text("{}\n", encoding="utf-8")
    # Not shard-shaped, so not this module's to remove.
    note = tmp_path / SCHEMA_3_ROOT / "NOTES.txt"
    note.write_text("left by an operator\n", encoding="utf-8")

    report = write_timeline_v3(tmp_path, timeline)

    assert report.removed_files == (
        f"{SCHEMA_3_ROOT}/timeline/retired-team/2026-05-04.jsonl.gz",
        f"{SCHEMA_3_ROOT}/timeline/retired-team/2026-05-04.jsonl.gz.index.jsonl",
    )
    assert report.files_changed == 2
    assert not stray_dir.exists()
    assert note.is_file()
    # Idempotent: with nothing left to remove the next build changes nothing at all.
    assert write_timeline_v3(tmp_path, timeline).removed_files == ()


def test_the_reader_and_the_writer_agree_on_what_a_shard_is_called(tmp_path: Path) -> None:
    """`query.py` ships standalone and cannot import this module, so the predicate is written
    twice. They must agree, and in particular the reader must not recognise a shape the writer
    declines to remove -- that combination is a generation that declines forever and that no
    rebuild can repair."""

    del tmp_path
    for name in (
        "2026-05-04.jsonl.gz",
        "2026-05-04.jsonl.gz.index.jsonl",
        "team-a.jsonl.gz",
        "bins.jsonl.gz",
        "NOTES.txt",
        "timeline.json",
        ".index.jsonl",
        "",
    ):
        assert is_schema_3_shard_name(name) is query_is_schema_3_shard_name(name), name


def test_two_builds_over_identical_input_produce_identical_bytes(tmp_path: Path) -> None:
    timeline = _timeline()
    first = tmp_path / "first"
    second = tmp_path / "second"
    write_timeline_v3(first, timeline)
    write_timeline_v3(second, timeline)
    left = {
        path.relative_to(first).as_posix(): path.read_bytes()
        for path in first.rglob("*")
        if path.is_file()
    }
    right = {
        path.relative_to(second).as_posix(): path.read_bytes()
        for path in second.rglob("*")
        if path.is_file()
    }
    assert left == right
    # And a rebuild in place must not churn a single file.
    assert write_timeline_v3(first, timeline).files_changed == 0


def test_a_new_day_rewrites_only_that_day(tmp_path: Path) -> None:
    """The reason the axis cuts at the day rather than at the team."""

    timeline = _timeline(("team-a",))
    early = dict(timeline)
    cutoff = MIDNIGHT + DAY_MS
    for field, instant in (
        ("events", "at_ms"),
        ("phases", "start_ms"),
        ("edges", "source_ms"),
    ):
        early[field] = [
            record
            for record in _records(timeline, field)
            if as_int(record[instant], instant) < cutoff
        ]
    write_timeline_v3(tmp_path, early)
    before = {
        path.relative_to(tmp_path).as_posix(): path.read_bytes()
        for path in tmp_path.rglob("*")
        if path.is_file()
    }
    write_timeline_v3(tmp_path, timeline)
    after = {
        path.relative_to(tmp_path).as_posix(): path.read_bytes()
        for path in tmp_path.rglob("*")
        if path.is_file()
    }
    rewritten = {name for name, data in after.items() if before.get(name) != data}
    day_one = "data/timeline-v3/timeline/team-a/2026-05-04.jsonl.gz"
    assert day_one in before
    assert day_one not in rewritten
    assert "data/timeline-v3/timeline/team-a/2026-05-05.jsonl.gz" in rewritten


def test_days_are_cut_in_utc_and_not_in_the_display_timezone(tmp_path: Path) -> None:
    """A record at 20:30 New York time is already tomorrow in UTC, and shards where UTC says."""

    timeline = _timeline(("team-a",))
    late = MIDNIGHT + DAY_MS - 1000  # 23:59:59Z, which is 19:59:59 in America/New_York
    over = MIDNIGHT + DAY_MS + 1000  # 00:00:01Z, which is 20:00:01 the previous local day
    timeline["events"] = [_event("team-a", late), _event("team-a", over)]
    timeline["phases"] = []
    timeline["edges"] = []
    write_timeline_v3(tmp_path, timeline)
    days = sorted(as_string(entry["day"], "day") for entry in _shard_entries(tmp_path, "timeline"))
    assert days == ["2026-05-04", "2026-05-05"]
    assert utc_day_start(over) == MIDNIGHT + DAY_MS


def test_a_timeline_shard_is_bisectable_and_a_window_read_is_cheaper_than_the_shard(
    tmp_path: Path,
) -> None:
    timeline = _timeline(("team-a",))
    base = MIDNIGHT
    # Enough bulk that the shard is cut into several members, so a window can skip some.
    timeline["events"] = [_event("team-a", base + step * 1000) for step in range(20000)]
    timeline["phases"] = []
    timeline["edges"] = []
    timeline["activity_bins"] = []
    write_timeline_v3(tmp_path, timeline, target_chunk_bytes=1 << 14)
    entry = _shard_entries(tmp_path, "timeline")[0]
    assert entry["timestamps_sorted"] is True
    path = tmp_path / as_string(entry["path"], "shard path")
    with SeekableJsonlReader(path) as reader:
        assert reader.index.timestamps_sorted
        window = list(reader.read_range(base + 5_000_000, base + 5_010_000))
        assert [int(str(record["at_ms"])) for record in window] == [
            base + step * 1000 for step in range(5000, 5010)
        ]
        assert reader.data_bytes_read < path.stat().st_size // 4


def test_the_spine_is_addressed_by_line_range_and_carries_no_instant(tmp_path: Path) -> None:
    write_timeline_v3(tmp_path, _timeline())
    entry = next(e for e in _shard_entries(tmp_path, "spine") if e["team"] == "team-a")
    ranges = entry["line_ranges"]
    assert isinstance(ranges, dict)
    # No time axis: nothing to bisect, nothing for `read_range` to half-answer.
    assert entry["t0"] is None and entry["t1"] is None
    assert entry["timestamps_sorted"] is False
    with SeekableJsonlReader(tmp_path / as_string(entry["path"], "shard path")) as reader:
        assert list(reader.read_range(None, None)) == []
        for kind, span in ranges.items():
            assert isinstance(span, list)
            first, count = as_int(span[0], "l0"), as_int(span[1], "n")
            selected = list(reader.read_lines(first, first + count))
            assert len(selected) == count
            assert {record[SCHEMA_3_RECORD_KIND_KEY] for record in selected} == {kind}
            assert all(SCHEMA_3_TIMESTAMP_KEY not in record for record in selected)


def test_a_shard_survives_losing_its_index(tmp_path: Path) -> None:
    write_timeline_v3(tmp_path, _timeline(("team-a",)))
    entry = _shard_entries(tmp_path, "timeline")[0]
    path = tmp_path / as_string(entry["path"], "shard path")
    with SeekableJsonlReader(path) as reader:
        indexed = list(reader.iter_records())
    index_path_for(path).unlink()
    with SeekableJsonlReader(path) as reader:
        assert not reader.has_index
        assert list(reader.iter_records()) == indexed


def test_t_end_exclusive_reports_a_record_reaching_past_its_own_shard(
    tmp_path: Path,
) -> None:
    """A record is filed once, by its start; the bootstrap says how far it reaches."""

    timeline = _timeline(("team-a",))
    spanning_start = MIDNIGHT + DAY_MS - 60_000
    timeline["events"] = [_event("team-a", spanning_start)]
    timeline["phases"] = [
        _phase("team-a", spanning_start, spanning_start + 1_800_000, "spanning")
    ]
    timeline["edges"] = []
    write_timeline_v3(tmp_path, timeline)
    entries = _shard_entries(tmp_path, "timeline")
    assert [entry["day"] for entry in entries] == ["2026-05-04"]
    entry = entries[0]
    assert entry["records"] == 2
    assert entry["t_end_exclusive"] == spanning_start + 1_800_000
    assert as_int(entry["t_end_exclusive"], "t_end_exclusive") > MIDNIGHT + DAY_MS


def test_the_published_shard_selection_rule_admits_every_record_in_the_window(
    tmp_path: Path,
) -> None:
    """`t0 < T1 and t_end_exclusive > T0` never skips a shard holding a live record.

    Swept rather than spot-checked, because the failure this pins was an off-by-one on one
    record kind only: an event's reach was published as its own instant, so a window starting
    exactly at that instant evaluated `t_end_exclusive > T0` as false and dropped the shard.
    Each kind is given its own single-record shard and each shard is probed at every instant
    from one before its first record to one past its last.
    """

    kinds: dict[str, dict[str, JsonValue]] = {
        "event": {"events": [_event("team-a", MIDNIGHT + 5_000)]},
        "phase": {"phases": [_phase("team-a", MIDNIGHT + 5_000, MIDNIGHT + 9_000, "p")]},
        "edge": {
            "edges": [
                _edge("team-a", MIDNIGHT + 5_000, MIDNIGHT + 9_000, "e1", "message")
            ]
        },
    }
    for kind, override in kinds.items():
        root = tmp_path / kind
        timeline = _timeline(("team-a",))
        for field in ("events", "phases", "edges"):
            timeline[field] = []
        timeline.update(override)
        write_timeline_v3(root, timeline)
        entry = _shard_entries(root, "timeline")[0]
        t0 = as_int(entry["t0"], "t0")
        reach = as_int(entry["t_end_exclusive"], "t_end_exclusive")
        records = _shard_records(root, str(entry["path"]))
        for probe in range(MIDNIGHT + 4_000, MIDNIGHT + 11_000):
            # The window is the single millisecond [probe, probe + 1).
            selected = t0 < probe + 1 and reach > probe
            live = any(_is_live(record, probe) for record in records)
            assert selected or not live, (
                f"{kind}: shard skipped at {probe} while holding a live record"
            )


def _is_live(record: Mapping[str, JsonValue], probe: int) -> bool:
    """Whether *record* occupies the instant *probe*, under schema 3's half-open reading."""

    kind = record["record_kind"]
    if kind == "event":
        return as_int(record["at_ms"], "at_ms") == probe
    if kind == "phase":
        return (
            as_int(record["start_ms"], "start_ms")
            <= probe
            < as_int(record["end_ms"], "end_ms")
        )
    if kind == "edge":
        return probe in (
            as_int(record["source_ms"], "source_ms"),
            as_int(record["target_ms"], "target_ms"),
        )
    raise AssertionError(f"unclassified record kind {kind!r}")


def test_a_single_team_timeline_needs_no_team_field_on_its_records(tmp_path: Path) -> None:
    timeline = _timeline(("team-a",))
    for field in ("events", "phases", "edges", "agents", "rollups", "activity_bins"):
        timeline[field] = [
            {key: value for key, value in record.items() if key != "team"}
            for record in _records(timeline, field)
        ]
    write_timeline_v3(tmp_path, timeline)
    entries = _shard_entries(tmp_path, "timeline")
    assert {entry["team"] for entry in entries} == {"team-a"}


def test_a_record_without_a_team_is_refused_when_the_timeline_has_several(
    tmp_path: Path,
) -> None:
    timeline = _timeline()
    timeline["events"] = [
        {key: value for key, value in record.items() if key != "team"}
        for record in _records(timeline, "events")
    ]
    with pytest.raises(TimelineV3Error, match="more than one team"):
        write_timeline_v3(tmp_path, timeline)


def test_an_unclassified_schema_one_field_is_refused_rather_than_dropped(
    tmp_path: Path,
) -> None:
    timeline = _timeline()
    timeline["annotations"] = [{"id": "x"}]
    with pytest.raises(TimelineV3Error, match="unclassified schema-1 fields"):
        write_timeline_v3(tmp_path, timeline)
    assert not (tmp_path / SCHEMA_3_BOOTSTRAP_PATH).exists()


def test_a_refusal_raised_in_the_spine_pass_leaves_no_shard_behind(
    tmp_path: Path,
) -> None:
    """Publication is all-or-nothing, and the latest possible refusal is the one that proves it.

    A rollup naming a team absent from ``teams[]`` is caught in the spine pass, which runs
    after every timeline shard would have been written. Those shards would be named by no
    manifest -- the caller aborts before writing its export manifest -- so the stale-file
    removal that diffs ``previous - current`` could never reap them, and neither could any
    later build. The assertion is on the tree, not on the report, because the report is
    exactly what a failed call does not return.
    """

    timeline = _timeline(("team-a",))
    rollups = _records(timeline, "rollups")
    rollups.append({**rollups[0], "team": "ghost-team", "path": "teams/ghost/day.md"})
    timeline["rollups"] = list(rollups)
    with pytest.raises(TimelineV3Error, match="absent from teams"):
        write_timeline_v3(tmp_path, timeline)
    assert sorted(path for path in tmp_path.rglob("*")) == []


def test_a_symlinked_team_directory_is_refused_before_any_shard_is_written(
    tmp_path: Path,
) -> None:
    """Path safety is checked in the same pre-pass, for the same reason.

    ``team-b``'s directory is a symlink, and ``team-a``'s shards sort first. Discovering the
    symlink while publishing would leave team-a's shards orphaned exactly as a late projection
    refusal would.
    """

    archive = tmp_path / "archive"
    elsewhere = tmp_path / "elsewhere"
    elsewhere.mkdir()
    (archive / SCHEMA_3_ROOT / "timeline").mkdir(parents=True)
    (archive / SCHEMA_3_ROOT / "timeline" / "team-b").symlink_to(
        elsewhere, target_is_directory=True
    )
    with pytest.raises(TimelineV3Error, match="symlink"):
        write_timeline_v3(archive, _timeline())
    assert not list((archive / SCHEMA_3_ROOT / "timeline" / "team-a").glob("*"))


def test_the_envelope_refuses_to_overwrite_a_field_the_record_already_has(
    tmp_path: Path,
) -> None:
    timeline = _timeline(("team-a",))
    timeline["events"] = [{**_event("team-a", MIDNIGHT), SCHEMA_3_RECORD_KIND_KEY: "mine"}]
    with pytest.raises(TimelineV3Error, match="would overwrite"):
        write_timeline_v3(tmp_path, timeline)


def test_a_record_whose_own_instant_disagrees_with_the_projection_is_refused(
    tmp_path: Path,
) -> None:
    timeline = _timeline(("team-a",))
    timeline["phases"] = [
        {**_phase("team-a", MIDNIGHT, MIDNIGHT + 1000, "p"), SCHEMA_3_TIMESTAMP_KEY: MIDNIGHT + 5}
    ]
    with pytest.raises(TimelineV3Error, match="places it at"):
        write_timeline_v3(tmp_path, timeline)


@pytest.mark.parametrize("slug", ["../escape", "a/b", "", ".hidden", "team a"])
def test_a_team_slug_that_is_not_a_path_component_is_refused(
    tmp_path: Path, slug: str
) -> None:
    timeline = _timeline(("team-a",))
    timeline["teams"] = [{"slug": slug, "label": slug, "projects": [], "hosts": []}]
    with pytest.raises(TimelineV3Error, match="safe path component"):
        write_timeline_v3(tmp_path, timeline)


def test_a_symlinked_shard_directory_is_refused(tmp_path: Path) -> None:
    elsewhere = tmp_path / "elsewhere"
    elsewhere.mkdir()
    (tmp_path / "archive" / "data").mkdir(parents=True)
    (tmp_path / "archive" / SCHEMA_3_ROOT).symlink_to(elsewhere, target_is_directory=True)
    with pytest.raises(TimelineV3Error, match="symlink"):
        write_timeline_v3(tmp_path / "archive", _timeline(("team-a",)))


def test_a_schema_two_timeline_is_refused(tmp_path: Path) -> None:
    timeline = _timeline()
    timeline["schema_version"] = 2
    with pytest.raises(TimelineV3Error, match="schema-1 source timeline"):
        write_timeline_v3(tmp_path, timeline)


def test_the_report_names_every_file_it_wrote(tmp_path: Path) -> None:
    report = write_timeline_v3(tmp_path, _timeline())
    on_disk = {
        path.relative_to(tmp_path).as_posix() for path in tmp_path.rglob("*") if path.is_file()
    }
    assert set(report.generated_files) == on_disk
    assert report.total_bytes == sum((tmp_path / name).stat().st_size for name in on_disk)


# ---------------------------------------------------------------------------------------
# The zoom bounds
# ---------------------------------------------------------------------------------------


def _bounds_by_ref(output: Path) -> dict[str, tuple[int, int]]:
    table: dict[str, tuple[int, int]] = {}
    for record in _all_records(output):
        if record[SCHEMA_3_RECORD_KIND_KEY] != "activity_bounds":
            continue
        reference = as_string(record["ref"], "activity bounds ref")
        table[reference] = (
            as_int(record["activity_start_ms"], "activity_start_ms"),
            as_int(record["activity_end_ms"], "activity_end_ms"),
        )
    return table


def test_the_published_bounds_are_the_numbers_schema_two_publishes(
    tmp_path: Path,
) -> None:
    """The differential that keeps two generations from disagreeing about where to zoom.

    Schema 2 attaches these to every agent, phase card and rollup; schema 3 publishes them as
    a spine kind of their own. The two must be the *same* numbers, or "zoom to agent lifetime"
    would frame a different window depending on which generation the browser happened to load,
    and nothing would report that as an error. Both sides come from
    `timeline_shards.activity_bounds`, and this is the assertion that keeps it that way.
    """

    timeline = _timeline()
    write_timeline_v3(tmp_path, timeline)
    published = _bounds_by_ref(tmp_path)

    phase_bounds, agent_bounds, rollup_bounds = activity_bounds(timeline)
    expected: dict[str, tuple[int, int]] = {}
    for record in _records(timeline, "agents"):
        team = as_string(record["team"], "agent.team")
        identifier = as_string(record["id"], "agent.id")
        expected[agent_reference(team, identifier)] = agent_bounds[identifier]
    for record in _records(timeline, "phases"):
        team = as_string(record["team"], "phase.team")
        identifier = as_string(record["id"], "phase.id")
        expected[phase_reference(team, identifier)] = phase_bounds[identifier]
    for index, record in enumerate(_records(timeline, "rollups")):
        team = as_string(record["team"], "rollup.team")
        expected[
            rollup_reference(
                team,
                as_string(record["kind"], "rollup.kind"),
                as_int(record["start_ms"], "rollup.start_ms"),
            )
        ] = rollup_bounds[index]

    assert published == expected
    assert len(published) == (
        len(_records(timeline, "agents"))
        + len(_records(timeline, "phases"))
        + len(_records(timeline, "rollups"))
    )


def test_the_bounds_are_narrower_than_the_records_they_describe(tmp_path: Path) -> None:
    """Otherwise there would be nothing to publish: the record already carries its interval."""

    timeline = _timeline()
    write_timeline_v3(tmp_path, timeline)
    published = _bounds_by_ref(tmp_path)
    narrower = 0
    for record in _records(timeline, "phases"):
        team = as_string(record["team"], "phase.team")
        reference = phase_reference(team, as_string(record["id"], "phase.id"))
        start, end = published[reference]
        assert as_int(record["start_ms"], "start") <= start < end
        assert end <= as_int(record["end_ms"], "end")
        if end - start < as_int(record["end_ms"], "end") - as_int(record["start_ms"], "start"):
            narrower += 1
    assert narrower == len(_records(timeline, "phases"))


def test_the_bounds_are_the_last_thing_in_a_spine_shard(tmp_path: Path) -> None:
    """A reader that never zooms must never pay for them.

    The line ranges are what make that true, and their *order* is what makes the ranges cheap:
    the kind a first frame needs is at line 0 and the kind only an interaction needs is at the
    end, so the two fall in different gzip members as soon as a shard has more than one.
    """

    write_timeline_v3(tmp_path, _timeline())
    for entry in _shard_entries(tmp_path, "spine"):
        ranges = entry["line_ranges"]
        assert isinstance(ranges, dict)
        bounds = ranges["activity_bounds"]
        assert isinstance(bounds, list)
        first = as_int(bounds[0], "activity_bounds first")
        for kind, span in ranges.items():
            assert isinstance(span, list)
            if kind != "activity_bounds":
                assert as_int(span[0], f"{kind} first") < first
        assert first + as_int(bounds[1], "activity_bounds count") == as_int(
            entry["records"], "records"
        )


def test_the_writers_references_are_the_readers_references(tmp_path: Path) -> None:
    """The duplication `timeline_v3` documents, pinned rather than promised.

    `query.py` is copied verbatim into every archive as the `./timeline` executable and may
    import nothing but the standard library, so the reader cannot import the writer's reference
    builders. What it can do is fail when they disagree.
    """

    timeline = _timeline()
    for record in _records(timeline, "agents"):
        team = as_string(record["team"], "agent.team")
        assert agent_reference(team, as_string(record["id"], "agent.id")) == agent_ref(record)
    for record in _records(timeline, "phases"):
        team = as_string(record["team"], "phase.team")
        assert phase_reference(team, as_string(record["id"], "phase.id")) == phase_ref(record)
    for record in _records(timeline, "rollups"):
        team = as_string(record["team"], "rollup.team")
        assert rollup_reference(
            team,
            as_string(record["kind"], "rollup.kind"),
            as_int(record["start_ms"], "rollup.start_ms"),
        ) == rollup_ref(record)
