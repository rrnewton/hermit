"""The synthetic corpus is reproducible, ingestible, and shaped like a real one.

The browser suite is where the generated archive earns its keep: it is the only place the levels
of detail can actually be entered, because they are chosen in the browser from
milliseconds-per-pixel. What that suite cannot state cheaply is the two properties the fixture
depends on, and this file does.

**Reproducible.** A baseline that differs run to run is not a baseline. The same size and seed
must write the same bytes, and a different seed must not.

**Ingestible, and structurally real.** The generator writes provider transcripts, not a timeline;
the timeline is whatever the ordinary ingest, summarize and build path makes of them. So this
builds one, small, and asserts on the projection rather than on the generator's own report:
overlapping lifetimes, nesting below the coordinator's children, both directions of message,
phases per agent in the range a real archive shows, and gaps where the corpus stops overnight.
Asserting on the generator's intent instead would pass even if none of it survived ingestion.

Everything here uses a size far below the committed fixture's, because none of these properties
need scale -- only the level-of-detail thresholds do, and those are the browser's business.
"""

from __future__ import annotations

import json
import re
from collections import Counter
from pathlib import Path

from wrkviz.archive import JsonValue
from wrkviz.synthetic import PRESETS, SyntheticScale, build_corpus, generate_sources, run
from tests.timeline_projection import schema_1_timeline_text


#: Small enough to generate and build in about a second, large enough that nesting, overlap and
#: an overnight gap all occur. The committed fixture's size lives in `PRESETS["ci"]`.
_SMALL = SyntheticScale(teams=1, agents=14, days=3, tool_calls_per_agent=6, seed=7)

_TIMELINE_CORE = (
    Path(__file__).resolve().parents[1] / "wrkviz" / "static" / "timeline-core.js"
)


def _files(root: Path) -> dict[str, bytes]:
    return {
        path.relative_to(root).as_posix(): path.read_bytes()
        for path in sorted(root.rglob("*"))
        if path.is_file()
    }


def _objects(value: JsonValue, key: str) -> list[dict[str, JsonValue]]:
    assert isinstance(value, dict), key
    records = value.get(key)
    assert isinstance(records, list), key
    result: list[dict[str, JsonValue]] = []
    for record in records:
        assert isinstance(record, dict), key
        result.append(record)
    return result


def _int(record: dict[str, JsonValue], key: str) -> int:
    value = record.get(key)
    assert isinstance(value, int) and not isinstance(value, bool), key
    return value


def _text(record: dict[str, JsonValue], key: str) -> str:
    value = record.get(key)
    assert isinstance(value, str), key
    return value


def test_the_same_size_and_seed_write_the_same_bytes(tmp_path: Path) -> None:
    first = generate_sources(tmp_path / "first", _SMALL)
    second = generate_sources(tmp_path / "second", _SMALL)

    assert _files(tmp_path / "first") == _files(tmp_path / "second")
    assert first.teams[0].records == second.teams[0].records
    assert first.teams[0].tool_calls == second.teams[0].tool_calls


def test_a_different_seed_writes_a_different_corpus(tmp_path: Path) -> None:
    generate_sources(tmp_path / "first", _SMALL)
    generate_sources(tmp_path / "second", SyntheticScale(**{**vars(_SMALL), "seed": 8}))

    assert _files(tmp_path / "first") != _files(tmp_path / "second")


def test_the_committed_size_spans_past_the_aggregate_threshold() -> None:
    """The fixture must be wide enough in TIME, which is what picks the level of detail.

    Read from the bundle rather than restated here: a threshold copied into a test is a threshold
    that can silently stop matching the code it is about.
    """

    source = _TIMELINE_CORE.read_text(encoding="utf-8")
    match = re.search(
        r"if \(millisecondsPerPixel <= (\d+) \* 60 \* 1000\) \{\s*return \"lifetime\";",
        source,
    )
    assert match is not None, "timeline-core.js no longer states the aggregate threshold"
    aggregate_above_ms_per_pixel = int(match.group(1)) * 60 * 1000

    scale = PRESETS["ci"]
    span_ms = (scale.days - 1) * 24 * 60 * 60 * 1000
    # A conservatively wide chart. If the fitted view is aggregate on a chart this wide it is
    # aggregate on any window a reader has.
    narrowest_ms_per_pixel = span_ms / 2_000
    assert narrowest_ms_per_pixel > aggregate_above_ms_per_pixel


def test_regenerating_at_a_different_size_replaces_what_the_last_run_left(
    tmp_path: Path,
) -> None:
    """A cached fixture directory must be reusable at a new size without being cleared by hand.

    Ingest treats a transcript as append-only: a source that was rewritten is a hard error, which
    is right for a captured log and fatal for a regenerated one. A second run must therefore
    discard the first run's output rather than write over it, and an archive left holding a team
    the new size does not produce would be served as if it were real.
    """

    out = tmp_path / "cache"
    common = [
        "--out", str(out), "--days", "3", "--tool-calls-per-agent", "4", "--build",
    ]
    assert run([*common, "--agents", "9", "--seed", "1"]) == 0
    assert run([*common, "--agents", "4", "--seed", "2"]) == 0

    report: JsonValue = json.loads((out / "report.json").read_text(encoding="utf-8"))
    assert isinstance(report, dict)
    build = report.get("build")
    assert isinstance(build, dict)
    assert build["agents"] == 5

    timeline: JsonValue = json.loads(schema_1_timeline_text(out / "archive"))
    assert len(_objects(timeline, "agents")) == 5


def test_the_built_corpus_has_the_structure_the_renderer_draws(tmp_path: Path) -> None:
    corpus = generate_sources(tmp_path / "sources", _SMALL)
    counts = build_corpus(corpus, tmp_path / "archive")

    timeline: JsonValue = json.loads(schema_1_timeline_text(tmp_path / "archive"))
    agents = _objects(timeline, "agents")
    phases = _objects(timeline, "phases")
    edges = _objects(timeline, "edges")
    bins = _objects(timeline, "activity_bins")

    assert counts["agents"] == len(agents) == _SMALL.agents + 1
    assert counts["phases"] == len(phases)

    # A tree, not a fan: the coordinator's children have children of their own, which is what
    # makes the packed-lane and family-selection paths meaningful.
    depths = Counter(_int(agent, "depth") for agent in agents)
    assert depths[0] == 1
    assert depths[1] > 0
    assert depths[2] > 0

    # Overlap. Sequential lifetimes would collapse the packed track to one lane and never
    # exercise lane assignment.
    spans = sorted(
        (_int(agent, "start_ms"), _int(agent, "end_ms"))
        for agent in agents
        if _int(agent, "depth") > 0
    )
    assert any(
        later[0] < earlier[1]
        for earlier, later in zip(spans, spans[1:])
    )

    # Both directions of traffic, and the structural join a completed subagent produces.
    kinds = Counter(_text(edge, "kind") for edge in edges)
    assert kinds["spawn"] == _SMALL.agents
    assert kinds["result"] > 0
    assert kinds["message"] > 0

    # Several phases per agent, as a real corpus has; one apiece would mean every lifetime fits
    # inside a single window and phase chunking is untested.
    per_agent = Counter(_text(phase, "agent_id") for phase in phases)
    assert max(per_agent.values()) >= 3
    assert len(phases) > len(agents)

    # Overnight gaps survive as ABSENT bins. The zoomed-out view draws inactivity by leaving a
    # hole, so a corpus whose bins tile the whole range would hide that rendering entirely.
    hourly = sorted(
        (_int(record, "start_ms"), _int(record, "end_ms"))
        for record in bins
        if _text(record, "resolution") == "hourly"
    )
    assert hourly
    assert any(later[0] > earlier[1] for earlier, later in zip(hourly, hourly[1:]))
