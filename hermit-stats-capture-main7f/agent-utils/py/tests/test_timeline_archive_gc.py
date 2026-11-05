"""Contract tests for retiring a format generation and for reclaiming what it leaves behind.

Two changes are asserted here and they are deliberately in one file, because each is only safe
because of the other. A published build stopped writing ``data/timeline.json``; the reason that
is not a data-loss bug is that nothing deletes the copy an older build already wrote, and the
reason *that* is not a hoarding bug is that `gc` will delete it when an operator says so.

The assertions are organised around the three ways this could go wrong, worst first.

*Something is deleted that a reader still needs.* Every reclaim is gated on a named superseding
artefact being present and complete, and there is one test per gate that damages exactly that
gate and requires `gc` to hold. The strongest of them is the incomplete-generation case: a
rebuild interrupted halfway leaves a bootstrap describing fewer shards than exist, and a `gc`
that trusted it would delete the better half of the rebuild.

*Something is deleted irreversibly by accident.* The first pass is a rename into a trash tree
that mirrors the archive, so the test for it is a *round trip*: sweep, copy the payload back,
and require the archive to read the same as before.

*The accounting silently stops adding up.* The schema-2 manifest's current/retained split is
exact today, and the orphan set is derived as ``on disk - current - retained`` rather than
counted separately, so a test that plants one unreferenced object and one retained object has
to see exactly one of them reclaimed. The schema-2 categories now partition the generation
rather than describing overlapping parts of it, so that is asserted directly: no file in two
categories, and the three of them together equal what is on disk.

*A whole generation is released while something still writes it.* ``superseded-schema-2`` is
gated on a second precondition the monolith never needed -- a writer that has been *retired in
code*, `timeline_shards.SCHEMA_2_IS_PUBLISHED`, rather than a writer that merely did not run
recently. The tests hold that constant and the writer together from both sides: with it true the
1.4 GB is held and the reason says which constant holds it, with it false the writer refuses to
run at all, and either way the bundled `static/app.js` has to agree with it -- a browser fetching
a format nobody publishes is how a flip would retire the graphical surface by accident.

Named `test_timeline_*` because the packaged workflow runs `pytest tests/test_timeline_*.py`;
a file outside that glob would run only under `make validate`.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import time
from importlib.resources import files
from pathlib import Path

import pytest

from wrkviz.build_store import team_build_root
from wrkviz.archive import ARCHIVE_MARKER_FILE
from wrkviz.archive_gc import (
    ArchiveGcError,
    TRASH_ROOT,
    collect,
    format_gc_report,
    plan_collection,
)
from wrkviz.cli import _inspect_counts
from wrkviz.cli import main as timeline_main
from wrkviz.multi_team import build_combined_archive
from wrkviz.pipeline import (
    _ensure_bulk_content_ignored,
    build_archive,
    summarize_archive,
)
from wrkviz.query import (
    QueryFilters,
    SearchResults,
    TimelineQuery,
    schema_3_completeness,
)
from wrkviz.render import SCHEMA_1_TIMELINE_PATH
from wrkviz.timeline_shards import (
    SCHEMA_2_BOOTSTRAP_PATH,
    SCHEMA_2_IS_PUBLISHED,
    SCHEMA_2_ROOT,
    schema_2_is_published,
    write_timeline_shards,
)
from wrkviz.timeline_v3 import SCHEMA_3_BOOTSTRAP_PATH, SCHEMA_3_ROOT
from tests.test_timeline_pipeline import _team, _write_team
from tests.timeline_projection import schema_1_timeline_text
from tests.timeline_legacy_generations import write_legacy_schema_2


_OBJECT_ROOT = "data/timeline-v2/objects"
_V2_MANIFEST = "data/timeline-v2/manifest.json"
_EXPORT_MANIFEST = "data/export.json"


def _publishing_schema_2(monkeypatch: pytest.MonkeyPatch) -> None:
    """Put the process back in the world where the tool still published schema 2.

    The inverse of what this helper used to be, and the inversion is the whole news: the release
    has flipped, so the *default* is now the retired world and the one that has to be simulated is
    the old one. It is still worth simulating for one test -- the reason string a collector prints
    while a writer is still emitting the generation it is looking at -- because that reason is the
    guard's other half and an operator would read it before typing `--delete`.

    Patched on the module that owns the constant rather than on `archive_gc`, because that is the
    coupling under test: `timeline_shards.schema_2_is_published` reads the global when it is
    called, so one patch moves the collector's precondition *and* the writer's refusal together.
    A test that could move one without the other would be able to assert a state the release can
    never be in.
    """

    monkeypatch.setattr(
        "wrkviz.timeline_shards.SCHEMA_2_IS_PUBLISHED", True
    )


def _built(archive: Path) -> str:
    """One real single-team archive, built the way the pipeline builds one.

    Schema 3 and nothing else, because that is now what a build writes. A test that needs the
    schema-2 generation -- which is most of the ones about `gc`, whose subject is what older
    builds left behind -- uses :func:`_built_before_the_flip`.
    """

    team = _team()
    _write_team(archive, team)
    summarize_archive(archive, team.team_slug, "heuristic", "test-model")
    build_archive(archive, team.team_slug)
    return team.team_slug


def _built_before_the_flip(archive: Path) -> str:
    """One archive as a build that still wrote schema 2 would have left it.

    The current build plus the schema-2 generation the previous one produced, over the same
    records; see `tests/timeline_legacy_generations.py` for why the older generation is written by
    the real writer rather than imitated.
    """

    slug = _built(archive)
    write_legacy_schema_2(archive)
    return slug


def _a_bundle_that_predates_schema_three(archive: Path) -> None:
    """Replace this archive's ``app.js`` with the one an older build would have copied in.

    The state cannot be reached by monkeypatching, and that asymmetry is the point of
    `archive_gc._website_refusal`: the bundle is *copied into* the archive by the build that made
    it, so "this website has no schema-3 mode" is a fact about this directory that no tool release
    can change. Every archive built before the flip is in it and stays in it until a rebuild.

    Written as a stub rather than by editing the real bundle, because the refusal tests the
    presence of a string that now appears in `static/app.js` some thirty times, and a test that
    tried to strip them all would be reimplementing the reader in a regular expression.
    """

    (archive / "app.js").write_text(
        "(function () {\n"
        '  var DATA_URL = "data/timeline.json";\n'
        f'  var SCHEMA_2_URL = "{SCHEMA_2_BOOTSTRAP_PATH}";\n'
        "}());\n",
        encoding="utf-8",
    )


def _categories(archive: Path) -> dict[str, tuple[bool, int]]:
    report = plan_collection(archive)
    return {item.name: (item.reclaimable, item.count) for item in report.categories}


def _answers(archive: Path) -> tuple[object, object, object]:
    """Everything a sweep must leave answerable, which is not only what schema 3 answers.

    The first two questions come out of the schema-3 spine, and an oracle made of those alone is
    structurally unable to notice that a pass destroyed something schema 3 does not implement --
    it would report "the archive still reads" about an archive that had just lost a whole
    command. A transcript search is the third question for exactly that reason: it is the one
    operation in `query.TimelineQuery` that reaches into schema 2 from under schema 3, so it is
    the only one whose survival says anything about the schema-2 tree.
    """

    query = TimelineQuery(archive)
    return (
        query.list_records("agents", QueryFilters()),
        query.list_records("phases", QueryFilters()),
        _search(archive).total_matches,
    )


def _search(archive: Path) -> SearchResults:
    """One transcript search, with every knob at the value the CLI defaults to."""

    return TimelineQuery(archive).search_v2(
        "the",
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


def _unpublish_the_schema_three_search_corpus(archive: Path) -> None:
    """Turn this archive back into one built before schema 3 carried a transcript search corpus.

    Not a monkeypatch, for the same reason :func:`_a_bundle_that_predates_schema_three` is not
    one: the
    state under test is a fact about *this directory* rather than about the process. Every archive
    on disk today is in it, and stays in it until somebody rebuilds -- which is exactly why `gc`
    asks the archive rather than the writer whether a successor exists.

    Both halves have to go. Dropping the catalogue sections alone would leave shard-shaped files
    under ``data/timeline-v3/`` that no entry names, and clause 5 of the reader's completeness
    rule declines the whole generation for that -- which would make this a test about an
    interrupted build instead of about an older one.
    """

    path = archive / SCHEMA_3_BOOTSTRAP_PATH
    bootstrap = json.loads(path.read_text(encoding="utf-8"))
    for stream in ("search", "search_bloom", "search_links"):
        for shard in bootstrap["streams"].pop(stream, {}).get("shards", []):
            (archive / shard["path"]).unlink()
            (archive / shard["index_path"]).unlink()
    for directory in ("search", "search-bloom", "search-links"):
        shutil.rmtree(archive / "data" / "timeline-v3" / directory, ignore_errors=True)
    path.write_text(json.dumps(bootstrap, indent=2, sort_keys=True), encoding="utf-8")


def _assert_the_website_reads_schema_three(archive: Path) -> None:
    """Every build now copies in a bundle that names the schema-3 bootstrap.

    This used to be a *mutation* -- the suite had to add the marker, because no build produced
    one -- and it is now an assertion, which is the same fact stated from the other side. Keeping
    it as a named check rather than deleting it is deliberate: the whole schema-2 retirement rests
    on a build putting that string in the archive, and a silent regression there would show up as
    a `gc` that reclaims nothing, with a reason nobody would think to doubt.
    """

    assert SCHEMA_3_BOOTSTRAP_PATH in (archive / "app.js").read_text(encoding="utf-8")


# -- what a published build no longer writes -------------------------------------------------


def test_a_published_build_writes_no_schema_one_monolith(tmp_path: Path) -> None:
    """The retirement itself, and the two generations that make it safe.

    Both readers reach schema 1 only as a fallback, and both of the generations in front of it
    are written here, so an archive this tool produces never needs the monolith. The manifest is
    checked as well as the filesystem: a path that vanished from disk but stayed in
    ``generated_files`` would be reported as missing by the next build's stale-file pass.
    """

    _built(tmp_path)

    assert not (tmp_path / SCHEMA_1_TIMELINE_PATH).exists()
    assert not (tmp_path / (SCHEMA_1_TIMELINE_PATH + ".gz")).exists()
    # And now the schema-2 presentation generation is gone from a build too, which is the second
    # retirement this file is about. One generation is written and two are readable.
    assert not (tmp_path / SCHEMA_2_BOOTSTRAP_PATH).exists()
    assert not (tmp_path / SCHEMA_2_ROOT).exists()
    assert (tmp_path / SCHEMA_3_BOOTSTRAP_PATH).is_file()
    _assert_the_website_reads_schema_three(tmp_path)

    manifest = json.loads((tmp_path / _EXPORT_MANIFEST).read_text(encoding="utf-8"))
    assert SCHEMA_1_TIMELINE_PATH not in manifest["generated_files"]
    assert manifest["retired_files"] == []
    assert TimelineQuery(tmp_path).schema_3_declined == ""


def test_a_combined_export_writes_no_monolith_and_is_still_readable(tmp_path: Path) -> None:
    """The combined export retires it too, though its per-team intermediate still uses it.

    `multi_team` renders each team into a temporary directory and reads ``data/timeline.json``
    back out of it, so schema 1 is still that render's *output format*. What must not survive is
    a merged monolith in the export an operator keeps.
    """

    archive = tmp_path / "archive"
    output = tmp_path / "export"
    first = _team()
    _write_team(archive, first)
    summarize_archive(archive, first.team_slug, "heuristic", "test-model")
    second = _team("A second team so the combined path is exercised.")
    second = second.__class__(**{**second.__dict__, "team_slug": "second-team"})
    _write_team(archive, second)
    summarize_archive(archive, second.team_slug, "heuristic", "test-model")

    build_combined_archive(
        archive,
        (first.team_slug, second.team_slug),
        output=output,
        display_timezone="UTC",
    )

    assert not (output / SCHEMA_1_TIMELINE_PATH).exists()
    manifest = json.loads((output / _EXPORT_MANIFEST).read_text(encoding="utf-8"))
    assert SCHEMA_1_TIMELINE_PATH not in manifest["generated_files"]
    query = TimelineQuery(output)
    assert query.schema_3_declined == ""
    assert len(query.teams) == 2


def test_a_rebuild_retires_the_older_monolith_instead_of_deleting_it(tmp_path: Path) -> None:
    """The ordering rule: not writing a file and deleting it are separate decisions.

    Left to itself, `_remove_stale_presentation_files` reaps ``previous - current``, and after
    this change the monolith is in every older manifest's ``previous`` and in no new build's
    ``current`` -- so the default behaviour would be a quarter-gigabyte irreversible unlink
    triggered by a build the operator asked for a different reason. The file has to survive the
    rebuild, and it has to be *recorded* as surviving, because a file nobody names is a file
    `gc` cannot classify.
    """

    slug = _built(tmp_path)
    # Stand in for an archive an older tool wrote: the monolith on disk and named by the
    # manifest the next build will read as `previous`.
    monolith = tmp_path / SCHEMA_1_TIMELINE_PATH
    monolith.write_text('{"schema_version": 1}', encoding="utf-8")
    (tmp_path / (SCHEMA_1_TIMELINE_PATH + ".gz")).write_bytes(b"\x1f\x8b old")
    manifest_path = tmp_path / _EXPORT_MANIFEST
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    manifest["generated_files"] = sorted(
        {*manifest["generated_files"], SCHEMA_1_TIMELINE_PATH,
         SCHEMA_1_TIMELINE_PATH + ".gz"}
    )
    manifest_path.write_text(json.dumps(manifest), encoding="utf-8")

    build_archive(tmp_path, slug)

    assert monolith.is_file()
    assert monolith.read_text(encoding="utf-8") == '{"schema_version": 1}'
    rebuilt = json.loads(manifest_path.read_text(encoding="utf-8"))
    assert SCHEMA_1_TIMELINE_PATH not in rebuilt["generated_files"]
    assert rebuilt["retired_files"] == [
        SCHEMA_1_TIMELINE_PATH,
        SCHEMA_1_TIMELINE_PATH + ".gz",
    ]


def test_inspect_counts_the_same_records_without_the_monolith(tmp_path: Path) -> None:
    """`inspect` printed six integers by parsing the whole monolith; it now reads the catalogue.

    246,973,399 bytes at 1.44 GiB resident, on the measured archive, to print six integers that
    the 168,703-byte bootstrap already publishes. The differential is the point: the two branches
    have to agree, or the command has quietly started reporting something else, and the natural
    place to get an honest schema-1 input is the combiner's unpublished render -- which is the
    one mode that still writes the monolith and nothing else.
    """

    archive = tmp_path / "archive"
    intermediate = tmp_path / "intermediate"
    slug = _built(archive)
    from_schema_3 = _inspect_counts(archive)

    query = TimelineQuery(archive)
    assert from_schema_3["agents"] == len(query.list_records("agents", QueryFilters()))
    assert from_schema_3["phases"] == len(query.list_records("phases", QueryFilters()))
    assert from_schema_3["rollups"] == len(query.list_records("rollups", QueryFilters()))
    assert from_schema_3["events"] > 0

    build_archive(archive, slug, output=intermediate, _published=False)
    assert (intermediate / SCHEMA_1_TIMELINE_PATH).is_file()
    assert not (intermediate / SCHEMA_3_BOOTSTRAP_PATH).exists()
    assert not (intermediate / SCHEMA_2_BOOTSTRAP_PATH).exists()
    assert _inspect_counts(intermediate) == from_schema_3


def test_the_reconstruction_the_suite_leans_on_is_the_real_monolith(tmp_path: Path) -> None:
    """`tests/timeline_projection.py` now feeds a hundred assertions; here is what backs it.

    Most of this suite still asserts against the schema-1 object, reconstructed from schema 3
    because the file is gone. That reconstruction is only load-bearing if it is *exact*, so it is
    compared here against the genuine article -- the monolith the combiner's unpublished render
    still writes -- collection by collection, as record sets.

    Sets and not sequences: schema 3 sorts a timeline shard by instant and a spine group by the
    kind's identifier, which is not the renderer's insertion order, and the helper says so. What
    must hold is that every record survives the round trip unchanged and none is invented or
    duplicated -- which is also `timeline_v3`'s own losslessness claim, executed.
    """

    archive = tmp_path / "archive"
    intermediate = tmp_path / "intermediate"
    slug = _built(archive)
    build_archive(archive, slug, output=intermediate, _published=False)

    real = json.loads((intermediate / SCHEMA_1_TIMELINE_PATH).read_text(encoding="utf-8"))
    rebuilt = json.loads(schema_1_timeline_text(archive))

    assert sorted(real) == sorted(rebuilt)
    for field, value in sorted(real.items()):
        if isinstance(value, list):
            assert sorted(json.dumps(item, sort_keys=True) for item in value) == sorted(
                json.dumps(item, sort_keys=True) for item in rebuilt[field]
            ), field
        else:
            assert value == rebuilt[field], field


# -- what gc reclaims, and what it refuses to ------------------------------------------------


def test_a_dry_run_touches_nothing_and_explains_every_category(tmp_path: Path) -> None:
    _built(tmp_path)
    before = sorted(path.relative_to(tmp_path).as_posix() for path in tmp_path.rglob("*"))

    report = plan_collection(tmp_path)

    assert report.action == "dry-run"
    assert report.moved == ()
    assert sorted(
        path.relative_to(tmp_path).as_posix() for path in tmp_path.rglob("*")
    ) == before
    assert {item.name for item in report.categories} == {
        "schema-1-monolith",
        "superseded-schema-2",
        "schema-2-search-corpus",
        "schema-2-orphan-objects",
        "schema-2-retained-objects",
        "schema-3-orphan-shards",
        "ingest-source-snapshots",
        "trash",
    }
    # Every category carries its reason even when it is empty, so a zero is never unexplained.
    assert all(item.reason for item in report.categories)
    text = format_gc_report(report, "text")
    assert "this was a dry run" in text
    assert TRASH_ROOT in text


def test_the_cli_defaults_to_a_dry_run(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    _built(tmp_path)
    (tmp_path / SCHEMA_1_TIMELINE_PATH).write_text("{}", encoding="utf-8")

    assert timeline_main(["gc", "--output", str(tmp_path)]) == 0

    assert (tmp_path / SCHEMA_1_TIMELINE_PATH).is_file()
    assert "dry run" in capsys.readouterr().out


def test_the_monolith_is_held_without_a_complete_schema_three(tmp_path: Path) -> None:
    """Clause one of the gate: the superseding generation must be readable as a whole.

    This is the state the measured archive is in before its migration -- schema 1 and schema 2
    and no schema 3 -- and reclaiming there would delete the only complete presentation
    generation the command-line reader can use.
    """

    _built(tmp_path)
    (tmp_path / SCHEMA_1_TIMELINE_PATH).write_text("{}", encoding="utf-8")
    (tmp_path / SCHEMA_3_BOOTSTRAP_PATH).unlink()

    report = plan_collection(tmp_path)
    monolith = report.category("schema-1-monolith")
    assert monolith.count == 1
    assert not monolith.reclaimable
    assert "no schema-3 bootstrap" in monolith.reason

    collect(tmp_path, delete=True)
    assert (tmp_path / SCHEMA_1_TIMELINE_PATH).is_file()


def test_the_monolith_is_held_when_the_website_has_no_fallback(tmp_path: Path) -> None:
    """Clause two: the *browser in this archive* may have retired schema 1 later than the CLI did.

    A bundle written before schema 3 existed loads schema 2 and falls back to the monolith when
    that load throws. An archive carrying one of those, with schema 3 but no schema 2, is one
    where reclaiming the monolith takes away the thing the fallback falls back to -- and no tool
    release can fix it, because the bundle is copied in and stays.

    The question is asked of the archive and not of the release, which is the correction this
    condition needed once the release flipped: a constant that says "the tool no longer writes
    schema 2" says nothing at all about what is sitting in somebody's directory.
    """

    _built(tmp_path)
    _a_bundle_that_predates_schema_three(tmp_path)
    (tmp_path / SCHEMA_1_TIMELINE_PATH).write_text("{}", encoding="utf-8")
    assert not (tmp_path / SCHEMA_2_BOOTSTRAP_PATH).exists()

    monolith = plan_collection(tmp_path).category("schema-1-monolith")
    assert not monolith.reclaimable
    assert "has no schema-3 mode" in monolith.reason

    # The current bundle reads schema 3, so the same archive with a rebuilt website releases it.
    build_archive(tmp_path, _team().team_slug)
    _assert_the_website_reads_schema_three(tmp_path)
    assert plan_collection(tmp_path).category("schema-1-monolith").reclaimable


def test_an_interrupted_rebuild_holds_its_own_shards(tmp_path: Path) -> None:
    """One direction of a half-published generation: a shard shorter than the catalogue says.

    This is the damage the bootstrap's own numbers can see. The direction it *cannot* see is the
    next test, which is the dangerous one.
    """

    _built(tmp_path)
    shard = next((tmp_path / SCHEMA_3_ROOT).rglob("*.jsonl.gz"))
    shard.write_bytes(shard.read_bytes()[:-8])

    shards = plan_collection(tmp_path).category("schema-3-orphan-shards")
    assert not shards.reclaimable
    assert "partly published" in shards.reason
    monolith = plan_collection(tmp_path).category("schema-1-monolith")
    assert not monolith.reclaimable


def test_a_build_that_died_before_its_bootstrap_loses_no_team_and_no_fallback(
    tmp_path: Path,
) -> None:
    """The failure the orphan rule used to cause, reproduced the way it actually happens.

    No hand-edited archive: build a two-team export, keep its bootstrap and its bins shard,
    build a three-team export, and put those two files back. That is exactly the state
    ``write_timeline_v3`` leaves when it raises anywhere between the last spine shard and the
    bootstrap -- ENOSPC, a SIGKILL, a validation error in a later phase of the build -- because
    publication is shards, then the bootstrap, with no atomicity across the set.

    Everything the surviving bootstrap names is present at its declared length, so a rule that
    only checked the catalogue against the disk in one direction called this complete. Two
    consequences followed, and both are asserted against here: the reader answered from the
    older catalogue and silently dropped a team the archive plainly has, and `gc` classified the
    third team's live shards as unreachable *and* declared the schema-1 monolith superseded, so
    one `--delete` plus one `--empty-trash` removed a team from schema 3 and the fallback that
    would have had it.
    """

    archive = tmp_path / "archive"
    output = tmp_path / "export"
    slugs: list[str] = []
    for index, slug in enumerate(("team-one", "team-two", "team-three")):
        team = _team(f"Team {index} narrative.")
        team = team.__class__(**{**team.__dict__, "team_slug": slug})
        _write_team(archive, team)
        summarize_archive(archive, slug, "heuristic", "test-model")
        slugs.append(slug)

    build_combined_archive(archive, tuple(slugs[:2]), output=output, display_timezone="UTC")
    surviving_bootstrap = (output / SCHEMA_3_BOOTSTRAP_PATH).read_bytes()
    bins = output / SCHEMA_3_ROOT / "bins.jsonl.gz"
    surviving_bins = bins.read_bytes()
    surviving_bins_index = bins.with_name(bins.name + ".index.jsonl").read_bytes()
    # An operator's older monolith, so the fallback has something to lose.
    (output / SCHEMA_1_TIMELINE_PATH).write_text(
        json.dumps({"schema_version": 1, "marker": "the fallback"}), encoding="utf-8"
    )

    build_combined_archive(archive, tuple(slugs), output=output, display_timezone="UTC")
    # The schema-2 generation the interrupted build's predecessor would have left, written from
    # the *complete* three-team schema 3 and therefore naming all three -- which is the point: the
    # reader falls back to it while schema 3 is refused, and it is what `gc` must not reclaim.
    write_legacy_schema_2(output)
    (output / SCHEMA_3_BOOTSTRAP_PATH).write_bytes(surviving_bootstrap)
    bins.write_bytes(surviving_bins)
    bins.with_name(bins.name + ".index.jsonl").write_bytes(surviving_bins_index)

    third_spine = output / SCHEMA_3_ROOT / "spine" / "team-three.jsonl.gz"
    assert third_spine.is_file()

    # The reader refuses the generation rather than answering with two teams out of three.
    query = TimelineQuery(output)
    assert "named by no entry" in query.schema_3_declined
    assert len(query.teams) == 3

    report = plan_collection(output)
    shards = report.category("schema-3-orphan-shards")
    assert not shards.reclaimable
    assert third_spine.relative_to(output).as_posix() in {
        item.relative_path for item in shards.files
    }
    assert not report.category("schema-1-monolith").reclaimable

    collect(output, delete=True)
    assert third_spine.is_file()
    assert (output / SCHEMA_1_TIMELINE_PATH).is_file()

    # And the remedy is the build itself: re-running it clears the residue and the archive is
    # whole again, with no operator decision and nothing in the trash.
    build_combined_archive(archive, tuple(slugs), output=output, display_timezone="UTC")
    assert TimelineQuery(output).schema_3_declined == ""
    assert plan_collection(output).category("schema-3-orphan-shards").count == 0


def test_a_shard_no_bootstrap_names_is_held_and_the_next_build_removes_it(
    tmp_path: Path,
) -> None:
    """Absence from the catalogue is reported, never swept, and cleared by the publisher.

    The planted file stands in for both cases at once, which is the whole argument: from `gc`'s
    position a retired team's leftover shard and a live team's not-yet-named shard are the same
    two bytes on disk. It reports them and holds; the next build -- which is the only thing that
    knows which one it is looking at -- removes them, along with the directory they emptied.
    """

    slug = _built_before_the_flip(tmp_path)
    orphan_dir = tmp_path / SCHEMA_3_ROOT / "timeline" / "retired-team"
    orphan_dir.mkdir(parents=True)
    orphan = orphan_dir / "2026-05-04.jsonl.gz"
    orphan.write_bytes(b"\x1f\x8b" + b"0" * 64)
    (orphan_dir / "2026-05-04.jsonl.gz.index.jsonl").write_text("{}\n", encoding="utf-8")

    shards = plan_collection(tmp_path).category("schema-3-orphan-shards")
    assert not shards.reclaimable
    assert {item.relative_path for item in shards.files} == {
        f"{SCHEMA_3_ROOT}/timeline/retired-team/2026-05-04.jsonl.gz",
        f"{SCHEMA_3_ROOT}/timeline/retired-team/2026-05-04.jsonl.gz.index.jsonl",
    }
    # A live team's shards are never in that set.
    assert all(f"/{slug}/" not in item.relative_path for item in shards.files)
    # While it is there the reader falls back rather than trusting a catalogue older than its
    # tree, so the archive is slower but never wrong.
    assert "named by no entry" in TimelineQuery(tmp_path).schema_3_declined

    collect(tmp_path, delete=True)
    assert orphan.is_file()

    build_archive(tmp_path, slug)

    assert not orphan.exists()
    assert not orphan_dir.exists()
    assert TimelineQuery(tmp_path).schema_3_declined == ""
    assert plan_collection(tmp_path).category("schema-3-orphan-shards").count == 0


def test_a_retained_object_is_held_and_an_unreferenced_one_is_reclaimed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The schema-2 accounting, extended rather than replaced.

    ``retained_objects`` is the previous generation's objects, kept alive one more generation so
    that a browser which already loaded the previous bootstrap can still fetch what it names.
    `gc` reclaiming them would collapse that grace period to zero at exactly the moment it is
    being used, since the operator most likely to run `gc` has just rebuilt. The orphan set is
    derived as ``on disk - current - retained``, so planting one of each has to reclaim exactly
    one.
    """

    # The pre-flip world, because that is where these two categories exist at all: once the format
    # is retired they are subsumed into `superseded-schema-2`, which
    # `test_the_four_schema_two_categories_partition_the_generation` asserts from the other side.
    _built_before_the_flip(tmp_path)
    _publishing_schema_2(monkeypatch)
    manifest_path = tmp_path / _V2_MANIFEST
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    retained = f"{_OBJECT_ROOT}/{'a' * 64}.json"
    orphan = f"{_OBJECT_ROOT}/{'b' * 64}.json"
    (tmp_path / retained).write_text('{"kept": true}', encoding="utf-8")
    (tmp_path / orphan).write_text('{"unreferenced": true}', encoding="utf-8")
    manifest["retained_objects"] = sorted({*manifest["retained_objects"], retained})
    manifest_path.write_text(json.dumps(manifest), encoding="utf-8")

    report = plan_collection(tmp_path)
    assert not report.category("schema-2-retained-objects").reclaimable
    assert retained in {
        item.relative_path for item in report.category("schema-2-retained-objects").files
    }
    orphans = report.category("schema-2-orphan-objects")
    assert orphans.reclaimable
    assert {item.relative_path for item in orphans.files} == {orphan}

    collect(tmp_path, delete=True)
    assert (tmp_path / retained).is_file()
    assert not (tmp_path / orphan).exists()


def test_an_unreadable_v2_manifest_holds_every_object(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """An absent reachability record means "an older tool wrote these", not "these are dead".

    `timeline_shards._previous_objects` reads the same absence the same way. A `gc` that read it
    as an empty current set would classify the entire object store as orphaned.
    """

    _built_before_the_flip(tmp_path)
    _publishing_schema_2(monkeypatch)
    before = sorted((tmp_path / _OBJECT_ROOT).iterdir())
    assert before
    (tmp_path / _V2_MANIFEST).unlink()

    orphans = plan_collection(tmp_path).category("schema-2-orphan-objects")
    assert not orphans.reclaimable
    assert orphans.count == 0
    collect(tmp_path, delete=True)
    assert sorted((tmp_path / _OBJECT_ROOT).iterdir()) == before


# -- retiring the schema-2 generation itself -------------------------------------------------


def test_the_whole_schema_two_generation_is_held_while_the_writer_still_emits_it(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The precondition the monolith never needed, and the state every archive used to be in.

    Simulated now rather than observed: the release has flipped, so a run of the shipping tool can
    no longer produce a writer that still emits this generation. The reason string is worth keeping
    a test for anyway, because it is what an operator reads before deciding not to sweep.

    Schema 3 is complete here and the archive is perfectly readable without schema 2, so the
    superseding half of the gate is satisfied and the 1.4 GB is *still* held -- because the tool
    writes this generation on every build, and reclaiming a build's own output is churn dressed
    up as collection. The reason has to name the constant rather than describe it: an operator
    looking at the largest held number in the report is entitled to know exactly what would have
    to change, and "the writer still emits it" without a symbol is not something anyone can act
    on.
    """

    _built_before_the_flip(tmp_path)
    _publishing_schema_2(monkeypatch)
    # Asked through the predicate, not through the name imported at module load. The whole reason
    # `schema_2_is_published` exists is that an importer who bound the constant once would keep the
    # value it imported -- and this test is the one place that would notice, by asserting a state
    # the process is not actually in.
    assert schema_2_is_published()

    report = plan_collection(tmp_path)
    superseded = report.category("superseded-schema-2")

    assert not superseded.reclaimable
    assert "timeline_shards.SCHEMA_2_IS_PUBLISHED is True" in superseded.reason
    # Both halves of the sentence, because only one of them was ever asserted and the other went
    # stale in silence. This branch is only reachable in a tree that has turned the constant back
    # on, which is exactly the reader least able to check the claim for themselves, and the claim
    # it used to make -- "static/app.js has no schema-3 mode" -- had become false. So the
    # sentence's *account of why* is pinned here alongside the symbol it names.
    assert "static/app.js" in superseded.reason
    assert "has no schema-3 mode" not in superseded.reason
    assert SCHEMA_3_BOOTSTRAP_PATH in superseded.reason
    # The manifest is in it, and so are the objects the manifest still names -- the ones no
    # category reported before this one existed. Since schema 3 publishes a search corpus of its
    # own, the bootstrap is in it too: it is the schema-2 search catalogue, and a catalogue whose
    # corpus has a successor is part of the duplicate generation rather than apart from it.
    paths = {item.relative_path for item in superseded.files}
    assert _V2_MANIFEST in paths
    assert any(value.startswith(_OBJECT_ROOT) for value in paths)
    assert SCHEMA_2_BOOTSTRAP_PATH in paths
    assert report.category("schema-2-search-corpus").count == 0
    assert superseded.bytes > 0

    collect(tmp_path, delete=True)
    assert (tmp_path / SCHEMA_2_BOOTSTRAP_PATH).is_file()
    assert (tmp_path / _V2_MANIFEST).is_file()


@pytest.mark.parametrize("retired", (False, True))
def test_the_four_schema_two_categories_partition_the_generation(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, retired: bool
) -> None:
    """No file in two categories, and nothing in the generation in none of them.

    Overlap here is not a reporting blemish. A file listed in a reclaimable category twice is
    moved twice, and the second `os.replace` raises on a path that is no longer there, so a
    sweep would abort partway with a traceback. The property is asserted against the filesystem
    rather than against the sum of the parts, so a category that quietly stopped listing
    something fails here rather than in an operator's byte count.

    Asserted in **both** worlds, because the retirement is where a partition is easiest to lose:
    the live branch subtracts three sets from the generation and the retired branch subtracts one,
    and the one it keeps subtracting -- the search corpus -- is the one whose absence would be a
    deletion rather than a miscount. The retired world is now the default and the live one is the
    one that has to be simulated, which is the only thing about this test the flip changed.
    """

    _built_before_the_flip(tmp_path)
    planted = f"{_OBJECT_ROOT}/{'c' * 64}.json"
    (tmp_path / planted).write_text('{"unreferenced": true}', encoding="utf-8")
    if not retired:
        # The pre-flip release, which no shipping build can produce any more.
        _publishing_schema_2(monkeypatch)

    report = plan_collection(tmp_path)
    listings = [
        [item.relative_path for item in report.category(name).files]
        for name in (
            "superseded-schema-2",
            "schema-2-search-corpus",
            "schema-2-orphan-objects",
            "schema-2-retained-objects",
        )
    ]
    flat = [value for listing in listings for value in listing]
    assert len(flat) == len(set(flat))

    on_disk = {
        path.relative_to(tmp_path).as_posix()
        for path in (tmp_path / SCHEMA_2_ROOT).rglob("*")
        if path.is_file()
    }
    on_disk.update(
        value
        for value in (SCHEMA_2_BOOTSTRAP_PATH, SCHEMA_2_BOOTSTRAP_PATH + ".gz")
        if (tmp_path / value).is_file()
    )
    assert set(flat) == on_disk
    assert planted in set(flat)


def test_a_retired_schema_two_goes_presentation_first_and_the_archive_still_reads(
    tmp_path: Path,
) -> None:
    """The day the writer stops: the superseded half goes, the unsuperseded half stays.

    The current/retained split is not preserved through the retirement, and that is the point of
    subsuming rather than extending. The grace period exists for a browser holding the previous
    bootstrap, and the precondition for retiring the format is that no browser reads it -- so
    holding 305 objects for a reader that cannot exist would be hoarding with a citation. The
    two subsumed categories keep their reasons and lose their files, because a zero that does not
    explain itself is not an answer.

    What is *not* subsumed is the transcript search corpus, and the oracle is what proves it:
    ``_answers`` asks a search as well as the two spine questions, so an archive that came
    through this sweep with `timeline search` broken fails here rather than being reported as
    "still reads". The bootstrap survives for the same reason the objects do -- it is the search
    catalogue, not only the schema-2 timeline header.

    The archive is put back into the pre-corpus state first, because that is the only state in
    which there is an unsuperseded half left to keep. On a freshly built archive the corpus has a
    schema-3 successor and the whole generation goes, which is
    :func:`test_the_transcript_search_corpus_is_released_once_schema_three_publishes_one`.
    """

    _built_before_the_flip(tmp_path)
    _unpublish_the_schema_three_search_corpus(tmp_path)
    before = _answers(tmp_path)
    _assert_the_website_reads_schema_three(tmp_path)

    report = plan_collection(tmp_path)
    superseded = report.category("superseded-schema-2")
    corpus = report.category("schema-2-search-corpus")
    assert superseded.reclaimable
    assert superseded.count > 3
    assert not corpus.reclaimable
    assert corpus.count > 0
    assert report.category("schema-2-orphan-objects").count == 0
    assert report.category("schema-2-retained-objects").count == 0
    for name in ("schema-2-orphan-objects", "schema-2-retained-objects"):
        assert "subsumed by superseded-schema-2" in report.category(name).reason

    swept = collect(tmp_path, delete=True)

    assert (tmp_path / SCHEMA_2_BOOTSTRAP_PATH).is_file()
    assert not (tmp_path / _V2_MANIFEST).exists()
    assert {item.relative_path for item in swept.moved} & {
        item.relative_path for item in corpus.files
    } == set()
    # The archive still answers all three questions: two out of schema 3, one out of what is
    # left of schema 2.
    assert _answers(tmp_path) == before

    payload = tmp_path / str(swept.trash_generation) / "files"
    shutil.copytree(payload, tmp_path, dirs_exist_ok=True)
    assert (tmp_path / _V2_MANIFEST).is_file()
    assert _answers(tmp_path) == before

    # A second run against an archive that has nothing but the corpus left says so plainly,
    # rather than repeating a warning about a decision nobody is being offered.
    collect(tmp_path, delete=True)
    absent = plan_collection(tmp_path).category("superseded-schema-2")
    assert not absent.reclaimable
    assert absent.count == 0
    assert "nothing to collect" in absent.reason
    assert "transcript search corpus, held above" in absent.reason
    assert _answers(tmp_path) == before


def test_the_reclaim_reason_warns_that_it_removes_the_last_fallback(tmp_path: Path) -> None:
    """The warning is part of the category, not part of a document.

    Removing schema 2 removes the last thing behind schema 3, and clause 5 declines a whole
    schema-3 generation over one shard-shaped file the catalogue does not name -- so after this
    sweep an interrupted build is the difference between a slow archive and an unreadable one.
    That is recoverable by a rebuild and it is still a change of failure mode, so it is said in
    the dry run, which is the one artefact an operator is guaranteed to read before typing
    `--delete`. The monolith's own availability in the same pass is said too, because one
    `--delete` takes both and no reason that omitted that would be describing what happens.
    """

    _built_before_the_flip(tmp_path)
    (tmp_path / SCHEMA_1_TIMELINE_PATH).write_text("{}", encoding="utf-8")
    _assert_the_website_reads_schema_three(tmp_path)

    report = plan_collection(tmp_path)
    reason = report.category("superseded-schema-2").reason
    assert "WARNING" in reason
    assert "last fallback" in reason
    assert SCHEMA_1_TIMELINE_PATH in reason
    assert "one --delete takes both" in reason
    assert report.category("schema-1-monolith").reclaimable
    assert reason in format_gc_report(report, "text")
    # An ingest archive can be rebuilt from what is beside it, and the reason says so only when
    # that is true here -- see the export case below.
    assert "reads the raw turns already reachable from here" in reason

    # With the monolith already gone -- the state a migrated archive is actually in -- the same
    # reason says the sharper thing rather than the same thing.
    (tmp_path / SCHEMA_1_TIMELINE_PATH).unlink()
    sharper = plan_collection(tmp_path).category("superseded-schema-2").reason
    assert "is already gone" in sharper


def test_a_retired_schema_two_is_still_held_without_a_complete_schema_three(
    tmp_path: Path,
) -> None:
    """Both halves of the gate, not either: a retired writer is not a readable archive.

    This is the dangerous combination, and the one an operator can create by accident. The tool
    has stopped writing schema 2, so nothing will replace it -- and the generation in front of it
    is a shard short, so the reader will not answer from it. Reclaiming here leaves an archive
    that cannot be read at all, which is exactly the outcome the completeness clause exists to
    prevent, and it has to be judged by the reader's own rule rather than by a restatement.
    """

    _built_before_the_flip(tmp_path)
    shard = next((tmp_path / SCHEMA_3_ROOT).rglob("*.jsonl.gz"))
    shard.write_bytes(shard.read_bytes()[:-8])

    superseded = plan_collection(tmp_path).category("superseded-schema-2")
    assert not superseded.reclaimable
    assert "no complete schema-3 generation supersedes it" in superseded.reason

    collect(tmp_path, delete=True)
    assert (tmp_path / SCHEMA_2_BOOTSTRAP_PATH).is_file()
    assert TimelineQuery(tmp_path).list_records("agents", QueryFilters())


def test_the_writer_cannot_run_and_no_build_asks_it_to(tmp_path: Path) -> None:
    """What makes the constant evidence rather than a comment, now that it has been acted on.

    `gc` hands 1.4 GB to the trash on the strength of one boolean, so the boolean has to be a fact
    about the writer, and both halves of that fact are asserted here. The writer **refuses**, so
    the flip could not have been a one-line edit that left builds quietly republishing the
    generation an operator had just reclaimed. And the call sites are **gone**, so a build does not
    merely fail to write schema 2, it does not ask -- which is the half that had to change for the
    refusal to be survivable.

    The second assertion is the one that would rot first. A future edit that reinstated the call
    would fail the build loudly, which is the guard working; an edit that reinstated it *and*
    lifted the constant would be silent, and this is what notices.
    """

    slug = _built(tmp_path)

    with pytest.raises(ValueError, match="SCHEMA_2_IS_PUBLISHED is False"):
        write_timeline_shards(tmp_path, {"schema_version": 1})

    build_archive(tmp_path, slug)
    assert not (tmp_path / SCHEMA_2_BOOTSTRAP_PATH).exists()
    assert not (tmp_path / SCHEMA_2_ROOT).exists()
    manifest = json.loads((tmp_path / _EXPORT_MANIFEST).read_text(encoding="utf-8"))
    assert not any(
        str(value).startswith(SCHEMA_2_ROOT) or value == SCHEMA_2_BOOTSTRAP_PATH
        for value in manifest["generated_files"]
    )


def test_the_constant_and_the_website_cannot_drift_apart(tmp_path: Path) -> None:
    """The other direction of the flip, which no build would catch.

    The website is the reason schema 2 is still written, so retiring the format while
    `static/app.js` has no schema-3 mode would retire the graphical surface -- and it would do it
    silently, because the browser's failure is a fetch that 404s in someone else's session hours
    later, not a build that stops. Asserted against the bundled asset, which is what a build
    copies into an archive.

    Deliberately **one-directional**, and the direction that is missing is the interesting one.
    An earlier form of this test asserted the biconditional -- schema-2's URL appears in the
    bundle if and only if the constant is true -- and that is wrong, not merely strict: the
    schema-2 bootstrap is also the transcript search catalogue, which the flip does not retire,
    so a correct post-flip bundle goes on fetching ``data/timeline-v2.json`` to search with. The
    claim that survives is the one about the *timeline*: after the flip the bundle must know
    where the new one lives, which is the same fact `archive_gc._website_refusal` asks each
    archive's own copy of the bundle.
    """

    app = (files("wrkviz") / "static" / "app.js").read_text(encoding="utf-8")

    if SCHEMA_2_IS_PUBLISHED:
        assert SCHEMA_2_BOOTSTRAP_PATH in app
    if not SCHEMA_2_IS_PUBLISHED:
        assert SCHEMA_3_BOOTSTRAP_PATH in app


def _schema_2_search_objects(archive: Path) -> set[str]:
    """Every object the schema-2 ``search`` catalogue names, read the way `gc` reads it.

    Membership is taken from the catalogue rather than from filenames, because the two halves of
    the generation share one content-addressed directory and are indistinguishable by name --
    which is exactly why nothing before the search category could tell them apart.
    """

    bootstrap = json.loads((archive / SCHEMA_2_BOOTSTRAP_PATH).read_text(encoding="utf-8"))
    return {
        f"{_OBJECT_ROOT}/{digest}.json"
        for shard in bootstrap["search"]["shards"]
        for digest in (shard["sha256"], (shard.get("linkage") or {}).get("sha256"))
        if digest
    }


def test_the_transcript_search_corpus_is_never_offered_while_it_is_the_only_copy(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The half of schema 2 that schema 3 did not replace, in the archives that still predate it.

    "Superseded" is a claim about a *successor*, and for an archive built before the schema-3
    search streams the transcript search corpus has none: it is schema-2 day shards with trigram
    blooms, catalogued in the bootstrap's ``search`` section, and the reader falls back to them
    from under a complete schema 3. Reclaiming them would not slow the archive down, it would
    delete `timeline search` and `timeline show` while the report said "superseded".

    Held in **both** worlds, because the retirement of the format is a different fact from the
    supersession of the corpus and the second does not follow from the first -- and this archive
    is the case that proves they are independent, since it has a complete schema 3 and no corpus
    inside it.
    """

    _built_before_the_flip(tmp_path)
    named = _schema_2_search_objects(tmp_path)
    assert named
    _unpublish_the_schema_three_search_corpus(tmp_path)

    for retired in (True, False):
        if not retired:
            _publishing_schema_2(monkeypatch)
        corpus = plan_collection(tmp_path).category("schema-2-search-corpus")
        assert not corpus.reclaimable
        assert "not superseded in this archive" in corpus.reason
        assert SCHEMA_2_BOOTSTRAP_PATH in {item.relative_path for item in corpus.files}
        assert named <= {item.relative_path for item in corpus.files}

    before = _search(tmp_path).total_matches
    assert before > 0
    collect(tmp_path, delete=True)
    for relative in sorted(named):
        assert (tmp_path / relative).is_file()
    assert _search(tmp_path).total_matches == before


def test_the_transcript_search_corpus_is_released_once_schema_three_publishes_one(
    tmp_path: Path,
) -> None:
    """The other side of the same condition, and the sweep it finally permits.

    A build now writes the corpus into schema 3 as well, so on a freshly built archive these
    objects stop being a capability and become a duplicate. The assertion that matters is not the
    empty category -- it is that the bytes moved into the category that *can* reclaim them, and
    that after the reclaim the archive still answers a transcript search. An archive that came
    through this sweep with `timeline search` broken would otherwise be reported as "collected".
    """

    _built_before_the_flip(tmp_path)
    named = _schema_2_search_objects(tmp_path)
    assert named

    corpus = plan_collection(tmp_path).category("schema-2-search-corpus")
    assert not corpus.reclaimable
    assert corpus.count == 0
    assert "publishes a transcript search corpus of its own" in corpus.reason
    superseded = plan_collection(tmp_path).category("superseded-schema-2")
    assert named <= {item.relative_path for item in superseded.files}
    assert SCHEMA_2_BOOTSTRAP_PATH in {item.relative_path for item in superseded.files}

    before = _answers(tmp_path)
    assert before[2]
    _assert_the_website_reads_schema_three(tmp_path)
    swept = collect(tmp_path, delete=True)

    assert named <= {item.relative_path for item in swept.moved}
    assert not (tmp_path / SCHEMA_2_BOOTSTRAP_PATH).exists()
    # The whole point of the port: every question still answers, out of schema 3 alone.
    assert _answers(tmp_path) == before


def test_a_schema_three_from_another_build_supersedes_nothing(tmp_path: Path) -> None:
    """The interruption completeness structurally cannot see, and the team it would have cost.

    All five completeness clauses are intra-generation, so a schema 3 that is a whole build
    behind passes every one of them. Publication is schema 2 and then schema 3, and the writer's
    lock dies with the process, so the state below is one OOM away: three teams in schema 2, two
    in a schema 3 that is otherwise perfect. Reclaiming on completeness alone takes the newer
    generation and keeps the older one, and the team in neither is gone from every readable
    generation at once.

    ``source_digest`` is what separates them, and it is not a rule invented here -- the reader
    refuses the same mismatch in `_search_bootstrap` when a search makes it look at both
    generations. `gc` has to ask before the sweep, because the reader only asks after it.
    """

    archive = tmp_path / "archive"
    output = tmp_path / "export"
    slugs: list[str] = []
    for index, slug in enumerate(("team-one", "team-two", "team-three")):
        team = _team(f"Team {index} narrative.")
        team = team.__class__(**{**team.__dict__, "team_slug": slug})
        _write_team(archive, team)
        summarize_archive(archive, slug, "heuristic", "test-model")
        slugs.append(slug)

    build_combined_archive(archive, tuple(slugs[:2]), output=output, display_timezone="UTC")
    stale_tree = tmp_path / "stale-v3"
    shutil.copytree(output / SCHEMA_3_ROOT, stale_tree)
    stale_bootstrap = (output / SCHEMA_3_BOOTSTRAP_PATH).read_bytes()

    build_combined_archive(archive, tuple(slugs), output=output, display_timezone="UTC")
    # Written before the older schema 3 is swapped back in, so that the schema 2 on disk describes
    # all three teams -- which is exactly the disagreement under test, and the order matters
    # because a real archive's schema 2 was written by the build that also wrote the newer teams.
    write_legacy_schema_2(output)
    shutil.rmtree(output / SCHEMA_3_ROOT)
    shutil.copytree(stale_tree, output / SCHEMA_3_ROOT)
    (output / SCHEMA_3_BOOTSTRAP_PATH).write_bytes(stale_bootstrap)
    _assert_the_website_reads_schema_three(output)

    # The state is exactly the dangerous one: the reader accepts schema 3, and schema 3 is a
    # team short of the schema 2 beside it.
    assert schema_3_completeness(output) == (True, "")
    assert len(TimelineQuery(output).teams) == 2
    assert len(json.loads((output / SCHEMA_2_BOOTSTRAP_PATH).read_text("utf-8"))["teams"]) == 3

    superseded = plan_collection(output).category("superseded-schema-2")
    assert not superseded.reclaimable
    assert "describe different builds" in superseded.reason

    collect(output, delete=True)
    assert (output / SCHEMA_2_BOOTSTRAP_PATH).is_file()
    assert (output / _V2_MANIFEST).is_file()

    # And the remedy is the build, which republishes schema 3 against this source -- and, being a
    # post-flip build, republishes no schema 2 at all, so the disagreement it fixes is the last one
    # that archive can have.
    build_combined_archive(archive, tuple(slugs), output=output, display_timezone="UTC")
    assert len(TimelineQuery(output).teams) == 3
    _assert_the_website_reads_schema_three(output)
    assert plan_collection(output).category("superseded-schema-2").reclaimable


def test_an_archive_whose_own_website_predates_schema_three_holds_the_generation(
    tmp_path: Path,
) -> None:
    """The one precondition a tool release cannot satisfy on an operator's behalf.

    Every other gate here is about the tool: ship a reader and every archive is ready at once.
    ``app.js`` is different, because a build *copies* it into the archive and `gc` does not
    rewrite it -- so an archive built before the flip carries a bundle that loads
    ``data/timeline-v2.json`` and falls back to ``data/timeline.json``, and this pass offers
    both. Sweeping without rebuilding is the convenience `gc` sells, and here it would spend that
    convenience on blanking the graphical surface silently, hours later, in somebody else's
    session.

    So the refusal is the positive question -- does this archive's bundle name the schema-3
    bootstrap -- and it clears the moment a build has put one there.
    """

    slug = _built_before_the_flip(tmp_path)
    _a_bundle_that_predates_schema_three(tmp_path)
    app = (tmp_path / "app.js").read_text(encoding="utf-8")
    assert SCHEMA_2_BOOTSTRAP_PATH in app
    assert SCHEMA_3_BOOTSTRAP_PATH not in app

    held = plan_collection(tmp_path).category("superseded-schema-2")
    assert not held.reclaimable
    assert "has no schema-3 mode" in held.reason
    collect(tmp_path, delete=True)
    assert (tmp_path / _V2_MANIFEST).is_file()

    # And it clears the way the reason says it does: by running the build, which republishes the
    # bundle. Not by editing a constant, not by a newer tool being installed -- the whole point of
    # this gate is that neither of those reaches into a directory.
    build_archive(tmp_path, slug)
    _assert_the_website_reads_schema_three(tmp_path)
    assert plan_collection(tmp_path).category("superseded-schema-2").reclaimable


def test_an_export_is_told_that_its_rebuild_is_somewhere_else(tmp_path: Path) -> None:
    """The recovery promise has to be true of the archive it is printed against.

    "The way back is a rebuild, which costs no tokens and reads teams/*/raw" is true of an ingest
    archive and false of a combined export: `build_combined_archive` writes
    ``teams/<slug>/summaries/`` and no ``raw/``, while writing the marker that makes `gc` willing
    to run there -- and an export is the archive most likely to be collected, being the one an
    operator keeps and serves. Told the wrong thing, they accept an irreversible-after-empty-trash
    decision on a recovery that is on another machine or already gone.
    """

    archive = tmp_path / "archive"
    output = tmp_path / "export"
    slugs: list[str] = []
    for index, slug in enumerate(("team-one", "team-two")):
        team = _team(f"Team {index} narrative.")
        team = team.__class__(**{**team.__dict__, "team_slug": slug})
        _write_team(archive, team)
        summarize_archive(archive, slug, "heuristic", "test-model")
        slugs.append(slug)
    build_combined_archive(archive, tuple(slugs), output=output, display_timezone="UTC")
    assert (output / ARCHIVE_MARKER_FILE).is_file()
    assert not any((team_build_root(output, slug) / "raw").exists() for slug in slugs)

    write_legacy_schema_2(output)
    _assert_the_website_reads_schema_three(output)
    reason = plan_collection(output).category("superseded-schema-2").reason

    assert "it cannot be run here" in reason
    assert "the ingest archive it was built from" in reason
    assert "reads the raw turns already reachable from here" not in reason


def test_source_snapshots_are_measured_and_never_touched(tmp_path: Path) -> None:
    """The largest thing in the archive is reported and refused in the same breath.

    It is 72% of the measured archive's bytes, so omitting it would make the report answer a
    different question than the one asked. It is also vendor input with a purpose-built deletion
    gate of its own, so a second, weaker gate here would be a liability.
    """

    slug = _built(tmp_path)
    snapshots = tmp_path / "teams" / slug / "source_snapshots"
    snapshots.mkdir(parents=True, exist_ok=True)
    (snapshots / "rollout.jsonl").write_text("x" * 4096, encoding="utf-8")

    category = plan_collection(tmp_path).category("ingest-source-snapshots")
    assert not category.reclaimable
    assert category.bytes >= 4096
    assert "audit-losslessness" in category.reason

    collect(tmp_path, delete=True)
    assert (snapshots / "rollout.jsonl").is_file()


# -- reversibility --------------------------------------------------------------------------


def test_a_sweep_is_undone_by_copying_the_trash_back(tmp_path: Path) -> None:
    """The whole reason the first pass renames instead of unlinking, asserted as a round trip.

    The archive costs hours to rebuild, so the test is not "the file is in the trash" but "the
    archive is the same afterwards": sweep, copy the payload tree back over the root, and read
    the same answers out of it.
    """

    _built(tmp_path)
    (tmp_path / SCHEMA_1_TIMELINE_PATH).write_text(
        json.dumps({"schema_version": 1, "marker": "restore me"}), encoding="utf-8"
    )
    before = _answers(tmp_path)

    report = collect(tmp_path, delete=True)

    assert report.action == "swept"
    assert report.trash_generation is not None
    assert not (tmp_path / SCHEMA_1_TIMELINE_PATH).exists()
    payload = tmp_path / report.trash_generation / "files"
    assert (payload / SCHEMA_1_TIMELINE_PATH).is_file()

    receipt = json.loads(
        (tmp_path / report.trash_generation / "receipt.json").read_text(encoding="utf-8")
    )
    assert receipt["kind"] == "archive-gc-trash-receipt"
    assert SCHEMA_1_TIMELINE_PATH in {entry["path"] for entry in receipt["files"]}
    assert "cp -a" in receipt["restore"]

    shutil.copytree(payload, tmp_path, dirs_exist_ok=True)
    restored = json.loads(
        (tmp_path / SCHEMA_1_TIMELINE_PATH).read_text(encoding="utf-8")
    )
    assert restored["marker"] == "restore me"
    assert _answers(tmp_path) == before


def test_emptying_the_trash_is_a_second_command(tmp_path: Path) -> None:
    """Sweeping and emptying in one invocation would reintroduce the irreversibility."""

    _built(tmp_path)
    (tmp_path / SCHEMA_1_TIMELINE_PATH).write_text("{}", encoding="utf-8")

    with pytest.raises(ArchiveGcError, match="separate passes"):
        collect(tmp_path, delete=True, empty_trash=True)
    assert (tmp_path / SCHEMA_1_TIMELINE_PATH).is_file()

    swept = collect(tmp_path, delete=True)
    assert swept.trash_generation is not None
    emptied = collect(tmp_path, empty_trash=True)
    assert emptied.action == "emptied"
    assert emptied.emptied_bytes > 0
    assert not (tmp_path / TRASH_ROOT).exists()


def test_two_sweeps_in_one_second_do_not_share_a_generation(tmp_path: Path) -> None:
    _built(tmp_path)
    (tmp_path / SCHEMA_1_TIMELINE_PATH).write_text("{}", encoding="utf-8")
    first = collect(tmp_path, delete=True)
    (tmp_path / SCHEMA_1_TIMELINE_PATH).write_text("{}", encoding="utf-8")
    second = collect(tmp_path, delete=True)

    assert first.trash_generation != second.trash_generation
    assert (tmp_path / str(first.trash_generation) / "receipt.json").is_file()
    assert (tmp_path / str(second.trash_generation) / "receipt.json").is_file()


def test_the_trash_is_gitignored_before_it_can_exist(tmp_path: Path) -> None:
    """An operator who reclaims 268 MiB must not then be offered it as a commit.

    Asserted against the ingest-time rule rather than against a built archive, because that is
    where the archive's ``.gitignore`` is written and the ordering claim is precisely that the
    entry lands before anything can occupy the directory.
    """

    _ensure_bulk_content_ignored(tmp_path)
    ignored = set((tmp_path / ".gitignore").read_text(encoding="utf-8").splitlines())
    assert f"/{TRASH_ROOT}/" in ignored
    # Idempotent, and it does not disturb what an operator added themselves.
    (tmp_path / ".gitignore").write_text(
        (tmp_path / ".gitignore").read_text(encoding="utf-8") + "/mine\n", encoding="utf-8"
    )
    assert not _ensure_bulk_content_ignored(tmp_path)
    assert "/mine" in (tmp_path / ".gitignore").read_text(encoding="utf-8")


# -- refusals -------------------------------------------------------------------------------


def test_gc_refuses_a_directory_that_is_not_an_archive(tmp_path: Path) -> None:
    with pytest.raises(ArchiveGcError, match="not a wrkviz archive"):
        collect(tmp_path)


def test_gc_waits_for_the_writer_lock(tmp_path: Path) -> None:
    """Even the dry run serializes against a build.

    A report computed beside a running build names files the build is halfway through replacing,
    and the report is the input to a human's decision about deleting them.
    """

    _built(tmp_path)
    ready = tmp_path / "holder-ready"
    holder = subprocess.Popen(
        [
            sys.executable,
            "-c",
            "import fcntl, pathlib, sys, time\n"
            "lock = pathlib.Path(sys.argv[1]) / '.wrkviz.lock'\n"
            "handle = open(lock, 'a+b')\n"
            "fcntl.flock(handle.fileno(), fcntl.LOCK_EX)\n"
            "pathlib.Path(sys.argv[2]).write_text('yes')\n"
            "time.sleep(1.5)\n",
            str(tmp_path),
            str(ready),
        ]
    )
    try:
        deadline = time.monotonic() + 20
        while not ready.is_file() and time.monotonic() < deadline:
            time.sleep(0.01)
        assert ready.is_file()
        started = time.monotonic()
        collect(tmp_path)
        assert time.monotonic() - started >= 0.5
    finally:
        holder.wait(timeout=30)


def test_gc_never_follows_a_symlink_out_of_the_archive(tmp_path: Path) -> None:
    """A symlinked shard must not become a way to delete something outside the archive."""

    _built(tmp_path)
    outside = tmp_path.parent / "outside.txt"
    outside.write_text("not ours", encoding="utf-8")
    link = tmp_path / SCHEMA_3_ROOT / "timeline" / "linked.jsonl.gz"
    os.symlink(outside, link)

    report = plan_collection(tmp_path)
    listed = {
        item.relative_path
        for category in report.categories
        for item in category.files
    }
    assert not any("linked.jsonl.gz" in value for value in listed)
    assert outside.is_file()
