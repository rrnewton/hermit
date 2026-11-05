"""Contract tests for reading a built archive through schema 3.

Three claims carry this change, and each is asserted on evidence rather than on the reader's
own description of itself.

*The answers do not change.* Schema 3 is a cheaper way to reach the same records, so the whole
file is built around a differential: one fixture is published as schema 1, schema 2 and schema
3 at once, three archives are opened over the same tree with the newer bootstraps progressively
hidden, and every list, show, search and stats surface is required to return byte-identical
JSON from all three. A read path that is fast and different is not a read path, it is a second
archive.

*The seek is real, measured in bytes.* ``TimelineQuery.bytes_read`` is the instrument, as in
`test_query_read_paths.py`, and ``TimelineQuery.opened_shards`` is the second one: a byte count
can be small for the wrong reason, but a shard that was never opened was never read. The
strongest assertion here is not a ratio, it is that a one-team listing opens exactly one file
and that no surface ever opens the timeline stream, which is 93% of the generation.

*A partial publication is refused, not misread.* Every clause of the completeness rule gets a
test that damages the tree in exactly that way and requires the reader to say so and fall back
-- because a schema-3 generation that is read as complete when it is not returns a listing with
a team silently missing from it, which is indistinguishable from a team that did no work.

Named `test_timeline_*` rather than `test_query_*` because the packaged workflow runs
`pytest tests/test_timeline_*.py` and a file outside that glob runs only under `make validate`.
"""

from __future__ import annotations

import json
import gzip
import shutil
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

import pytest

from wrkviz.archive import JsonValue, canonical_json
from wrkviz.query import (
    SCHEMA_3_BOOTSTRAP_PATH,
    QueryFilters,
    TimelineQuery,
)
from wrkviz.timeline_shards import write_timeline_shards
from tests.timeline_legacy_generations import schema_2_writer_enabled
from wrkviz.timeline_v3 import SCHEMA_3_ROOT, write_timeline_v3


DAY_MS = 24 * 60 * 60 * 1000
#: 2026-05-04T00:00:00Z.
MIDNIGHT = 1777852800000
TEAMS = ("team-a", "team-b")

#: Keys schema 2 and schema 3 both derive and publish and schema 1 does not carry at all.
#: Named here so the three-way differential can say precisely what it excuses schema 1 for,
#: rather than excusing a whole surface and losing the rest of the comparison with it.
_DERIVED_KEYS = frozenset({"activity_start_ms", "activity_end_ms"})

#: Small enough that a team's spine spans several gzip members, so a line-range read is
#: measurably cheaper than the shard rather than merely no more expensive. The default
#: 1 MiB target would put this whole fixture in one member and make every seek assertion
#: below vacuously true -- which is the failure mode a chunk-size-independent test would
#: have hidden.
CHUNK = 1 << 12


@dataclass(frozen=True)
class _Window:
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


def _filler(seed: int, words: int) -> str:
    """Deterministic, low-redundancy prose.

    The fixture needs shards that are several gzip members long, and repeating one sentence
    would compress to nothing and leave every seek assertion below measuring an empty file.
    A linear congruential walk over a small vocabulary is reproducible and compresses like
    real text rather than like a rectangle.
    """

    vocabulary = (
        "deadline schedule rebuild latency handoff quorum backlog rollout digest "
        "shard bootstrap manifest interval concurrency envelope checksum"
    ).split()
    state = seed * 2654435761 + 1
    chosen: list[str] = []
    for _ in range(words):
        state = (state * 1103515245 + 12345) & 0x7FFFFFFF
        chosen.append(f"{vocabulary[state % len(vocabulary)]}{state % 9973}")
    return " ".join(chosen)


def _agent(team: str, index: int) -> dict[str, JsonValue]:
    return {
        "id": f"{team}::agent-{index:02d}",
        "team": team,
        "parent_id": None if index == 0 else f"{team}::agent-00",
        "path": f"/{team}/agent-{index:02d}",
        "label": f"agent {index}",
        "short_name": f"agent-{index:02d}",
        "official_name": f"/{team}/agent-{index:02d}",
        "nickname": "",
        "naming_rationale": f"named for deadline reasons {index}",
        "lifetime_summary": f"deadline {_filler(index * 7 + len(team), 90)}",
        "summary_available": True,
        "depth": 0 if index == 0 else 1,
        "start_ms": MIDNIGHT,
        "end_ms": MIDNIGHT + 2 * DAY_MS,
        "status": "done",
    }


def _phase(team: str, agent_index: int, day: int, index: int) -> dict[str, JsonValue]:
    start_ms = MIDNIGHT + day * DAY_MS + index * 3_600_000
    end_ms = start_ms + 1_800_000
    return {
        "id": f"{team}::phase-{day}-{index:02d}",
        "agent_id": f"{team}::agent-{agent_index:02d}",
        "team": team,
        "start_ms": start_ms,
        "end_ms": end_ms,
        "phrase": f"deadline work {index}",
        "paragraph": f"deadline {_filler(day * 101 + index * 13 + len(team), 90)}",
        "summary_available": True,
        "detail_path": f"data/details/{team}/phase-{day}-{index:02d}.json",
        "stats": {"events": 3},
        "states": [
            {"kind": "idle", "start_ms": start_ms, "end_ms": start_ms + 600_000},
            {"kind": "active", "start_ms": start_ms + 600_000, "end_ms": start_ms + 900_000},
            {"kind": "idle", "start_ms": start_ms + 900_000, "end_ms": end_ms},
        ],
        "artifact_ids": [],
        "output_artifact_ids": [],
    }


def _rollup(team: str, day: int) -> dict[str, JsonValue]:
    start_ms = MIDNIGHT + day * DAY_MS
    return {
        "kind": "day",
        "team": team,
        "label": f"{team} day {day}",
        "path": f"teams/{team}/summaries/daily/day-{day}.md",
        "technical_path": f"teams/{team}/summaries/daily/day-{day}.md",
        "plain_language_path": f"teams/{team}/summaries/daily/day-{day}-plain.md",
        "technical_summary_available": True,
        "plain_language_summary_available": True,
        "summary_available": True,
        "start_ms": start_ms,
        "end_ms": start_ms + DAY_MS,
        "stats": {"events": 9},
    }


def _timeline() -> dict[str, JsonValue]:
    agents: list[JsonValue] = []
    phases: list[JsonValue] = []
    edges: list[JsonValue] = []
    events: list[JsonValue] = []
    rollups: list[JsonValue] = []
    bins: list[JsonValue] = []
    for team in TEAMS:
        for index in range(24):
            agents.append(_agent(team, index))
        for day in range(2):
            rollups.append(_rollup(team, day))
            for index in range(24):
                phase = _phase(team, index % 24, day, index)
                phases.append(phase)
                start = phase["start_ms"]
                assert isinstance(start, int)
                for step in range(3):
                    events.append(
                        {
                            "agent_id": phase["agent_id"],
                            "team": team,
                            "at_ms": start + 700_000 + step * 1000,
                            "kind": "user_prompt",
                        }
                    )
                edges.append(
                    {
                        "id": f"{team}::edge-{day}-{index}",
                        "kind": "message" if index % 2 else "spawn",
                        "team": team,
                        "source_id": f"{team}::agent-00",
                        "target_id": phase["agent_id"],
                        "source_ms": start + 700_000,
                        "target_ms": start + 800_000,
                        "phrase": "said something about the deadline",
                        "paragraph": "",
                        "full_text": "a message body " * 8,
                        "content_status": "",
                    }
                )
            for hour in range(6):
                bins.append(
                    {
                        "team": team,
                        "resolution": "hourly",
                        "role": "coordinator",
                        "start_ms": MIDNIGHT + day * DAY_MS + hour * 3_600_000,
                        "end_ms": MIDNIGHT + day * DAY_MS + (hour + 1) * 3_600_000,
                        "peak_concurrency": 2,
                        "avg_active_concurrency": 0.5,
                    }
                )
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
            {
                "slug": team,
                "label": team,
                "provider": "codex",
                "projects": [],
                "hosts": [],
                "stats": {"agents": 24},
            }
            for team in TEAMS
        ],
        "agents": agents,
        "phases": phases,
        "edges": edges,
        "events": events,
        "rollups": rollups,
        "projects": [],
        "summary_files": [
            {
                "kind": "daily",
                "team": team,
                "label": f"{team} daily",
                "path": f"teams/{team}/summaries/daily/day-0.md",
                "period": "day-0",
            }
            for team in TEAMS
        ],
        "glossary": [],
        "activity_bins": bins,
        "project_overviews": [
            {
                "team": team,
                "text": "An overview mentioning the deadline.",
                "summary_available": True,
                "evidence_status": "supported",
                "model": "a-model",
                "prompt_version": "v1",
                "input_hash": "1" * 64,
            }
            for team in TEAMS
        ],
    }


def _publish(root: Path) -> dict[str, JsonValue]:
    """Write all three generations of one timeline, plus the files they point at."""

    timeline = _timeline()
    (root / "data").mkdir(parents=True, exist_ok=True)
    (root / "data" / "timeline.json").write_text(
        canonical_json(timeline), encoding="utf-8"
    )
    raw_phases = timeline["phases"]
    assert isinstance(raw_phases, list)
    for raw in raw_phases:
        assert isinstance(raw, dict)
        relative = raw["detail_path"]
        assert isinstance(relative, str)
        detail = root / relative
        detail.parent.mkdir(parents=True, exist_ok=True)
        detail.write_text(
            json.dumps(
                {
                    "phase_id": raw["id"],
                    "transcript": [
                        {"at_ms": raw["start_ms"], "role": "user", "text": "deadline?"}
                    ],
                },
                sort_keys=True,
            ),
            encoding="utf-8",
        )
    raw_rollups = timeline["rollups"]
    assert isinstance(raw_rollups, list)
    for raw in raw_rollups:
        assert isinstance(raw, dict)
        for key in ("technical_path", "plain_language_path"):
            relative = raw[key]
            assert isinstance(relative, str)
            markdown = root / relative
            markdown.parent.mkdir(parents=True, exist_ok=True)
            markdown.write_text(f"# {relative}\n\nthe deadline moved.\n", encoding="utf-8")
    # Schema 2 beside schema 3, which is the shape this suite compares across: an archive an
    # older tool wrote and a newer one rebuilt. A build no longer produces the schema-2 half, so
    # the writer is enabled for exactly the call that makes it.
    with schema_2_writer_enabled():
        write_timeline_shards(root, timeline, search_records=[])
    write_timeline_v3(root, timeline, target_chunk_bytes=CHUNK)
    return timeline


@pytest.fixture
def archives(tmp_path: Path) -> dict[str, Path]:
    """The same archive at three generations: schema 3, schema 2 and schema 1.

    Copied rather than rebuilt, so the three trees are the same bytes with the newer entry
    points removed. A differential between two independently generated fixtures would be
    testing the fixture generator.
    """

    three = tmp_path / "schema-3"
    three.mkdir()
    _publish(three)

    two = tmp_path / "schema-2"
    shutil.copytree(three, two)
    (two / SCHEMA_3_BOOTSTRAP_PATH).unlink()
    shutil.rmtree(two / SCHEMA_3_ROOT)

    one = tmp_path / "schema-1"
    shutil.copytree(two, one)
    (one / "data" / "timeline-v2.json").unlink()
    shutil.rmtree(one / "data" / "timeline-v2")

    return {"schema-3": three, "schema-2": two, "schema-1": one}


#: Every read surface, as a name and a call. Each is exercised against all three generations by
#: `test_every_surface_answers_the_same_under_all_three_generations`, so adding a surface here
#: is what makes it covered -- there is no second list to keep in step.
_SURFACES: tuple[tuple[str, Callable[[TimelineQuery], object]], ...] = (
    ("list teams", lambda q: q.list_records("teams", QueryFilters())),
    ("list agents", lambda q: q.list_records("agents", QueryFilters())),
    (
        "list agents --team team-a",
        lambda q: q.list_records("agents", QueryFilters(teams=("team-a",))),
    ),
    ("list phases", lambda q: q.list_records("phases", QueryFilters())),
    (
        "list phases --since",
        lambda q: q.list_records(
            "phases", QueryFilters(window=_Window(MIDNIGHT + DAY_MS, None))
        ),
    ),
    ("list rollups", lambda q: q.list_records("rollups", QueryFilters())),
    (
        "list agents --agent",
        lambda q: q.list_records(
            "agents", QueryFilters(agent_ref="agent:team-b::agent-03")
        ),
    ),
    ("show team", lambda q: q.show("team:team-a")),
    ("show agent", lambda q: q.show("agent:team-a::agent-00")),
    ("show phase", lambda q: q.show("phase:team-b::phase-1-05")),
    ("show phase --transcript", lambda q: q.show("phase:team-b::phase-1-05", transcript=True)),
    ("show rollup", lambda q: q.show(f"rollup:team-a::day::{MIDNIGHT}")),
    (
        "stats",
        lambda q: {k: v.to_mapping() for k, v in q.summary_stats(QueryFilters()).items()},
    ),
    (
        "stats --team team-b",
        lambda q: {
            k: v.to_mapping()
            for k, v in q.summary_stats(QueryFilters(teams=("team-b",))).items()
        },
    ),
    (
        "stats --since",
        lambda q: {
            k: v.to_mapping()
            for k, v in q.summary_stats(
                QueryFilters(window=_Window(MIDNIGHT + DAY_MS, None))
            ).items()
        },
    ),
    (
        "search summaries",
        lambda q: q.search(
            "deadline",
            scope="summaries",
            filters=QueryFilters(),
            case_sensitive=False,
            limit=50,
        ),
    ),
    (
        "search transcripts",
        lambda q: q.search(
            "deadline",
            scope="transcripts",
            filters=QueryFilters(teams=("team-a",)),
            case_sensitive=False,
            limit=50,
        ),
    ),
    ("activity_bins", lambda q: q.activity_bins(QueryFilters())),
    (
        "activity_bins --window",
        lambda q: q.activity_bins(
            QueryFilters(window=_Window(MIDNIGHT + DAY_MS, MIDNIGHT + DAY_MS + 7_200_000))
        ),
    ),
)


def _number(record: dict[str, JsonValue], key: str) -> int:
    """One integer field, narrowed, so the assertions below read as arithmetic."""

    value = record[key]
    assert isinstance(value, int) and not isinstance(value, bool), (key, value)
    return value


def _answer(root: Path, run: Callable[[TimelineQuery], object]) -> str:
    return json.dumps(run(TimelineQuery(root)), sort_keys=True, default=str)


def _strip_derived(value: object) -> object:
    """Remove the keys schema 1 never had, so it can be compared on everything else."""

    if isinstance(value, dict):
        return {
            key: _strip_derived(item)
            for key, item in value.items()
            if key not in _DERIVED_KEYS
        }
    if isinstance(value, list):
        return [_strip_derived(item) for item in value]
    return value


def _answer_without_derived(root: Path, run: Callable[[TimelineQuery], object]) -> str:
    return json.dumps(
        _strip_derived(json.loads(_answer(root, run))), sort_keys=True, default=str
    )


# ---------------------------------------------------------------------------------------
# The answers do not change
# ---------------------------------------------------------------------------------------


def test_the_fixture_really_is_read_through_three_different_generations(
    archives: dict[str, Path]
) -> None:
    """Guard the differential below, which is worthless if all three open the same file."""

    three = TimelineQuery(archives["schema-3"])
    two = TimelineQuery(archives["schema-2"])
    one = TimelineQuery(archives["schema-1"])
    assert three.schema_3_declined == ""
    assert two.schema_3_declined == "no schema-3 bootstrap"
    assert one.schema_3_declined == "no schema-3 bootstrap"
    assert three.opened_shards == ()
    assert three.bytes_read == (archives["schema-3"] / SCHEMA_3_BOOTSTRAP_PATH).stat().st_size
    assert two.bytes_read > 10 * three.bytes_read
    assert one.bytes_read > 10 * three.bytes_read


@pytest.mark.parametrize("label,run", _SURFACES, ids=[name for name, _ in _SURFACES])
def test_every_surface_answers_the_same_under_all_three_generations(
    archives: dict[str, Path], label: str, run: Callable[[TimelineQuery], object]
) -> None:
    assert _answer(archives["schema-3"], run) == _answer(archives["schema-2"], run), label
    # Schema 1 is compared with two allowances, both of them real differences between the
    # generations rather than slack in the test. It carries no `activity_start_ms` /
    # `activity_end_ms` -- schemas 2 and 3 both derive those -- so they come off both sides;
    # and it keeps whole phase records where 2 and 3 keep cards, so `show phase` returns
    # strictly more under schema 1 and is excused entirely. Everything else must match.
    if not label.startswith("show phase"):
        assert _answer_without_derived(
            archives["schema-3"], run
        ) == _answer_without_derived(archives["schema-1"], run), label


def test_show_returns_the_recorded_zoom_bounds_under_schema_three(
    archives: dict[str, Path]
) -> None:
    """The feature the bounds exist for: ``zoomToActivityRange`` needs them on the record.

    Asserted against schema 2 rather than against a literal, because "the same numbers as the
    generation that already had them" is the property, and a literal would pass just as happily
    if both were wrong.
    """

    for reference in (
        "agent:team-a::agent-03",
        "phase:team-b::phase-1-05",
        f"rollup:team-a::day::{MIDNIGHT}",
    ):
        three = TimelineQuery(archives["schema-3"]).show(reference)
        two = TimelineQuery(archives["schema-2"]).show(reference)
        assert isinstance(three["activity_start_ms"], int)
        assert isinstance(three["activity_end_ms"], int)
        assert three["activity_start_ms"] < three["activity_end_ms"]
        assert three["activity_start_ms"] == two["activity_start_ms"], reference
        assert three["activity_end_ms"] == two["activity_end_ms"], reference


def test_the_bounds_frame_the_work_and_not_the_whole_interval(
    archives: dict[str, Path]
) -> None:
    """If the bounds equalled the record's own interval they would be worth nothing.

    The fixture's phases are idle for the first ten minutes and the last fifteen of every
    thirty, so a bound that has not been derived from the state runs is visible as an exact
    match against ``start_ms``.
    """

    phase = TimelineQuery(archives["schema-3"]).show("phase:team-b::phase-1-05")
    assert _number(phase, "activity_start_ms") > _number(phase, "start_ms")
    assert _number(phase, "activity_end_ms") < _number(phase, "end_ms")


# ---------------------------------------------------------------------------------------
# The seek is real
# ---------------------------------------------------------------------------------------


def _spine(root: Path, team: str) -> str:
    return f"{SCHEMA_3_ROOT}/spine/{team}.jsonl.gz"


def test_a_one_team_listing_opens_one_shard_and_reads_part_of_it(
    archives: dict[str, Path]
) -> None:
    root = archives["schema-3"]
    query = TimelineQuery(root)
    records = query.list_records("agents", QueryFilters(teams=("team-a",)))
    assert [record["ref"] for record in records] == [
        f"agent:team-a::agent-{index:02d}" for index in range(24)
    ]
    assert query.opened_shards == (_spine(root, "team-a"),)
    shard = (root / _spine(root, "team-a")).stat().st_size
    bootstrap = (root / SCHEMA_3_BOOTSTRAP_PATH).stat().st_size
    # The line-range read, net of the bootstrap and the sidecar, is a fraction of the shard.
    # A shard is only worth seeking into if the seek beats reading it, and at this chunk size
    # the agents are the first of nine kinds in it.
    assert 0 < query.bytes_read - bootstrap < shard // 2


def test_no_read_surface_ever_opens_the_timeline_stream(
    archives: dict[str, Path]
) -> None:
    """93% of schema 3's bytes, and every question above is answered without them.

    This is the assertion that a byte count cannot make. `stats` over the whole archive is
    legitimately expensive -- it reads every team's spine and every rollup's Markdown -- so a
    ratio would not distinguish "read the spine" from "read a day of events as well". A path
    set does.
    """

    for _label, run in _SURFACES:
        query = TimelineQuery(archives["schema-3"])
        run(query)
        for opened in query.opened_shards:
            assert f"{SCHEMA_3_ROOT}/timeline/" not in opened, opened


def test_a_windowed_bin_read_seeks_rather_than_scans(archives: dict[str, Path]) -> None:
    root = archives["schema-3"]
    everything = TimelineQuery(root)
    everything.activity_bins(QueryFilters())
    windowed = TimelineQuery(root)
    selected = windowed.activity_bins(
        QueryFilters(window=_Window(MIDNIGHT + DAY_MS, MIDNIGHT + DAY_MS + 7_200_000))
    )
    assert 0 < len(selected) < 24
    assert 0 < windowed.bytes_read < everything.bytes_read


def test_resolving_one_reference_does_not_read_the_other_team(
    archives: dict[str, Path]
) -> None:
    root = archives["schema-3"]
    query = TimelineQuery(root)
    query.show("agent:team-b::agent-02")
    assert query.opened_shards == (_spine(root, "team-b"),)


# ---------------------------------------------------------------------------------------
# A partial publication is refused, not misread
# ---------------------------------------------------------------------------------------


def _declines(root: Path, fragment: str) -> None:
    """The generation is refused with *fragment* in the reason, and the answers still come."""

    query = TimelineQuery(root)
    assert query.schema_3_declined != ""
    assert fragment in query.schema_3_declined, query.schema_3_declined
    assert query.opened_shards == ()
    assert [record["ref"] for record in query.list_records("teams", QueryFilters())] == [
        "team:team-a",
        "team:team-b",
    ]


def test_a_missing_shard_is_a_partial_generation(archives: dict[str, Path]) -> None:
    root = archives["schema-3"]
    (root / _spine(root, "team-b")).unlink()
    _declines(root, "schema-3 shard is missing")


def test_a_missing_sidecar_is_a_partial_generation(archives: dict[str, Path]) -> None:
    """Not a slow read: a sidecar the bootstrap names and the tree does not have is a tree
    that has stopped matching its own manifest, and the next thing missing may be a shard."""

    root = archives["schema-3"]
    (root / (_spine(root, "team-b") + ".index.jsonl")).unlink()
    _declines(root, "schema-3 sidecar is missing")


def test_a_shard_of_the_wrong_length_is_a_partial_generation(
    archives: dict[str, Path]
) -> None:
    """The case the bootstrap's own existence cannot rule out: a rebuild that replaced a shard
    and died before replacing the bootstrap that describes it."""

    root = archives["schema-3"]
    shard = root / _spine(root, "team-a")
    shard.write_bytes(shard.read_bytes()[:-16])
    _declines(root, "the generation is partly published")


def test_a_team_with_no_spine_shard_is_a_partial_generation(
    archives: dict[str, Path]
) -> None:
    """The one damage the file checks cannot see, because the bootstrap simply stops naming it.

    Dropping a whole team from the catalogue leaves every remaining shard present and the right
    length, so only the cross-check against the inlined team list catches it -- and it is the
    most dangerous failure of the set, because reading it as complete returns a listing with a
    team quietly absent.
    """

    root = archives["schema-3"]
    path = root / SCHEMA_3_BOOTSTRAP_PATH
    bootstrap = json.loads(path.read_text(encoding="utf-8"))
    bootstrap["streams"]["spine"]["shards"] = [
        entry
        for entry in bootstrap["streams"]["spine"]["shards"]
        if entry["team"] != "team-b"
    ]
    path.write_text(json.dumps(bootstrap), encoding="utf-8")
    _declines(root, "do not cover the published teams")


def test_a_codec_this_reader_does_not_implement_is_declined(
    archives: dict[str, Path]
) -> None:
    root = archives["schema-3"]
    path = root / SCHEMA_3_BOOTSTRAP_PATH
    bootstrap = json.loads(path.read_text(encoding="utf-8"))
    bootstrap["codec"]["container"] = "zstd-seekable"
    path.write_text(json.dumps(bootstrap), encoding="utf-8")
    _declines(root, "this reader implements")


def test_a_shard_path_outside_the_schema_three_root_is_declined(
    archives: dict[str, Path]
) -> None:
    """The bootstrap is data, and the shard paths in it decide what this process opens."""

    root = archives["schema-3"]
    path = root / SCHEMA_3_BOOTSTRAP_PATH
    bootstrap = json.loads(path.read_text(encoding="utf-8"))
    bootstrap["streams"]["spine"]["shards"][0]["path"] = "../../etc/passwd"
    path.write_text(json.dumps(bootstrap), encoding="utf-8")
    _declines(root, "outside data/timeline-v3/")


def test_a_symlinked_shard_is_declined(archives: dict[str, Path]) -> None:
    root = archives["schema-3"]
    shard = root / _spine(root, "team-a")
    target = shard.with_name("elsewhere.jsonl.gz")
    shard.rename(target)
    shard.symlink_to(target.name)
    _declines(root, "schema-3 shard is missing")


def test_a_malformed_bootstrap_falls_back_rather_than_failing_the_query(
    archives: dict[str, Path]
) -> None:
    root = archives["schema-3"]
    (root / SCHEMA_3_BOOTSTRAP_PATH).write_text("{ not json", encoding="utf-8")
    _declines(root, "unreadable")


def test_a_future_schema_version_falls_back(archives: dict[str, Path]) -> None:
    root = archives["schema-3"]
    path = root / SCHEMA_3_BOOTSTRAP_PATH
    bootstrap = json.loads(path.read_text(encoding="utf-8"))
    bootstrap["schema_version"] = 4
    path.write_text(json.dumps(bootstrap), encoding="utf-8")
    _declines(root, "not a schema-3 bootstrap")


def test_an_equal_length_rewrite_is_the_documented_limit(
    archives: dict[str, Path]
) -> None:
    """The honest edge of a length check, pinned so the docstring cannot overstate it.

    Substituting one shard for another of exactly the same length passes every O(1) check, and
    is then caught one layer down by the sidecar agreement the reader makes on open -- the
    sidecar names a file, and it is not this one. What is genuinely uncaught is a rewrite of
    the *records* that preserves both length and sidecar, and that is what ``c_sha256`` in the
    bootstrap is for; verifying it means reading the shard, which is the cost schema 3 exists
    to avoid, so it is not done on the read path.
    """

    root = archives["schema-3"]
    shard = root / _spine(root, "team-a")
    original = shard.read_bytes()
    shard.write_bytes(b"\x00" * len(original))
    query = TimelineQuery(root)
    assert query.schema_3_declined == ""
    with pytest.raises(ValueError):
        query.list_records("agents", QueryFilters(teams=("team-a",)))


def test_a_search_corpus_from_another_generation_is_refused(
    archives: dict[str, Path]
) -> None:
    """The deferred cross-generation check, at the one place the two generations are mixed."""

    root = archives["schema-3"]
    path = root / "data" / "timeline-v2.json"
    bootstrap = json.loads(path.read_text(encoding="utf-8"))
    bootstrap["source_digest"] = "f" * 64
    path.write_text(json.dumps(bootstrap), encoding="utf-8")
    query = TimelineQuery(root)
    assert query.schema_3_declined == ""
    # Everything that lives wholly inside schema 3 keeps working; only the mixed operation stops.
    assert query.list_records("agents", QueryFilters())
    with pytest.raises(ValueError, match="different source generation"):
        query.search_v2(
            "deadline",
            corpus="all-transcript",
            filters=QueryFilters(),
            case_sensitive=False,
            match_mode="smart",
            sort="relevance",
            prompt_author="any",
            linkage="any",
            roles=(),
            offset=0,
            limit=5,
        )


def test_a_phase_detail_stored_only_as_gzip_is_still_resolved(
    archives: dict[str, Path]
) -> None:
    """``show`` reads the FILESYSTEM, so a stored-compressed document must resolve without HTTP.

    This is the half of the gzip-only change that the browser cannot vouch for. The server
    materialises identity bytes from the stored member on demand, so a page keeps working with no
    twin on disk -- and that made it easy to believe the whole archive was fine. The shipped
    ``timeline`` executable does not go through the server: it opens
    ``data/details/<team>/<phase>.json`` directly, and when the twin stopped being written every
    ``show <phase-ref>`` began failing with "archive file is missing".

    The regression is pinned by removing the identity file and leaving only the ``.gz``, which is
    exactly the shape a real build now produces.
    """

    archive = archives["schema-3"]
    reference = "phase:team-b::phase-1-05"
    expected = TimelineQuery(archive).show(reference)

    relative = expected["detail_path"]
    assert isinstance(relative, str) and relative.startswith("data/details/")
    identity = archive / relative
    stored = identity.with_name(identity.name + ".gz")
    if not stored.is_file():
        stored.write_bytes(gzip.compress(identity.read_bytes(), 6, mtime=0))
    identity.unlink()
    assert not identity.exists()

    # Byte-identical, not merely "does not raise": the point is that the reader cannot tell.
    assert TimelineQuery(archive).show(reference) == expected
