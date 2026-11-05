"""The schema-3 transcript search corpus, proved equal to the schema-2 one it replaces.

A port of a search corpus is not a feature to spot-check. The two generations are read by the
same `search_v2`, so the only thing that can distinguish them is what
`query.TimelineQuery._iter_search_records` and `_search_link_context` hand it -- and a difference
there is invisible in the shape of the output and visible only in *which* records come back, on
*some* queries. So the contract asserted here is a differential: one corpus, two archives, a
battery of queries, and full equality of the answer.

The battery is built from the failure modes the case study in
`wrkviz/ai_docs/TRANSCRIPT_SEARCH_CASE_STUDY.md` actually recorded, rather than from
the axes of the API:

* its acceptance query, ``backend maturity B3``, and the ``B3`` that motivated it -- two UTF-8
  bytes, so no trigram prefilter can act on it and every selected shard is scanned. Under schema 3
  that query reads no Bloom data at all, which is a different *cost* and must be the same
  *answer*;
* the Unicode trap -- "Unicode case equivalences did not match the ASCII Bloom normalization".
  The Kelvin sign folds to ``k`` under Python's ``re.IGNORECASE`` and does not under the corpus's
  portable ASCII-only rule, so a filter built one way and a matcher compiled the other way would
  prune a shard that holds a match. Both generations must agree, and must agree with the matcher;
* the pruning trap -- "text-shard pruning could hide a previous-day prompt or later-day response".
  A term that appears only on the second day prunes the first day's shard, and the answer still
  has to carry the excerpt of the prompt that lived in the pruned shard;
* ``prompt_in_scope`` -- a sliced export retains a ``prompt_ref`` whose prompt is outside the
  range. It is a field of the record and the port must not quietly drop it;
* the receiver-side Codex return -- a child's final answer recorded on the parent's rollout and
  attributed to the child.

The second archive is the first one with its schema-3 search streams removed, which is exactly
what an archive built before those streams looks like, and it is the reason the comparison is
sound: everything else about the two -- the same records, the same agents, the same phases, the
same digests -- is byte-identical, so a difference in an answer can only have come from the
corpus that answered it.
"""

from __future__ import annotations

import gzip
import json
import shutil
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

import pytest

from wrkviz.archive import JsonValue, as_object, read_json
from wrkviz.query import ArchiveReadError
from wrkviz.query import QueryFilters, SearchResults, TimelineQuery
from wrkviz.timeline_shards import (
    SCHEMA_2_BOOTSTRAP_PATH,
    write_timeline_shards,
)
from wrkviz.seekable_jsonl import write_seekable_jsonl
from tests.timeline_legacy_generations import schema_2_writer_enabled
from wrkviz.timeline_v3 import (
    SCHEMA_3_BOOTSTRAP_PATH,
    SCHEMA_3_SEARCH_BLOOM_ROOT,
    SCHEMA_3_SEARCH_LINKS_ROOT,
    SCHEMA_3_SEARCH_ROOT,
    SCHEMA_3_SEARCH_STREAMS,
    write_timeline_v3,
)


@dataclass(frozen=True)
class _Window:
    """The half-open window `QueryFilters` takes, spelled out because the reader's is a Protocol."""

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


_DAY_ONE = "2026-08-11"
_DAY_TWO = "2026-08-12"


def _ms(value: str) -> int:
    return int(datetime.fromisoformat(value).astimezone(timezone.utc).timestamp() * 1000)


_START = _ms("2026-08-11T09:00:00+00:00")
_END = _ms("2026-08-12T23:00:00+00:00")


def _agent(team: str, identifier: str, parent: str | None = None) -> dict[str, JsonValue]:
    """One agent, with the ``<team>::`` prefix a combined export puts on every identifier.

    Not decoration: schema 2's global object refuses a duplicate *exact* agent id across teams, so
    an unqualified ``root`` in two teams is an archive neither generation would ever produce. The
    reference the reader builds strips the prefix again, so ``agent:alpha-team::root`` either way.
    """

    return {
        "id": f"{team}::{identifier}",
        "team": team,
        "parent_id": None if parent is None else f"{team}::{parent}",
        "agent_path": f"/{identifier}",
        "start_ms": _START,
        "end_ms": _END,
    }


def _phase(team: str, identifier: str, agent: str, start: int, end: int) -> dict[str, JsonValue]:
    return {
        "id": identifier,
        "team": team,
        "agent_id": f"{team}::{agent}",
        "start_ms": start,
        "end_ms": end,
        "phrase": "Work",
        "paragraph": "Work happened.",
        "summary_available": True,
        "detail_path": f"details/{team}/{identifier}.json",
        "stats": {"tool_calls": 1},
        "states": [],
    }


def _timeline() -> dict[str, JsonValue]:
    """Two teams over two UTC days, which is the smallest shape that can shard wrongly."""

    teams = ("alpha-team", "beta-team")
    agents: list[JsonValue] = []
    phases: list[JsonValue] = []
    events: list[JsonValue] = []
    for team in teams:
        agents.append(_agent(team, "root"))
        agents.append(_agent(team, "child", "root"))
        phases.append(
            _phase(team, f"{team}-phase-1", "root", _START, _ms("2026-08-12T00:00:00+00:00"))
        )
        phases.append(
            _phase(team, f"{team}-phase-2", "child", _ms("2026-08-12T00:00:00+00:00"), _END)
        )
        events.append(
            {
                "id": f"{team}-event",
                "team": team,
                "agent_id": f"{team}::root",
                "at_ms": _START,
                "kind": "user_prompt",
                "text": "an event the presentation timeline carries",
            }
        )
    return {
        "schema_version": 1,
        "generated_at": "2026-08-12T23:00:00Z",
        "source_digest": "differential-source-digest",
        "display_timezone": "America/New_York",
        "display_timezone_source": "explicit",
        "range": {"start_ms": _START, "end_ms": _END},
        "teams": [{"slug": slug, "label": slug} for slug in teams],
        "agents": agents,
        "phases": phases,
        "edges": [],
        "events": events,
        "rollups": [],
        "activity_bins": [],
        "summary_files": [],
        "glossary": [],
        "projects": [],
    }


def _record(
    team: str,
    identifier: str,
    *,
    record_type: str,
    role: str,
    at: str,
    text: str,
    agent: str = "root",
    author_kind: str | None = "agent",
    prompt_ref: str | None = None,
    prompt_author_kind: str | None = None,
    prompt_at_ms: int | None = None,
    prompt_in_scope: bool | None = None,
    kind: str = "message",
) -> dict[str, JsonValue]:
    reference = f"{kind}:{team}::{identifier}"
    return {
        "schema_version": 1,
        "ref": reference,
        "record_type": record_type,
        "role": role,
        "team": team,
        "agent_id": f"{team}::{agent}",
        "agent_ref": f"agent:{team}::{agent}",
        "agent_path": f"/{agent}",
        "event_id": identifier,
        "turn_id": None,
        "at_ms": _ms(at),
        "text": text,
        "author_kind": author_kind,
        "ingress_kind": "codex",
        "prompt_ref": prompt_ref if prompt_ref is not None else reference
        if record_type in {"prompt", "inter_agent_prompt"}
        else None,
        "prompt_author_kind": prompt_author_kind,
        "prompt_at_ms": prompt_at_ms,
        "prompt_in_scope": prompt_in_scope,
        "content_fidelity": "verbatim",
    }


def _records() -> list[dict[str, JsonValue]]:
    """The corpus, written so that each trap the case study names is actually reachable."""

    records: list[dict[str, JsonValue]] = []
    for team in ("alpha-team", "beta-team"):
        records.append(
            _record(
                team,
                "prompt-day-one",
                record_type="prompt",
                role="user",
                at="2026-08-11T09:30:00+00:00",
                text="Where did we define backend maturity grade B3?",
                author_kind="owner_human",
                prompt_author_kind="owner_human",
                prompt_in_scope=True,
            )
        )
        # The pruning trap: the only occurrence of `strict-verify` is on day two, so a search for
        # it prunes day one's shard -- and the answer still owes the excerpt of the day-one prompt
        # this response is linked to.
        records.append(
            _record(
                team,
                "response-day-two",
                record_type="response",
                role="assistant",
                at="2026-08-12T02:15:00+00:00",
                agent="child",
                text=(
                    "B3 passes at least 50% of the ptrace strict-verify corpus; KVM sits at "
                    "134/180 cells and DBI is held at B2+."
                ),
                prompt_ref=f"message:{team}::prompt-day-one",
                prompt_author_kind="owner_human",
                prompt_at_ms=_ms("2026-08-11T09:30:00+00:00"),
                prompt_in_scope=True,
            )
        )
        # The receiver-side Codex return: recorded on the parent's rollout, attributed to the
        # child, and classified as an agent-authored response.
        records.append(
            _record(
                team,
                "child-return",
                record_type="inter_agent_response",
                role="agent",
                at="2026-08-12T03:00:00+00:00",
                agent="child",
                text="FINAL_ANSWER: LiteInst is B3 rather than B4 on virtual-time differences.",
                prompt_ref=f"message:{team}::instruction",
                prompt_author_kind="agent",
                prompt_at_ms=_ms("2026-08-11T20:00:00+00:00"),
                prompt_in_scope=True,
            )
        )
        records.append(
            _record(
                team,
                "instruction",
                record_type="inter_agent_prompt",
                role="agent",
                at="2026-08-11T20:00:00+00:00",
                text="Measure LiteInst against the canonical L2 corpus.",
                prompt_author_kind="agent",
                prompt_in_scope=True,
            )
        )
        # `prompt_in_scope` False: the export was sliced and this response's prompt is not in it.
        records.append(
            _record(
                team,
                "orphaned-response",
                record_type="response",
                role="assistant",
                at="2026-08-12T04:00:00+00:00",
                text="Continuing from the earlier ptrace run, DBI reached 130/152.",
                prompt_ref=f"message:{team}::before-the-window",
                prompt_author_kind="owner_human",
                prompt_at_ms=_START - 60_000,
                prompt_in_scope=False,
            )
        )
        # The Unicode trap. U+212A KELVIN SIGN case-folds to `k` under Python's Unicode rules and
        # does NOT under the corpus's portable ASCII-only rule, and `ſ` (U+017F) folds to `s` the
        # same way. A prefilter built on one rule and a matcher compiled on the other disagree.
        records.append(
            _record(
                team,
                "unicode-day-one",
                record_type="external",
                role="external",
                at="2026-08-11T11:00:00+00:00",
                text="Measured Kelvin drift and ſtrict rounding in the KELVIN harness.",
            )
        )
        records.append(
            _record(
                team,
                "tool-day-one",
                record_type="tool",
                role="tool",
                at="2026-08-11T12:00:00+00:00",
                text="3 tools used: 1 Bash, 2 Read",
                kind="tool",
            )
        )
    return records


def _build(root: Path) -> None:
    """Both corpora over one set of records, which is what makes the differential a differential.

    A build no longer writes the schema-2 half; this suite compares the two generations' answers,
    so it asks the retired writer for one on purpose.
    """

    timeline = _timeline()
    records = _records()
    with schema_2_writer_enabled():
        write_timeline_shards(root, dict(timeline), search_records=records)
    write_timeline_v3(root, timeline, search_records=records)


def _without_the_schema_three_corpus(source: Path, destination: Path) -> Path:
    """A copy of *source* as an archive built before schema 3 carried a search corpus.

    The catalogue sections and the files they name both go, because leaving either behind would
    change what is under test: an orphaned shard makes the reader decline the whole generation
    (completeness clause 5) and a catalogue entry with no file makes it decline on clause 3, and
    in both cases the comparison would be schema 2 against schema *1* instead.
    """

    shutil.copytree(source, destination)
    bootstrap = json.loads((destination / SCHEMA_3_BOOTSTRAP_PATH).read_text(encoding="utf-8"))
    for stream in SCHEMA_3_SEARCH_STREAMS:
        assert stream in bootstrap["streams"], stream
        bootstrap["streams"].pop(stream)
    for directory in (
        SCHEMA_3_SEARCH_ROOT,
        SCHEMA_3_SEARCH_BLOOM_ROOT,
        SCHEMA_3_SEARCH_LINKS_ROOT,
    ):
        shutil.rmtree(destination / directory)
    (destination / SCHEMA_3_BOOTSTRAP_PATH).write_text(
        json.dumps(bootstrap, indent=2, sort_keys=True), encoding="utf-8"
    )
    return destination


@pytest.fixture(name="archives")
def _archives(tmp_path: Path) -> tuple[Path, Path]:
    """``(schema 3 corpus, schema 2 corpus)`` over byte-identical inputs."""

    schema_3 = tmp_path / "with-v3-search"
    schema_3.mkdir()
    _build(schema_3)
    schema_2 = _without_the_schema_three_corpus(schema_3, tmp_path / "with-v2-search")
    return schema_3, schema_2


#: Every query the differential runs, as ``(label, kwargs for search_v2)``. Each one is here for a
#: reason the case study or a filter boundary supplies; the labels are what a failure prints.
_QUERIES: tuple[tuple[str, dict[str, object]], ...] = (
    ("acceptance query", {"needle": "backend maturity B3"}),
    ("two-byte term, no prefilter possible", {"needle": "B3"}),
    ("two-byte term, case sensitive", {"needle": "b3", "case_sensitive": True}),
    ("three-byte term the prefilter can prune on", {"needle": "KVM"}),
    ("term that appears on one day only", {"needle": "strict-verify"}),
    ("definite miss", {"needle": "quicksilver"}),
    ("kelvin sign against ascii folding", {"needle": "Kelvin"}),
    ("ascii kelvin against the sign", {"needle": "kelvin"}),
    ("kelvin sign finds only the kelvin sign", {"needle": "KELVIN"}),
    ("long s against ascii folding", {"needle": "ſtrict"}),
    ("ascii s against the long s", {"needle": "strict"}),
    ("phrase mode", {"needle": "ptrace strict-verify corpus", "match_mode": "phrase"}),
    ("literal mode", {"needle": "134/180", "match_mode": "literal"}),
    ("quoted term inside smart mode", {"needle": '"B2+" DBI'}),
    ("owner prompts only", {"needle": "B3", "corpus": "owner-prompts"}),
    ("agent responses only", {"needle": "B3", "corpus": "agent-responses"}),
    ("owner-authored prompts", {"needle": "B3", "prompt_author": "owner"}),
    ("agent-authored prompts", {"needle": "B3", "prompt_author": "agent"}),
    ("unclassified prompt author", {"needle": "tools", "prompt_author": "unclassified"}),
    ("linked only", {"needle": "B3", "linkage": "linked"}),
    ("unlinked only", {"needle": "tools", "linkage": "unlinked"}),
    ("assistant role only", {"needle": "B3", "roles": ("assistant",)}),
    ("tool role only", {"needle": "tools", "roles": ("tool",)}),
    ("newest first", {"needle": "B3", "sort": "newest"}),
    ("oldest first", {"needle": "B3", "sort": "oldest"}),
    ("second page", {"needle": "B3", "offset": 2, "limit": 2}),
    ("page past the end", {"needle": "B3", "offset": 99}),
)

#: Filters applied across the whole battery, so every query runs unscoped, team-scoped,
#: window-scoped and agent-scoped. The window deliberately cuts between the two UTC days, which is
#: the boundary the shard axis is built on and therefore the one a port is most likely to move.
_FILTERS: tuple[tuple[str, QueryFilters], ...] = (
    ("unfiltered", QueryFilters()),
    ("one team", QueryFilters(teams=("alpha-team",))),
    (
        "second day only",
        QueryFilters(
            window=_Window(_ms("2026-08-12T00:00:00+00:00"), _END),
        ),
    ),
    ("one agent", QueryFilters(agent_ref="agent:alpha-team::child")),
)


def _search(root: Path, filters: QueryFilters, **overrides: object) -> SearchResults:
    arguments: dict[str, object] = {
        "corpus": "all-transcript",
        "case_sensitive": False,
        "match_mode": "smart",
        "sort": "relevance",
        "prompt_author": "any",
        "linkage": "any",
        "roles": (),
        "offset": 0,
        "limit": 20,
    }
    arguments.update(overrides)
    needle = arguments.pop("needle")
    assert isinstance(needle, str)
    query = TimelineQuery(root)
    return query.search_v2(needle, filters=filters, **arguments)  # type: ignore[arg-type]


def _comparable(results: SearchResults) -> str:
    """Everything a caller can observe about one page, in a form a diff can show."""

    return json.dumps(
        {
            "total_matches": results.total_matches,
            "offset": results.offset,
            "limit": results.limit,
            "corpus": results.corpus,
            "match_mode": results.match_mode,
            "sort": results.sort,
            "results": list(results.items),
        },
        indent=2,
        sort_keys=True,
    )


def test_the_two_corpora_answer_every_query_identically(
    archives: tuple[Path, Path],
) -> None:
    """The differential itself: 104 query/filter pairs, full equality of the answer.

    Not a spot check and not a count comparison. The whole page is compared -- every excerpt,
    every match range, every ``prompt_excerpt``, ``prompt_in_scope``, ``linked_response_count``
    and ``phase_ref`` -- because those are the fields a port drops silently. A total that matches
    while an excerpt does not is exactly the bug this is built to catch.
    """

    schema_3, schema_2 = archives
    for label, overrides in _QUERIES:
        for scope, filters in _FILTERS:
            assert _comparable(_search(schema_3, filters, **overrides)) == _comparable(
                _search(schema_2, filters, **overrides)
            ), f"{label} / {scope}"


def test_the_battery_is_not_quietly_empty(archives: tuple[Path, Path]) -> None:
    """A differential over two empty answers proves nothing, so the corpus is asserted first.

    Equality is trivially satisfied by two paths that both return nothing, and a corpus that
    stopped being read at all would pass the test above without a single record. So every query
    that is *meant* to match is required to, and the ones meant to miss are required to miss.
    """

    schema_3, _schema_2 = archives
    matched = {
        label: _search(schema_3, QueryFilters(), **overrides).total_matches
        for label, overrides in _QUERIES
    }
    assert matched["page past the end"] == matched["two-byte term, no prefilter possible"]
    empty = sorted(label for label, total in matched.items() if total == 0)
    # The two that are *meant* to be empty: a term nothing contains, and a case-sensitive `b3`
    # against a corpus that only ever writes `B3`. Everything else in the battery has to bring
    # something back, or it is asserting equality about nothing.
    assert empty == ["definite miss", "two-byte term, case sensitive"], matched


def test_pruning_a_text_shard_does_not_hide_the_prompt_it_replies_to(
    archives: tuple[Path, Path],
) -> None:
    """The case study's third trap, asserted as a property rather than as a total.

    ``strict-verify`` appears only in the day-two response, so the day-one shard that holds its
    prompt is a definite miss and is never opened. The prompt excerpt still has to come back,
    which is what the relationship sidecar exists for -- and under schema 3 that sidecar is a line
    range in a per-team shard rather than a per-day content-addressed object, which is precisely
    the part of the port that could have lost it.
    """

    schema_3, schema_2 = archives
    for root in (schema_3, schema_2):
        results = _search(root, QueryFilters(teams=("alpha-team",)), needle="strict-verify")
        assert results.total_matches == 1
        item = as_object(results.items[0], "result")
        assert item["ref"] == "message:alpha-team::response-day-two"
        assert item["prompt_ref"] == "message:alpha-team::prompt-day-one"
        assert item["prompt_excerpt"] == "Where did we define backend maturity grade B3?"
        assert item["prompt_in_scope"] is True


def test_ascii_case_folding_keeps_the_kelvin_sign_and_the_letter_k_apart(
    archives: tuple[Path, Path],
) -> None:
    """The case study's first trap, and the one an ASCII-only reading of it would miss.

    The corpus line is ``Measured \N{KELVIN SIGN}elvin drift and \N{LATIN SMALL LETTER LONG S}trict
    rounding in the KELVIN harness.`` -- one occurrence spelled with U+212A and one spelled with an
    ASCII ``K``, in the same record. Under Python's *default* ``re.IGNORECASE`` those two are the
    same string and either query matches both; under the portable ASCII-only contract the corpus
    and the trigram filter are both defined against, they are different strings and each query
    matches exactly one.

    Asserted on the **match offsets** rather than on the totals, because the totals are equal
    either way -- both queries hit the same two records. The offset is the only observable that
    distinguishes "matched the sign" from "matched the letter", which is why a differential over
    totals would have let a Unicode-folding regression through.
    """

    schema_3, schema_2 = archives
    for root in (schema_3, schema_2):
        by_sign = _search(root, QueryFilters(teams=("alpha-team",)), needle="\N{KELVIN SIGN}elvin")
        by_letter = _search(root, QueryFilters(teams=("alpha-team",)), needle="Kelvin")
        assert by_sign.total_matches == by_letter.total_matches == 1
        sign_ranges = as_object(
            as_object(by_sign.items[0], "sign result")["excerpt_details"], "sign excerpt"
        )["match_ranges"]
        letter_ranges = as_object(
            as_object(by_letter.items[0], "letter result")["excerpt_details"], "letter excerpt"
        )["match_ranges"]
        assert sign_ranges == [[9, 15]]
        assert letter_ranges == [[49, 55]]

    # And the long s, which is the same trap in the other direction: an ASCII `strict` must reach
    # `strict-verify` in a different record and not the `\N{LATIN SMALL LETTER LONG S}trict` beside
    # the Kelvin sign.
    for root in (schema_3, schema_2):
        long_s = _search(
            root, QueryFilters(teams=("alpha-team",)), needle="\N{LATIN SMALL LETTER LONG S}trict"
        )
        plain = _search(root, QueryFilters(teams=("alpha-team",)), needle="strict")
        assert {str(as_object(item, "item")["ref"]) for item in long_s.items} == {
            "message:alpha-team::unicode-day-one"
        }
        assert {str(as_object(item, "item")["ref"]) for item in plain.items} == {
            "message:alpha-team::response-day-two"
        }


def test_a_sliced_export_keeps_saying_its_prompt_is_out_of_range(
    archives: tuple[Path, Path],
) -> None:
    """``prompt_in_scope`` survives the port, in both directions and in both generations.

    A response whose prompt fell outside the exported range keeps a ``prompt_ref`` that resolves
    to nothing, and the field is how a client tells that from corruption. Dropping it would make
    the two indistinguishable, so it is asserted as ``False`` -- not merely as present.
    """

    schema_3, schema_2 = archives
    for root in (schema_3, schema_2):
        results = _search(root, QueryFilters(teams=("alpha-team",)), needle="130/152")
        assert results.total_matches == 1
        item = as_object(results.items[0], "result")
        assert item["prompt_ref"] == "message:alpha-team::before-the-window"
        assert item["prompt_in_scope"] is False
        assert "prompt_excerpt" not in item


def test_a_child_return_recorded_on_the_parent_stays_attributed_to_the_child(
    archives: tuple[Path, Path],
) -> None:
    """Codex records a child's final answer on the receiving parent's rollout.

    The record's ``agent_ref`` is the child's, and it is the record that is copied, so this is a
    statement that the copy is verbatim -- including for the field an agent filter acts on, which
    is the one that would make the record unfindable if it moved.
    """

    schema_3, schema_2 = archives
    for root in (schema_3, schema_2):
        results = _search(
            root,
            QueryFilters(agent_ref="agent:alpha-team::child"),
            needle="FINAL_ANSWER",
        )
        assert results.total_matches == 1
        item = as_object(results.items[0], "result")
        assert item["agent_ref"] == "agent:alpha-team::child"
        assert item["record_type"] == "inter_agent_response"
        assert item["prompt_ref"] == "message:alpha-team::instruction"


def test_show_resolves_the_same_record_out_of_either_corpus(
    archives: tuple[Path, Path],
) -> None:
    """`timeline show` on a ``message:`` or ``tool:`` reference, for every record in the corpus.

    `show` is the other reader of the corpus and it returns the record whole, so it is the
    strictest available comparison: any field the port dropped, renamed or re-derived shows up
    here even if no search ranks on it.
    """

    schema_3, schema_2 = archives
    references = sorted(str(record["ref"]) for record in _records())
    assert len(references) == 14
    for reference in references:
        assert json.dumps(
            TimelineQuery(schema_3).show(reference), indent=2, sort_keys=True
        ) == json.dumps(TimelineQuery(schema_2).show(reference), indent=2, sort_keys=True)


def test_a_two_byte_query_reads_no_prefilter_and_a_longer_one_prunes(
    archives: tuple[Path, Path],
) -> None:
    """The cost claim behind moving the Bloom filters out of the bootstrap.

    Two facts, and they are only interesting together. A query with no term long enough to make a
    trigram opens no ``search-bloom`` shard at all -- schema 2 parsed all of its filters out of a
    bootstrap every command reads, and then skipped every one of them. A query with a rare term
    opens the filters and then opens *fewer* text shards than it otherwise would.

    Asserted on the set of shards opened rather than on a byte count or a clock, for the reason
    `test_query_read_paths.py` gives: a path set cannot be small for the wrong reason.
    """

    schema_3, _schema_2 = archives

    short = TimelineQuery(schema_3)
    short.search_v2(
        "B3",
        corpus="all-transcript",
        filters=QueryFilters(),
        case_sensitive=False,
        match_mode="smart",
        sort="relevance",
        prompt_author="any",
        linkage="any",
        roles=(),
        offset=0,
        limit=20,
    )
    opened = set(short.opened_shards)
    assert not any(path.startswith(SCHEMA_3_SEARCH_BLOOM_ROOT) for path in opened)
    assert sum(1 for path in opened if path.startswith(SCHEMA_3_SEARCH_ROOT + "/")) == 4

    rare = TimelineQuery(schema_3)
    rare.search_v2(
        "strict-verify",
        corpus="all-transcript",
        filters=QueryFilters(),
        case_sensitive=False,
        match_mode="smart",
        sort="relevance",
        prompt_author="any",
        linkage="any",
        roles=(),
        offset=0,
        limit=20,
    )
    pruned = set(rare.opened_shards)
    assert any(path.startswith(SCHEMA_3_SEARCH_BLOOM_ROOT) for path in pruned)
    text_shards = {path for path in pruned if path.startswith(SCHEMA_3_SEARCH_ROOT + "/")}
    assert text_shards == {
        f"{SCHEMA_3_SEARCH_ROOT}/alpha-team/{_DAY_TWO}.jsonl.gz",
        f"{SCHEMA_3_SEARCH_ROOT}/beta-team/{_DAY_TWO}.jsonl.gz",
    }
    assert all(_DAY_ONE not in path for path in text_shards)


def test_a_search_never_opens_the_schema_two_bootstrap_when_schema_three_has_a_corpus(
    archives: tuple[Path, Path],
) -> None:
    """The point of the whole exercise, asserted where it cannot be argued away.

    ``data/timeline-v2.json`` is 5,702,530 bytes on the measured archive and a search used to
    parse it before answering anything. The claim is not that it is cheaper -- it is that the file
    is not read, which is what lets `gc` offer it. Proved by making the file unreadable rather
    than by counting bytes: if the corpus still reached for it the search would raise.
    """

    schema_3, schema_2 = archives
    (schema_3 / SCHEMA_2_BOOTSTRAP_PATH).write_text("not json at all", encoding="utf-8")
    results = _search(schema_3, QueryFilters(), needle="backend maturity B3")
    assert results.total_matches == 2

    # And the converse, which is what makes the whole differential mean anything: break the same
    # file in the other archive and its search stops working. Without this, "the two agree" would
    # be satisfiable by both of them quietly reading the same corpus.
    (schema_2 / SCHEMA_2_BOOTSTRAP_PATH).write_text("not json at all", encoding="utf-8")
    with pytest.raises(ValueError):
        _search(schema_2, QueryFilters(), needle="backend maturity B3")


def test_a_partly_published_search_corpus_is_declined_rather_than_half_read(
    tmp_path: Path,
) -> None:
    """All three streams or none, and the reader says which is missing.

    A corpus without its relationship sidecar answers `--linkage linked` with an empty result,
    which is a perfectly ordinary-looking answer and a wrong one. So the generation is refused and
    the reader falls back to schema 2, which is slow and correct. The refusal is asserted on the
    schema-3 *reader* rather than on a search, because falling back means the search still works
    and only ``schema_3_declined`` records that it did.
    """

    root = tmp_path / "archive"
    root.mkdir()
    _build(root)
    bootstrap = json.loads((root / SCHEMA_3_BOOTSTRAP_PATH).read_text(encoding="utf-8"))
    for shard in bootstrap["streams"].pop("search_links")["shards"]:
        (root / shard["path"]).unlink()
        (root / shard["index_path"]).unlink()
    shutil.rmtree(root / SCHEMA_3_SEARCH_LINKS_ROOT)
    (root / SCHEMA_3_BOOTSTRAP_PATH).write_text(
        json.dumps(bootstrap, indent=2, sort_keys=True), encoding="utf-8"
    )

    query = TimelineQuery(root)
    assert "transcript search corpus" in query.schema_3_declined
    assert _search(root, QueryFilters(), needle="backend maturity B3").total_matches == 2


def _rewrite_shard(
    root: Path, entry: dict[str, JsonValue], records: list[dict[str, JsonValue]]
) -> None:
    """Replace one catalogued shard's records, and make the catalogue describe what is now there.

    A corruption test has to leave the archive *acceptable* -- every completeness rule satisfied,
    every declared length matching -- or it proves only that the completeness rules work. So the
    shard is rewritten with the same writer the archive uses and its catalogue entry is refreshed
    from the sidecar the writer just produced. What differs from a real archive is one field
    inside one record, which is the whole point: `_check_completeness` deliberately does not
    verify ``c_sha256``/``u_sha256``, so a length-preserving substitution is exactly the class of
    damage a reader has to catch for itself.
    """

    path = root / str(entry["path"])
    report = write_seekable_jsonl(path, records, timestamp_key="at_ms")
    index = report.index
    entry["records"] = index.record_count
    entry["members"] = len(index.members)
    entry["c_bytes"] = index.c_size
    entry["u_bytes"] = index.u_size
    entry["c_sha256"] = index.c_sha256
    entry["u_sha256"] = index.u_sha256


def test_a_prefilter_that_names_another_teams_shard_is_refused(tmp_path: Path) -> None:
    """The one way the prefilter stream can be worse than the fields it replaced.

    Schema 2 inlines each Bloom filter on the catalogue entry it belongs to, so a filter cannot be
    addressed anywhere but its own shard. Schema 3's prefilter is a stream keyed by the *path* a
    record names, which makes another team's shard reachable from this team's file -- and a Bloom
    filter's only wrong answer is a **false miss**, so a filter installed under the wrong path
    makes the reader report a definite miss for a day it never opened. Every record in that day is
    then silently absent, with no error and no diagnostic.

    Nothing produces this today: the writer derives the path from the bucket it is filtering. It
    is refused anyway, because the check `_check_completeness` skips is the digest -- see
    `_SchemaThreeArchive`'s docstring -- and a length-preserving corruption is precisely what that
    leaves uncaught. `static/app.js`'s `ensureSearchBlooms` refuses the same record.
    """

    root = tmp_path / "archive"
    root.mkdir()
    _build(root)
    bootstrap = json.loads((root / SCHEMA_3_BOOTSTRAP_PATH).read_text(encoding="utf-8"))
    blooms = bootstrap["streams"]["search_bloom"]["shards"]
    alpha = next(entry for entry in blooms if entry["team"] == "alpha-team")
    victim = f"{SCHEMA_3_SEARCH_ROOT}/beta-team/{_DAY_TWO}.jsonl.gz"

    with gzip.open(root / str(alpha["path"]), "rt", encoding="utf-8") as handle:
        records = [json.loads(line) for line in handle]
    assert records and all(record["team"] == "alpha-team" for record in records)
    # Alpha's own team slug, alpha's own shard file -- and beta's path in the one field the reader
    # keys by. `_unwrap`'s team check passes; only the ownership check can see this.
    records[0]["shard"] = victim
    _rewrite_shard(root, alpha, records)
    (root / SCHEMA_3_BOOTSTRAP_PATH).write_text(
        json.dumps(bootstrap, indent=2, sort_keys=True), encoding="utf-8"
    )

    # The generation is still complete: the damage is inside a shard, not in the catalogue.
    assert TimelineQuery(root).schema_3_declined == ""
    with pytest.raises(ArchiveReadError, match="not a search shard of team 'alpha-team'"):
        _search(root, QueryFilters(), needle="strict-verify")


def test_rebuilding_the_corpus_changes_nothing(tmp_path: Path) -> None:
    """Two builds over identical input produce identical bytes, search streams included.

    The whole archive rests on this: `_replace_if_changed` only writes a shard whose bytes differ,
    so an unchanged rebuild must churn nothing -- and a corpus that churned would republish 67 MiB
    on every build and make every rebuild look like a change to whoever is watching the archive.
    The two places it could have been lost are the Bloom filter, which would differ if trigram
    collection depended on set iteration order, and the linkage sort, which would differ if it
    tiebroke on anything but the reference.
    """

    root = tmp_path / "archive"
    root.mkdir()
    _build(root)
    timeline = _timeline()
    records = _records()
    with schema_2_writer_enabled():
        write_timeline_shards(root, dict(timeline), search_records=records)
    again = write_timeline_v3(root, timeline, search_records=records)
    assert again.files_changed == 0
    assert again.removed_files == ()
    assert again.search_shards == 4
    assert again.search_records == len(records)


def test_the_writer_refuses_a_corpus_it_could_not_publish_faithfully(tmp_path: Path) -> None:
    """Two refusals, both raised before the first byte, both about a silent wrong answer.

    A duplicate ``ref`` would let one record shadow another under a reference the archive promises
    is unique -- `show` would return whichever the reader met last. A record naming a team the
    timeline does not publish would be written to a shard the catalogue cannot describe, and the
    reader's own scope check would then reject the whole shard at query time, which is a build
    failure discovered by a user.
    """

    root = tmp_path / "archive"
    root.mkdir()
    timeline = _timeline()
    duplicated = _records()
    duplicated.append(dict(duplicated[0]))
    with pytest.raises(ValueError, match="duplicate search record reference"):
        write_timeline_v3(root, timeline, search_records=duplicated)

    stranger = _records()
    stranger[0] = {**stranger[0], "team": "gamma-team"}
    with pytest.raises(ValueError, match="absent from teams"):
        write_timeline_v3(root, timeline, search_records=stranger)
    assert not (root / SCHEMA_3_SEARCH_ROOT).exists()


def test_the_corpus_is_a_verbatim_copy_of_the_records_the_builder_produced(
    tmp_path: Path,
) -> None:
    """One envelope key added, nothing else touched.

    The differential above proves the two generations *answer* the same; this proves the stronger
    thing the envelope contract claims, which is that a schema-3 search line is the schema-2
    record plus ``record_kind`` and nothing more. Without it, two compensating changes -- a field
    dropped by the writer and re-derived by the reader -- would pass the differential and leave
    the format lying about what it stores.
    """

    root = tmp_path / "archive"
    root.mkdir()
    _build(root)
    bootstrap = as_object(read_json(root / SCHEMA_3_BOOTSTRAP_PATH), "bootstrap")
    streams = as_object(bootstrap["streams"], "streams")
    section = as_object(streams["search"], "search stream")
    stored: dict[str, dict[str, JsonValue]] = {}
    for raw in section["shards"]:  # type: ignore[union-attr]
        entry = as_object(raw, "shard")
        with gzip.open(root / str(entry["path"]), "rb") as handle:
            for line in handle:
                if not line.strip():
                    continue
                record = as_object(json.loads(line.decode("utf-8")), "record")
                assert record.pop("record_kind") == "search_record"
                stored[str(record["ref"])] = record
    assert stored == {str(record["ref"]): record for record in _records()}
