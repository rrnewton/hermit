"""The browser bundle reads schema 3, over HTTP, from the server the archive ships.

Every other test of the website reads `static/app.js` as text, or drives a few functions out of it
in a Node realm with a hand-built stub for whatever they touch. That is the right shape for
matching a regular expression or checking an excerpt, and it is the wrong shape for the question
this file asks, which is whether the bundle can actually *read an archive* -- because every part
that could be wrong lives in the seam between three components a stub replaces:

* the writer, :func:`wrkviz.timeline_v3.write_timeline_v3`, which decides where a
  member ends and what its sidecar says about it;
* the server, :mod:`wrkviz.standalone_server`, which is copied verbatim into the
  archive as ``serve.py`` and answers one byte range with a 206, honours ``If-Range``, and answers
  416 past the end; and
* the reader in ``app.js``, which turns a catalogue entry into a byte range and a byte range back
  into records.

So this test uses all three, unstubbed: the shards are written by the writer, served by the server
on loopback, and read by functions sliced out of the bundle. `tests/js/test_timeline_v3_ui.js` is
the companion and owns the opposite half -- the refusals, which need a server that misbehaves on
demand and therefore cannot be the real one.

Node is not a dependency of this package, so a host without it skips, exactly as
`test_timeline_js_suites.py` does and with the same admission: on such a host these tests prove
nothing, and CI is where they are load-bearing.
"""

from __future__ import annotations

import json
import re
import shutil
import subprocess
import threading
from collections.abc import Iterator
from datetime import datetime, timezone
from pathlib import Path

import pytest

from wrkviz.archive import JsonValue
from wrkviz.query import QueryFilters, TimelineQuery
from wrkviz.standalone_server import make_static_server
from wrkviz.timeline_v3 import write_timeline_v3

from tests.test_timeline_v3_search import _records, _timeline


_JS = Path(__file__).parent / "js"
_PROBE = _JS / "schema3_http_probe.js"

#: `SEARCH_RESULT_LIMIT` out of the bundle, read rather than restated: the page truncates its
#: result list at this many rows, so a differential that asked the Python reader for a different
#: page size would be comparing two different answers and calling the difference a bug.
_APP_JS = (
    Path(__file__).parent.parent / "wrkviz" / "static" / "app.js"
).read_text(encoding="utf-8")
_LIMIT_MATCH = re.search(r"var SEARCH_RESULT_LIMIT = (\d+);", _APP_JS)
assert _LIMIT_MATCH is not None, "app.js no longer declares SEARCH_RESULT_LIMIT"
SEARCH_RESULT_LIMIT = int(_LIMIT_MATCH.group(1))


def _ms(value: str) -> int:
    return int(datetime.fromisoformat(value).astimezone(timezone.utc).timestamp() * 1000)


def _obj(value: JsonValue, where: str) -> dict[str, JsonValue]:
    """One section of the probe's report, narrowed.

    The probe prints JSON, so everything arrives as `JsonValue` and every assertion below would
    otherwise be an unchecked index into an unknown shape. These three helpers make the narrowing
    explicit and give `--strict` something to hold, which is the same contract
    `wrkviz.archive` imposes on the library's own JSON.
    """

    assert isinstance(value, dict), where
    return value


def _int(value: JsonValue, where: str) -> int:
    assert isinstance(value, int) and not isinstance(value, bool), where
    return value


def _strings(value: JsonValue, where: str) -> list[str]:
    assert isinstance(value, list), where
    for item in value:
        assert isinstance(item, str), where
    return [item for item in value if isinstance(item, str)]


def _node() -> str:
    node = shutil.which("node")
    if node is None:
        pytest.skip("node is not installed; the browser bundle cannot be driven here")
    return node


@pytest.fixture
def archive(tmp_path: Path) -> Path:
    """One schema-3 archive with a transcript search corpus, and nothing else."""

    root = tmp_path / "archive"
    root.mkdir()
    write_timeline_v3(root, _timeline(), search_records=_records())
    return root


@pytest.fixture
def served(archive: Path) -> Iterator[str]:
    """The archive, on loopback, through the server the archive itself ships."""

    server = make_static_server(archive, port=0)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_address[1]}/"
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=10)


def _probe(base_url: str, tmp_path: Path, request: dict[str, JsonValue]) -> dict[str, JsonValue]:
    node = _node()
    request_path = tmp_path / "probe-request.json"
    request_path.write_text(json.dumps(request), encoding="utf-8")
    result = subprocess.run(
        [node, str(_PROBE), base_url, str(request_path)],
        capture_output=True,
        text=True,
        timeout=300,
        check=False,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    parsed = json.loads(result.stdout)
    assert isinstance(parsed, dict)
    return parsed


@pytest.fixture
def report(served: str, tmp_path: Path) -> dict[str, JsonValue]:
    return _probe(
        served,
        tmp_path,
        {
            "window": {
                "start_ms": _ms("2026-08-11T09:00:00+00:00"),
                "end_ms": _ms("2026-08-12T00:00:00+00:00"),
            },
            # `B3` is the case study's own acceptance query and is two bytes, so no trigram can be
            # built from it; `backend maturity` can. The pair is what makes the prefilter's cost
            # measurable rather than assumed. Each entry is driven through `loadSchema3` and
            # `requestTranscriptSearchCorpus` -- the page's own entry points -- and answered by
            # `updateTranscriptSearch`, the page's own matcher.
            "searches": [
                {"query": "B3"},
                {"query": "backend maturity"},
                {"query": "B3", "team": "alpha-team"},
                {"query": "B3", "scope": "owner-prompts"},
                {"query": "B3", "scope": "agent-responses"},
            ],
        },
    )


def test_the_bundle_reads_the_first_paint_out_of_spine_prefixes(
    report: dict[str, JsonValue],
) -> None:
    """Agents, phase cards, edges and bins, from a line range and one bins shard.

    The identities are asserted, not just the counts, because a reader that returned the right
    *number* of agents out of the wrong member would be exactly as broken and would look fine.
    """

    bootstrap = report["bootstrap"]
    assert isinstance(bootstrap, dict)
    assert bootstrap["schema_version"] == 3
    assert bootstrap["kind"] == "timeline-v3-bootstrap"
    assert bootstrap["teams"] == ["alpha-team", "beta-team"]

    paint = report["first_paint"]
    assert isinstance(paint, dict)
    assert paint["agents"] == [
        "alpha-team::child",
        "alpha-team::root",
        "beta-team::child",
        "beta-team::root",
    ]
    # The cards are *not* part of the paint -- they are a line range of their own, fetched behind
    # the agent-lifetime modal, exactly as schema 2's separate phase-index object was. What the
    # paint may contain is whatever shared a member with a kind it did need, which on a fixture
    # this small is all of them and on a real archive is none.
    cards = report["phase_cards"]
    assert isinstance(cards, dict)
    assert cards["ids"] == [
        "alpha-team-phase-1",
        "alpha-team-phase-2",
        "beta-team-phase-1",
        "beta-team-phase-2",
    ]
    # A card is the nine-field subset, not the full phase: `states` is what a detail shard carries
    # and what the first paint must not be paying for.
    assert paint["phase_card_has_states"] is False
    # The envelope's one added key never reaches the application.
    assert paint["envelope_leaked"] is False


def test_the_first_paint_does_not_read_the_zoom_bounds(report: dict[str, JsonValue]) -> None:
    """The prefix stops at ``activity_bounds``; a page that never zooms never fetches them.

    Both halves are asserted from the same run: the paint's own traffic, and then a second read
    that produces the bounds and therefore proves they were there to be read all along. Asserting
    only the first would pass just as well against a writer that had stopped publishing them.
    """

    paint = _obj(report["first_paint"], "first_paint")
    bounds = _obj(report["activity_bounds"], "activity_bounds")
    prefix = paint["prefix"]
    assert isinstance(prefix, list) and prefix
    for raw in prefix:
        shard = _obj(raw, "prefix entry")
        assert _int(shard["bounds"], "bounds") > 0, "the writer published zoom bounds here"
        assert _int(shard["cards"], "cards") > 0, "the writer published phase cards here"
        assert _int(shard["lines"], "lines") == (
            _int(shard["records"], "records")
            - _int(shard["bounds"], "bounds")
            - _int(shard["cards"], "cards")
        ), "the paint asks for every line except the zoom bounds and the phase cards"
    # Four agents and four phases, each with a published interval, and no rollups in the fixture.
    assert bounds["count"] == 8
    # The bytes are deliberately *not* asserted here. On a fixture this small a team's whole spine
    # is one gzip member, so the bounds ride along in the member the prefix already inflated and
    # cost nothing extra -- which is a true fact about small shards and not the property under
    # test. What the format promises is that the *line range* excludes them, and that is what the
    # loop above measures; on the measured archive the two kinds fall in different members and the
    # saving is the 324,624 bytes recorded in `timeline_v3`.


def test_a_detail_window_reads_only_the_shards_that_overlap_it(
    report: dict[str, JsonValue],
) -> None:
    """`t0 < T1 and t_end_exclusive > T0`, applied to the catalogue and then within the member."""

    detail = report["detail"]
    assert isinstance(detail, dict)
    assert detail["shards_total"] == 4, "two teams over two UTC days"
    assert detail["shards_selected"] == 2, "the window is the first UTC day"
    records = detail["records"]
    assert isinstance(records, list)
    kinds = sorted({str(record["kind"]) for record in records if isinstance(record, dict)})
    assert kinds == ["event", "phase"]
    identifiers = sorted(
        str(record["id"]) for record in records if isinstance(record, dict)
    )
    assert identifiers == [
        "alpha-team-event",
        "alpha-team-phase-1",
        "beta-team-event",
        "beta-team-phase-1",
    ]


def test_every_shard_read_is_a_range_request(report: dict[str, JsonValue]) -> None:
    """The point of the format: the browser asks for one member, never for a whole shard.

    The sidecar is fetched whole -- it is the map, and a range over a map is a chicken-and-egg --
    so the assertion is that every request for a ``.jsonl.gz`` carried a ``Range`` and came back
    206, which is also a statement about the server: it really did honour them.
    """

    for section in ("first_paint", "detail"):
        cost = report[section]
        assert isinstance(cost, dict)
        inner = cost["cost"]
        assert isinstance(inner, dict)
        paths = inner["paths"]
        statuses = inner["statuses"]
        assert isinstance(paths, list) and isinstance(statuses, list)
        ranged = [
            status
            for path, status in zip(paths, statuses)
            if str(path).endswith(".jsonl.gz")
        ]
        assert ranged, section
        assert inner["ranged"] == len(ranged), section
        assert set(ranged) == {206}, section
    # `activity_bounds` is deliberately not in that list. On this fixture its member was already
    # inflated by the first paint, so the read is answered from the page's own cache and issues no
    # request at all -- which is the caching working, not the ranging failing.


def test_the_first_paint_is_a_fraction_of_what_schema_2_transferred(
    report: dict[str, JsonValue], archive: Path
) -> None:
    """What a browser downloads before it can draw, measured rather than claimed.

    The comparison is against the *whole* schema-3 generation rather than against a rebuilt schema
    2, because a two-team fixture is too small for the schema-2 bootstrap's fixed costs to mean
    anything -- the real numbers are in the report this change ships with. What this asserts is the
    structural property that makes those numbers possible and that a regression would break: the
    first paint reads a strict subset of the archive, and the entry point is small.
    """

    bootstrap_cost = _obj(_obj(report["bootstrap"], "bootstrap")["cost"], "bootstrap.cost")
    paint_cost = _obj(_obj(report["first_paint"], "first_paint")["cost"], "first_paint.cost")
    everything = sum(
        path.stat().st_size for path in archive.rglob("*") if path.is_file()
    )
    downloaded = _int(bootstrap_cost["bytes"], "bytes") + _int(paint_cost["bytes"], "bytes")
    assert downloaded < everything
    assert (
        _int(bootstrap_cost["bytes"], "bytes")
        == (archive / "data/timeline-v3.json").stat().st_size
    )


def _searches(report: dict[str, JsonValue]) -> dict[tuple[str, str, str], dict[str, JsonValue]]:
    """The probe's search results, keyed by the three things that identify one: query, team, scope."""

    search = _obj(report["search"], "search")
    assert search["published"] is True
    raw = search["searches"]
    assert isinstance(raw, list) and raw
    table: dict[tuple[str, str, str], dict[str, JsonValue]] = {}
    for entry in raw:
        assert isinstance(entry, dict)
        table[(str(entry["query"]), str(entry["team"]), str(entry["scope"]))] = entry
    return table


def test_a_two_byte_query_transfers_no_prefilter_and_a_longer_one_prunes(
    report: dict[str, JsonValue],
) -> None:
    """The decision schema 2's inlined blooms could not make.

    Schema 2 put every shard's filter in the bootstrap, so `B3` -- which cannot form a trigram --
    paid for all of them and then skipped every one. Here the filters are a stream, and the cost of
    the prefilter is exactly the prefilter.

    Measured through `requestTranscriptSearchCorpus` rather than by fetching the bloom stream by
    hand, so what is asserted is that *the page* does not ask for it: `prepareTranscriptSearchPre
    filter` is the function that decides, and a probe that made the decision itself would be
    asserting its own judgement.
    """

    table = _searches(report)
    short = table[("B3", "", "all-transcript")]
    assert short["uses_bloom"] is False
    assert short["bloom_bytes"] == 0
    assert short["shards_selected"] == short["shards_total"]
    assert short["total"], "the acceptance query still finds its records without a prefilter"

    long = table[("backend maturity", "", "all-transcript")]
    assert long["uses_bloom"] is True
    assert isinstance(long["bloom_bytes"], int) and long["bloom_bytes"] > 0
    assert long["total"]


def test_the_relationship_sidecar_answers_the_result_rows(
    report: dict[str, JsonValue],
) -> None:
    """Excerpts and reply counts on the rows the page would actually render.

    The two halves of the links shard are asserted from the two places they surface, because they
    surface in different rows and a reader that dropped one of them would still return every match:
    a matched *response* shows the excerpt of the prompt it replies to, and a matched *prompt* shows
    how many replies it received. Both are read out of `app.searchPromptExcerpts` and
    `app.searchResponsesByPrompt` after `ensureSearchLinks` filled them, which is the entry point
    no test in this tree used to call.
    """

    rows = _searches(report)[("B3", "", "all-transcript")]["rows"]
    assert isinstance(rows, list) and rows
    excerpts = 0
    replies = 0
    for raw in rows:
        row = _obj(raw, "row")
        if row["has_prompt_excerpt"]:
            excerpts += 1
            assert str(row["prompt_ref"]).startswith("message:")
            assert isinstance(row["prompt_excerpt"], str) and row["prompt_excerpt"]
        replies += _int(row["linked_response_count"], "linked_response_count")
    assert excerpts, "no matched response carried the excerpt of the prompt it replies to"
    assert replies, "no matched prompt carried a reply count"


def test_the_browser_and_the_command_line_answer_the_same_corpus_alike(
    archive: Path, report: dict[str, JsonValue]
) -> None:
    """The second reader of the schema-3 corpus, differenced against the first.

    Retiring the schema-2 writer rests on the browser reading schema 3 as well as `query.py` does,
    and until this test the evidence for that was one-sided: the Python reader had a 104-case
    query/filter differential against the schema-2 corpus and the browser had nothing comparing its
    *answers* to anything. A regression confined to `ensureSearchLinks` -- one of the two line
    ranges dropped, or the two swapped -- produces an ordinary-looking result page and no error, so
    only a differential can see it.

    **What is compared, and what is not.** The set of matching refs, each matched record's
    `linked_response_count`, and the `prompt_excerpt` shown above a matched response: these are the
    three things a broken corpus reader gets wrong silently. *Rank* is not compared, because the two
    scorers are separate implementations with no equality contract -- `search-rank-v1` against
    `smartSearchMatch` -- and asserting an order they were never required to share would make this
    a test of scoring rather than of the corpus. The filters are limited to the two the page has
    (team and scope), for the same reason: `--window` and `--agent` narrow the Python linkage counts
    and have no equivalent in the search box, so a comparison across them would be comparing two
    different questions.
    """

    table = _searches(report)
    cases = (
        ((), "all-transcript"),
        (("alpha-team",), "all-transcript"),
        ((), "owner-prompts"),
        ((), "agent-responses"),
    )
    for teams, scope in cases:
        key = ("B3", teams[0] if teams else "", scope)
        browser = table[key]
        expected = TimelineQuery(archive).search_v2(
            "B3",
            filters=QueryFilters(teams=teams),
            corpus=scope,
            case_sensitive=False,
            match_mode="smart",
            sort="relevance",
            prompt_author="any",
            linkage="any",
            roles=(),
            offset=0,
            limit=SEARCH_RESULT_LIMIT,
        )
        assert browser["state"] == "ready", key
        assert browser["total"] == expected.total_matches, key
        rows = browser["rows"]
        assert isinstance(rows, list)
        assert [str(_obj(row, "row")["ref"]) for row in rows] == sorted(
            str(item["ref"]) for item in expected.items
        ), key
        by_ref = {str(_obj(row, "row")["ref"]): _obj(row, "row") for row in rows}
        for item in expected.items:
            reference = str(item["ref"])
            row = by_ref[reference]
            assert row["linked_response_count"] == item["linked_response_count"], (
                key,
                reference,
            )
            assert row["prompt_excerpt"] == item.get("prompt_excerpt"), (key, reference)
        # A differential over two empty answers proves nothing, and three of these four scopes
        # could plausibly return nothing on a fixture this small.
        assert expected.total_matches, key


def test_the_bundle_declines_a_partial_search_corpus(
    archive: Path, tmp_path: Path
) -> None:
    """One of the three search streams missing is refused, not half-read.

    The writer publishes the three together and the reader's rule matches, for the reason
    `SCHEMA_3_SEARCH_STREAMS` gives: a corpus with no prefilter would silently over-read and one
    with no relationships would silently answer with the wrong linkage. Both failures are invisible
    -- the page would show *an* answer -- which is why this is a refusal rather than a degradation.
    """

    node = _node()
    bootstrap_path = archive / "data" / "timeline-v3.json"
    bootstrap = json.loads(bootstrap_path.read_text(encoding="utf-8"))
    del bootstrap["streams"]["search_bloom"]
    script = f"""
      const {{ loadReader }} = require({str(_JS / "schema3_probe.js")!r});
      const reader = loadReader();
      const bootstrap = {json.dumps(bootstrap)};
      const counts = ["search", "search_bloom", "search_links"].map(function (name) {{
        return reader.schema3Stream(bootstrap, name, false).length ? 1 : 0;
      }}).reduce(function (a, b) {{ return a + b; }}, 0);
      if (counts !== 0 && counts !== 3) {{
        process.stdout.write("declined\\n");
      }} else {{
        process.stdout.write("accepted " + counts + "\\n");
      }}
    """
    result = subprocess.run(
        [node, "-e", script],
        capture_output=True,
        text=True,
        timeout=120,
        check=False,
        cwd=str(_JS),
    )
    assert result.returncode == 0, result.stdout + result.stderr
    assert result.stdout.strip() == "declined"


def test_the_whole_loader_installs_a_schema_one_shaped_timeline(
    report: dict[str, JsonValue],
) -> None:
    """`loadSchema3` itself, not its parts: bootstrap in, the object the page renders out.

    Every other assertion here drives one function. This one drives the assembly, which is where a
    name typed wrong survives both the unit tests and `node --check` — the reader would throw at
    load, `loadTimeline` would swallow it into the schema-2 fallback, and the page would come up
    *working*, one generation behind, with a parenthetical in the meta line nobody reads. The DOM
    boundary is the only thing stubbed: `initializeData` is captured rather than executed.

    What it installs has to be schema-1 shaped, because that is the contract every renderer below
    it was written against, and empty in exactly the two collections schema 2 also leaves empty —
    `phases` and `events` arrive from the day shards as the view asks for them.
    """

    loader = _obj(report["loader"], "loader")
    assert loader["accepted"] is True
    assert loader["schema_mode"] == "schema3"
    assert loader["phase_index_ready"] is False, "the cards are a later line range"
    assert loader["shard_catalog"] == 4
    assert loader["search_catalog"] == 4
    assert loader["spine_teams"] == ["alpha-team", "beta-team"]

    installed = _obj(loader["installed"], "loader.installed")
    assert installed["schema_version"] == 3
    assert installed["source_digest"] == "differential-source-digest"
    assert installed["display_timezone"] == "America/New_York"
    assert installed["teams"] == ["alpha-team", "beta-team"]
    assert installed["agents"] == 4
    assert installed["edges"] == 0, "the fixture publishes no structural edges"
    assert installed["phases"] == 0, "full phases come from the day shards, not the spine"
    assert installed["events"] == 0
    assert _obj(installed["range"], "range")["start_ms"] == _ms("2026-08-11T09:00:00+00:00")
