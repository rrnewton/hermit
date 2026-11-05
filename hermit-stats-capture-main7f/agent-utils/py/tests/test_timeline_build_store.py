"""Build state belongs beside the published archive, not inside it.

The archive is shipped to whatever machine serves the site, so everything in it is paid for in
bandwidth and disk on every copy. On the measured archive 717 MiB of the 1.5 GiB was intermediate
state that neither the browser nor the shipped CLI ever opens. These tests pin the relocation and,
more importantly, the two refusals: the ways an archive could end up with its build state split
across two trees are the ways a rebuild silently loses a team.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from wrkviz.build_store import (
    BUILD_POINTER_FILE,
    BuildLocationError,
    TENANTS,
    candidate_store_roots,
    default_store_root,
    ingested_team_slugs,
    legacy_team_roots,
    migrate_build_state,
    plan_build_migration,
    resolve_build_root,
    team_build_root,
)


def _legacy_team(archive: Path, slug: str, *, body: str = "{}") -> Path:
    """Write a team in the layout an older tool produced, inside the archive."""

    root = archive / "teams" / slug
    (root / "raw").mkdir(parents=True)
    (root / "raw" / "team.json").write_text(body, encoding="utf-8")
    (root / "raw" / "source-manifest.json").write_text('{"provider":"codex"}', encoding="utf-8")
    (root / "summaries").mkdir(parents=True)
    (root / "summaries" / "kept.md").write_text("paid\n", encoding="utf-8")
    (root / "summary_data").mkdir(parents=True)
    (root / "summary_data" / "cache.json").write_text("{}", encoding="utf-8")
    return root


def test_a_fresh_archive_puts_build_state_in_the_sibling_store(tmp_path: Path) -> None:
    archive = tmp_path / "widget"
    archive.mkdir()
    location = resolve_build_root(archive, "team-one")
    assert location.layout == "external"
    assert location.origin == "default"
    assert location.root == tmp_path / "widget.build" / "team-one"
    assert not location.inside_archive


def test_an_unmigrated_archive_keeps_working_where_it_is(tmp_path: Path) -> None:
    """No flag, no migration, no move: the whole point of branch 3.

    A build that silently relocated somebody's archive as a side effect of being run for some
    other reason is the behaviour an explicit migration exists to forbid.
    """

    archive = tmp_path / "widget"
    _legacy_team(archive, "team-one")
    location = resolve_build_root(archive, "team-one")
    assert location.layout == "in-archive-legacy"
    assert location.inside_archive
    assert location.root == archive / "teams" / "team-one"
    assert location.store_root is None


def test_summaries_stay_in_the_archive_and_raw_does_not(tmp_path: Path) -> None:
    """The line this whole module draws: rendered output stays, intermediate state goes.

    ``summaries/`` is what the shipped ``timeline`` CLI reads for ``stats`` and ``search``, and it
    is what the tokens were spent on. ``summary_data/`` stays too, for the separate reason that
    the spend receipts live inside it -- see the module docstring.
    """

    archive = tmp_path / "widget"
    _legacy_team(archive, "team-one")
    assert migrate_build_state(archive, apply=True).moved

    assert (archive / "teams" / "team-one" / "summaries" / "kept.md").is_file()
    assert (archive / "teams" / "team-one" / "summary_data" / "cache.json").is_file()
    assert not (archive / "teams" / "team-one" / "raw").exists()
    assert (tmp_path / "widget.build" / "team-one" / "raw" / "team.json").is_file()
    assert set(TENANTS) == {"raw", "payloads"}


def test_a_migration_is_a_dry_run_until_it_is_told_otherwise(tmp_path: Path) -> None:
    archive = tmp_path / "widget"
    _legacy_team(archive, "team-one")

    plan = plan_build_migration(archive)
    assert [team.team_slug for team in plan.teams] == ["team-one"]
    assert plan.files == 2
    assert plan.bytes > 0

    result = migrate_build_state(archive)
    assert result.moved == ()
    assert not result.pointer_written
    assert (archive / "teams" / "team-one" / "raw" / "team.json").is_file()
    assert not (tmp_path / "widget.build").exists()


def test_a_completed_migration_records_the_layout_and_ignores_itself(tmp_path: Path) -> None:
    archive = tmp_path / "widget"
    _legacy_team(archive, "team-one")
    result = migrate_build_state(archive, apply=True)

    assert result.pointer_written
    pointer = json.loads((archive / BUILD_POINTER_FILE).read_text(encoding="utf-8"))
    assert pointer["layout"] == "external"
    # Relative, so an archive copied together with its store keeps working.
    assert pointer["store_root"] == "../widget.build"
    # `/*` ignores every entry including the .gitignore, so a store that lands inside somebody's
    # repository is invisible rather than being proposed as a three-quarter-gigabyte addition.
    assert (tmp_path / "widget.build" / ".gitignore").read_text(encoding="utf-8") == "/*\n"

    after = resolve_build_root(archive, "team-one")
    assert after.origin == "recorded"
    assert after.root == tmp_path / "widget.build" / "team-one"


def test_an_interrupted_migration_says_finish_it_rather_than_starting_again(
    tmp_path: Path,
) -> None:
    """A pointer saying "external" beside populated in-archive state is not a fresh archive.

    Resolving it as one would build the team twice into two trees, and the second build would
    read a baseline that is not the one the first wrote. Asked per team on purpose, so the teams
    already moved keep building while the rest wait.
    """

    archive = tmp_path / "widget"
    _legacy_team(archive, "moved")
    _legacy_team(archive, "stranded")
    migrate_build_state(archive, apply=True)
    # Put one team back, as an interrupted move would have left it.
    (archive / "teams" / "stranded" / "raw").mkdir(parents=True)
    (archive / "teams" / "stranded" / "raw" / "team.json").write_text("{}", encoding="utf-8")

    assert resolve_build_root(archive, "moved").origin == "recorded"
    with pytest.raises(BuildLocationError, match="interrupted migration"):
        resolve_build_root(archive, "stranded")


def test_a_requested_root_refuses_while_any_team_is_still_inside(tmp_path: Path) -> None:
    """One archive, one layout -- asked of the archive, not of the team being built.

    A new team given ``--build-root`` beside eleven un-migrated ones would otherwise succeed,
    record ``external`` archive-wide, and leave the other eleven refusing on the next run.
    """

    archive = tmp_path / "widget"
    _legacy_team(archive, "old-team")
    with pytest.raises(BuildLocationError, match="still keep build state inside the archive"):
        resolve_build_root(archive, "new-team", tmp_path / "elsewhere")


def test_discovery_sees_both_layouts_at_once(tmp_path: Path) -> None:
    """A part-migrated archive has teams in each tree, and neither half is un-ingested.

    Three call sites used to spell this test inline against ``teams/<slug>/``. A discovery loop
    that hard-codes one layout is what turns a relocation into a silent "no teams found".
    """

    archive = tmp_path / "widget"
    _legacy_team(archive, "moved")
    migrate_build_state(archive, apply=True)
    _legacy_team(archive, "not-yet")

    assert ingested_team_slugs(archive) == ("moved", "not-yet")
    assert [path.name for path in legacy_team_roots(archive)] == ["not-yet"]
    assert candidate_store_roots(archive)[-1] == archive / "teams"


def test_nothing_to_migrate_is_reported_rather_than_treated_as_an_error(
    tmp_path: Path,
) -> None:
    archive = tmp_path / "widget"
    archive.mkdir()
    result = migrate_build_state(archive, apply=True)
    assert result.moved == ()
    # Still establishes the store, which is how a fresh archive is told where its state goes.
    assert result.pointer_written
    assert resolve_build_root(archive, "team-one").origin == "recorded"


def test_the_store_refuses_to_overwrite_state_already_there(tmp_path: Path) -> None:
    """A second migration into an occupied target is a collision, not an update.

    Overwriting would silently discard whichever copy is not the one the operator meant, and
    there is no way to tell from here which that is.
    """

    archive = tmp_path / "widget"
    _legacy_team(archive, "team-one")
    (tmp_path / "widget.build" / "team-one" / "raw").mkdir(parents=True)
    (tmp_path / "widget.build" / "team-one" / "raw" / "team.json").write_text(
        '{"other":true}', encoding="utf-8"
    )

    with pytest.raises(BuildLocationError, match="refusing to overwrite"):
        migrate_build_state(archive, apply=True)
    assert (archive / "teams" / "team-one" / "raw" / "team.json").is_file()


def test_the_accessor_and_the_default_agree(tmp_path: Path) -> None:
    archive = tmp_path / "widget"
    archive.mkdir()
    assert default_store_root(archive) == tmp_path / "widget.build"
    assert team_build_root(archive, "team-one") == tmp_path / "widget.build" / "team-one"
    # Resolution alone must not bring a store into existence.
    assert not (tmp_path / "widget.build").exists()


def test_the_transcript_baseline_is_rerun_state_and_lives_in_the_store(
    tmp_path: Path,
) -> None:
    """The monotonic union is input to the next extraction, not output of this one.

    It is the largest thing the exporter writes -- 106.3 MiB against 110.2 MiB for all three
    published projections combined on the measured archive -- and nothing that consumes an
    archive opens it. Shipping it meant every machine that served the site carried a copy of
    the exporter's scratch space.
    """

    from wrkviz.build_store import SHARED_TENANT, shared_build_root

    archive = tmp_path / "widget"
    archive.mkdir()
    root = shared_build_root(archive)
    assert root == tmp_path / "widget.build" / SHARED_TENANT
    # Reserved rather than merely unused: `validate_team_slug` rejects a leading underscore, so
    # this entry cannot collide with a team however the archive grows.
    with pytest.raises(ValueError):
        resolve_build_root(archive, SHARED_TENANT)


def test_an_unmigrated_archive_keeps_its_baseline_beside_the_projection(
    tmp_path: Path,
) -> None:
    """Branch 3 again, and it has to hold here too.

    An archive nobody has migrated has no store to put the baseline in, and inventing one as a
    side effect of an extraction is the silent relocation this design refuses everywhere else.
    """

    from wrkviz.build_store import shared_build_root

    archive = tmp_path / "widget"
    _legacy_team(archive, "team-one")
    assert shared_build_root(archive) == archive / "extracted" / "transcripts"
