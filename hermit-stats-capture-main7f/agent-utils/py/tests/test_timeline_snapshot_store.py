"""The vendor snapshots leave the published tree, and an existing archive gets there safely.

The subject of this file is a *location*, so unlike the rest of the suite it names paths
literally. `tests/timeline_snapshots.py` exists so that assertions which merely need to find a
snapshot ask the resolver; here the resolver's answer is the thing under test, and a test that
asked it where the snapshots are and then asserted that is where they are would assert nothing.
"""

from __future__ import annotations

import hashlib
import http.client
import json
import os
import shutil
import threading
from pathlib import Path

import pytest

from wrkviz.build_store import team_build_root
from wrkviz.archive_gc import plan_collection
from wrkviz.cli import main as timeline_main
from wrkviz.losslessness import audit_codex_losslessness
from wrkviz.pipeline import build_archive, ingest_codex, ingest_orc
from wrkviz.project_config import load_project_ingest_config
from wrkviz.snapshot_store import (
    SNAPSHOT_POINTER_FILE,
    SnapshotLocationError,
    default_store_root,
    migrate_snapshots,
    plan_snapshot_migration,
    resolve_snapshot_root,
)
from wrkviz.standalone_server import make_static_server
from tests.test_timeline_orc import ROOT as ORC_ROOT, _append_task_note, _fixture as _orc_fixture
from tests.test_timeline_source_snapshots import (
    ROOT as CODEX_ROOT,
    _first_ingest,
    _origin,
    _root_bytes,
)


def _tree(root: Path) -> dict[str, bytes]:
    """Every regular file under *root*, keyed by its path relative to it."""

    result: dict[str, bytes] = {}
    for dirpath, _dirnames, filenames in os.walk(root):
        for name in filenames:
            path = Path(dirpath) / name
            if path.is_symlink() or not path.is_file():
                continue
            result[str(path.relative_to(root))] = path.read_bytes()
    return result


def _legacy_archive(tmp_path: Path) -> tuple[Path, Path]:
    """Ingest a Codex team the way an older tool did: snapshots inside the archive.

    Built by ingesting normally and then moving the tree back, rather than by hand-writing files,
    so the "old layout" this suite migrates from is the layout the previous code actually produced
    -- manifest digests, complete-line trimming and all.
    """

    sessions, archive, _origin_path = _first_ingest(tmp_path)
    store = default_store_root(archive)
    legacy = archive / "teams" / "codex-test" / "source_snapshots"
    legacy.parent.mkdir(parents=True, exist_ok=True)
    shutil.move(str(store / "codex-test"), str(legacy))
    shutil.rmtree(store)
    (archive / SNAPSHOT_POINTER_FILE).unlink()
    return sessions, archive


# -- the default ------------------------------------------------------------------------------


def test_a_fresh_archive_keeps_no_snapshot_inside_the_published_tree(tmp_path: Path) -> None:
    """The whole point, stated as a property of the archive rather than of one file.

    Asserted as "no path under --output has a `source_snapshots` component and the vendor bytes
    are somewhere else", not as "this one file moved", because the requirement is about the tree.
    """

    _sessions, archive, _origin_path = _first_ingest(tmp_path)

    assert not list(archive.rglob("source_snapshots"))
    store = archive.parent / (archive.name + ".sources")
    assert store.is_dir()
    copied = store / "codex-test" / "2026" / "08" / "05" / "rollout-root.jsonl"
    assert copied.read_bytes() == _root_bytes()


def test_the_store_ignores_itself_and_says_whose_it_is(tmp_path: Path) -> None:
    """A multi-gigabyte tree beside a repository must not appear in `git status`.

    `/*` ignores every entry including the `.gitignore` that declares it, which is what makes the
    whole directory invisible rather than merely mostly-ignored. The archive's own `.gitignore`
    cannot cover this tree at all, because the tree is outside the archive.
    """

    _sessions, archive, _origin_path = _first_ingest(tmp_path)
    store = default_store_root(archive)

    assert (store / ".gitignore").read_text(encoding="utf-8").splitlines()[-1] == "/*"
    marker = json.loads((store / ".snapshot-store.json").read_text(encoding="utf-8"))
    assert marker["kind"] == "source-snapshot-store"
    assert (store / marker["archive"]).resolve() == archive.resolve()


def test_the_manifest_records_the_root_relative_to_the_archive(tmp_path: Path) -> None:
    """Tracked and served, so never absolute; and relative so a moved pair keeps resolving."""

    _sessions, archive, _origin_path = _first_ingest(tmp_path)

    manifest = json.loads(
        (team_build_root(archive, "codex-test") / "raw" / "source-manifest.json").read_text(
            encoding="utf-8"
        )
    )
    recorded = manifest["snapshot_root"]
    assert not Path(recorded).is_absolute()
    assert (archive / recorded).resolve() == (default_store_root(archive) / "codex-test")


def test_the_layout_is_recorded_rather_than_rederived(tmp_path: Path) -> None:
    """A configured store must survive the operator forgetting the flag on the next run.

    Without the pointer this is the silent failure the module exists to prevent: the second run
    would resolve the default, find it empty, snapshot everything again, and leave two valid trees
    where the operator believes there is one.
    """

    sessions = tmp_path / "sessions"
    archive = tmp_path / "archive"
    elsewhere = tmp_path / "elsewhere"
    origin = _origin(sessions)
    origin.parent.mkdir(parents=True)
    origin.write_bytes(_root_bytes())

    ingest_codex(archive, sessions, CODEX_ROOT, "codex-test", "UTC", snapshot_root=elsewhere)
    assert (elsewhere / "codex-test").is_dir()
    assert not default_store_root(archive).exists()

    ingest_codex(archive, sessions, CODEX_ROOT, "codex-test", "UTC")

    assert not default_store_root(archive).exists()
    assert resolve_snapshot_root(archive, "codex-test").root == elsewhere / "codex-test"


def test_a_store_inside_the_archive_is_refused(tmp_path: Path) -> None:
    """The store's one defining property is negative, so it is enforced negatively."""

    sessions = tmp_path / "sessions"
    archive = tmp_path / "archive"
    origin = _origin(sessions)
    origin.parent.mkdir(parents=True)
    origin.write_bytes(_root_bytes())

    with pytest.raises(SnapshotLocationError, match="outside the published tree"):
        ingest_codex(
            archive,
            sessions,
            CODEX_ROOT,
            "codex-test",
            "UTC",
            snapshot_root=archive / "inside",
        )


def test_a_store_that_belongs_to_another_archive_is_refused(tmp_path: Path) -> None:
    """Two archives sharing a store collide the moment they share a slug, which they will."""

    sessions = tmp_path / "sessions"
    origin = _origin(sessions)
    origin.parent.mkdir(parents=True)
    origin.write_bytes(_root_bytes())
    shared = tmp_path / "shared"
    ingest_codex(
        tmp_path / "first", sessions, CODEX_ROOT, "codex-test", "UTC", snapshot_root=shared
    )

    with pytest.raises(SnapshotLocationError, match="belongs to"):
        ingest_codex(
            tmp_path / "second", sessions, CODEX_ROOT, "codex-test", "UTC", snapshot_root=shared
        )


# -- the archive nobody has migrated ------------------------------------------------------------


def test_an_archive_with_snapshots_in_the_old_place_keeps_working_untouched(
    tmp_path: Path,
) -> None:
    """"Must either keep working or be migrated deliberately" -- this is the first half.

    Not merely "does not crash": the incremental path is exercised, because the in-archive tree is
    the append guard's baseline and an ingest that quietly started a second tree elsewhere would
    also pass a crash test.
    """

    sessions, archive = _legacy_archive(tmp_path)
    legacy = archive / "teams" / "codex-test" / "source_snapshots"
    before = _tree(legacy)

    appended = _root_bytes()
    _origin(sessions).write_bytes(appended)
    _team, report = ingest_codex(archive, sessions, CODEX_ROOT, "codex-test", "UTC")

    assert report.snapshot_root_layout == "in-archive-legacy"
    assert _tree(legacy) == before
    assert not default_store_root(archive).exists()
    assert resolve_snapshot_root(archive, "codex-test").root == legacy


def test_a_requested_root_beside_a_populated_old_layout_is_refused(tmp_path: Path) -> None:
    """Splitting one archive's snapshots across two trees is the one outcome with no owner."""

    sessions, archive = _legacy_archive(tmp_path)

    with pytest.raises(SnapshotLocationError, match="migrate-snapshots"):
        ingest_codex(
            archive,
            sessions,
            CODEX_ROOT,
            "codex-test",
            "UTC",
            snapshot_root=tmp_path / "elsewhere",
        )


def test_an_interrupted_migration_is_refused_by_name_and_finished_by_rerunning(
    tmp_path: Path,
) -> None:
    """The pointer is written before the first byte moves, so a crash is resumable.

    Simulated at exactly the point the ordering protects: pointer recorded, team not yet moved.
    The wrong ordering would leave this state looking like a never-ingested team, which is the one
    misreading that is indistinguishable from a correct reading.
    """

    sessions, archive = _legacy_archive(tmp_path)
    store = default_store_root(archive)
    (archive / SNAPSHOT_POINTER_FILE).write_text(
        json.dumps(
            {
                "schema_version": 1,
                "kind": "source-snapshot-location",
                "layout": "external",
                "root": os.path.relpath(store, archive),
            }
        )
        + "\n",
        encoding="utf-8",
    )

    with pytest.raises(SnapshotLocationError, match="interrupted migration"):
        resolve_snapshot_root(archive, "codex-test")

    migrate_snapshots(archive, move=True)

    assert resolve_snapshot_root(archive, "codex-test").root == store / "codex-test"
    _team, report = ingest_codex(archive, sessions, CODEX_ROOT, "codex-test", "UTC")
    assert report.snapshot_root_layout == "external"


# -- the migration ------------------------------------------------------------------------------


def test_the_dry_run_moves_nothing_and_reports_what_it_would_move(tmp_path: Path) -> None:
    """Default-safe, in the same shape `gc` uses: a report is not a decision."""

    _sessions, archive = _legacy_archive(tmp_path)
    legacy = archive / "teams" / "codex-test" / "source_snapshots"
    before = _tree(legacy)

    plan = plan_snapshot_migration(archive)

    assert [team.team_slug for team in plan.teams] == ["codex-test"]
    assert plan.total_files == len(before)
    assert plan.total_bytes == sum(len(value) for value in before.values())
    assert plan.refusals == ()
    assert _tree(legacy) == before
    assert not default_store_root(archive).exists()


def test_the_move_is_byte_for_byte_and_incremental_ingest_survives_it(tmp_path: Path) -> None:
    """The requirement that decides whether this is a relocation or a data loss.

    Orc rather than Codex, because Orc is the provider whose guard actually *reads* the previous
    snapshot: `_prepare_snapshot_candidate` opens the content-addressed database, checks it
    against the digest recorded in the manifest, and recomputes logical state from it. A relocation
    that broke the tree would surface here as a refusal, not as a wrong number.
    """

    source, _root_db, task_db = _orc_fixture(tmp_path)
    archive = tmp_path / "archive"
    ingest_orc(archive, source, ORC_ROOT, "orc-test", "UTC")
    store = default_store_root(archive)
    legacy = archive / "teams" / "orc-test" / "source_snapshots"
    legacy.parent.mkdir(parents=True, exist_ok=True)
    shutil.move(str(store / "orc-test"), str(legacy))
    shutil.rmtree(store)
    (archive / SNAPSHOT_POINTER_FILE).unlink()
    before = _tree(legacy)
    assert any(name.startswith(".projections/") for name in before)
    assert any(name.startswith(".objects/") for name in before)

    result = migrate_snapshots(archive, move=True)

    assert result.moved == ("orc-test",)
    assert not legacy.exists()
    assert _tree(store / "orc-test") == before

    _append_task_note(task_db, "written after the move")
    _team, report = ingest_orc(archive, source, ORC_ROOT, "orc-test", "UTC")

    assert report.snapshot_root_layout == "external"
    assert (store / "orc-test" / ".objects").is_dir()


def test_the_move_refuses_rather_than_merging_into_an_occupied_target(tmp_path: Path) -> None:
    """Two trees for one team is a question with no defensible default answer."""

    _sessions, archive = _legacy_archive(tmp_path)
    occupied = default_store_root(archive) / "codex-test"
    occupied.mkdir(parents=True)
    (occupied / "someone-elses.jsonl").write_text("{}\n", encoding="utf-8")

    with pytest.raises(SnapshotLocationError, match="already holds snapshots"):
        migrate_snapshots(archive, move=True)

    assert (archive / "teams" / "codex-test" / "source_snapshots").is_dir()


def test_a_cross_filesystem_move_is_refused_until_it_is_asked_for_by_name(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A rename cannot be interrupted; a copy can, and while it runs the bytes exist twice.

    The device numbers are forced rather than a second filesystem mounted, because what is under
    test is the decision the planner makes from them, not the kernel's rename semantics.
    """

    _sessions, archive = _legacy_archive(tmp_path)
    real_stat = Path.stat

    def _stat(self: Path, **kwargs: bool) -> os.stat_result:
        info = real_stat(self, **kwargs)
        if "source_snapshots" in self.parts:
            return os.stat_result(
                (info.st_mode, info.st_ino, info.st_dev + 1) + tuple(info)[3:]
            )
        return info

    monkeypatch.setattr(Path, "stat", _stat)

    with pytest.raises(SnapshotLocationError, match="--copy"):
        migrate_snapshots(archive, move=True)

    assert (archive / "teams" / "codex-test" / "source_snapshots").is_dir()


def test_copy_without_move_is_refused_by_the_cli(tmp_path: Path) -> None:
    """`--copy` authorizes a destructive path; a dry run performs no path at all."""

    _sessions, archive = _legacy_archive(tmp_path)

    assert (
        timeline_main(["migrate-snapshots", "--output", str(archive), "--copy"]) == 2
    )


def test_the_cli_reports_then_moves(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    """The operator-facing path end to end, in the order an operator would use it."""

    _sessions, archive = _legacy_archive(tmp_path)

    assert timeline_main(["migrate-snapshots", "--output", str(archive)]) == 0
    dry = capsys.readouterr().out
    assert "would move" in dry and "Re-run with --move" in dry
    assert (archive / "teams" / "codex-test" / "source_snapshots").is_dir()

    assert timeline_main(["migrate-snapshots", "--output", str(archive), "--move"]) == 0
    wet = capsys.readouterr().out
    assert "moved 1 team(s)" in wet
    assert not (archive / "teams" / "codex-test" / "source_snapshots").exists()


# -- nothing under the snapshot root is reachable over HTTP -------------------------------------


def _fetch(root: Path, path: str) -> int:
    server = make_static_server(root, port=0)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        connection = http.client.HTTPConnection("127.0.0.1", server.server_address[1])
        try:
            connection.request("GET", path)
            return connection.getresponse().status
        finally:
            connection.close()
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def test_the_server_refuses_a_snapshot_even_when_the_archive_still_holds_one(
    tmp_path: Path,
) -> None:
    """The second half of the HTTP requirement, for the archive nobody has migrated.

    A migrated archive satisfies "unreachable" by construction -- the files are not under the
    served root -- which is exactly why it cannot be the only guarantee: it says nothing about the
    archives that exist today.
    """

    _sessions, archive = _legacy_archive(tmp_path)
    build_archive(archive, "codex-test")

    assert _fetch(archive, "/index.html") == 200
    assert (
        _fetch(
            archive,
            "/teams/codex-test/source_snapshots/2026/08/05/rollout-root.jsonl",
        )
        == 404
    )
    assert _fetch(archive, "/teams/codex-test/source_snapshots/") == 404
    # Spelled around the refusal rather than through it: the check is on the path the server
    # resolved, so an encoded traversal that lands in the same place is judged the same way.
    assert (
        _fetch(
            archive,
            "/teams/codex-test/raw/../source_snapshots/2026/08/05/rollout-root.jsonl",
        )
        == 404
    )


def test_a_migrated_archive_serves_nothing_from_the_store(tmp_path: Path) -> None:
    """Measured the way the requirement is worded: over the served tree, not over one path."""

    _sessions, archive = _legacy_archive(tmp_path)
    migrate_snapshots(archive, move=True)
    build_archive(archive, "codex-test")

    served = [
        path
        for path in archive.rglob("*")
        if path.is_file() and not path.is_symlink()
    ]
    assert served
    assert not any("source_snapshots" in path.parts for path in served)


# -- the rest of the archive follows the move ---------------------------------------------------


def test_the_losslessness_audit_follows_the_relocated_snapshots(tmp_path: Path) -> None:
    """The gate on any future *deletion* must not read a relocation as a deletion.

    An audit that only looked in the archive would report "already absent, nothing to compare",
    which is a green result whose meaning is the opposite of green.
    """

    _sessions, archive = _legacy_archive(tmp_path)
    before = audit_codex_losslessness(archive, "codex-test")
    assert before.covered and before.vendor_files == 1

    migrate_snapshots(archive, move=True)
    after = audit_codex_losslessness(archive, "codex-test")

    assert after.covered
    assert after.vendor_files == before.vendor_files
    assert after.vendor_rows == before.vendor_rows
    assert after.source_problems == ()


def test_gc_reports_the_snapshots_before_the_move_and_nothing_after_it(
    tmp_path: Path,
) -> None:
    """`gc` classifies the directory it was given; after a migration there is nothing to report.

    The zero is the correct answer rather than a blind spot, and it is asserted beside the
    non-zero so that the two together say what changed.
    """

    _sessions, archive = _legacy_archive(tmp_path)
    build_archive(archive, "codex-test")
    before = plan_collection(archive).category("ingest-source-snapshots")
    assert before.bytes > 0 and not before.reclaimable

    migrate_snapshots(archive, move=True)
    after = plan_collection(archive).category("ingest-source-snapshots")

    assert after.bytes == 0
    assert not after.reclaimable


def test_a_project_config_can_name_the_store(tmp_path: Path) -> None:
    """The non-interactive surface needs the setting too, and gets it once per archive."""

    sessions = tmp_path / "sessions"
    origin = _origin(sessions)
    origin.parent.mkdir(parents=True)
    origin.write_bytes(_root_bytes())
    config = tmp_path / "project.json"
    config.write_text(
        json.dumps(
            {
                "schema_version": 1,
                "output": "archive",
                "snapshot_root": str(tmp_path / "bulk"),
                "teams": [
                    {
                        "slug": "codex-test",
                        "provider": "codex",
                        "source": {
                            "sessions_root": str(sessions),
                            "root_session": CODEX_ROOT,
                        },
                    }
                ],
            }
        )
        + "\n",
        encoding="utf-8",
    )

    loaded = load_project_ingest_config(config)

    assert loaded.snapshot_root == tmp_path / "bulk"


def test_the_relocated_bytes_are_the_same_bytes(tmp_path: Path) -> None:
    """Digest equality over the whole tree, because "moved" is a claim about content."""

    _sessions, archive = _legacy_archive(tmp_path)
    legacy = archive / "teams" / "codex-test" / "source_snapshots"
    before = {
        name: hashlib.sha256(data).hexdigest() for name, data in _tree(legacy).items()
    }

    migrate_snapshots(archive, move=True)

    after = {
        name: hashlib.sha256(data).hexdigest()
        for name, data in _tree(default_store_root(archive) / "codex-test").items()
    }
    assert after == before


# -- the archive this migration exists for has twelve teams, not one -------------------------


def _legacy_multi_team_archive(tmp_path: Path, slugs: tuple[str, ...]) -> tuple[Path, Path]:
    """An unmigrated archive holding several teams, built the way the previous code built one.

    Every migration test before this one moved exactly one team, and the archive the command was
    written for holds twelve and 995 files. One team exercises no loop: not the per-team rename,
    not resuming after an interruption between two of them, not the aggregation of refusals.
    """

    sessions = tmp_path / "sessions"
    archive = tmp_path / "archive"
    origin = _origin(sessions)
    origin.parent.mkdir(parents=True)
    origin.write_bytes(_root_bytes())
    for slug in slugs:
        ingest_codex(archive, sessions, CODEX_ROOT, slug, "UTC")
    store = default_store_root(archive)
    for slug in slugs:
        legacy = archive / "teams" / slug / "source_snapshots"
        legacy.parent.mkdir(parents=True, exist_ok=True)
        shutil.move(str(store / slug), str(legacy))
    shutil.rmtree(store)
    (archive / SNAPSHOT_POINTER_FILE).unlink()
    return sessions, archive


def test_every_team_moves_and_an_interruption_between_two_of_them_resumes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The loop, and the state a `SIGKILL` between two of its iterations leaves.

    The interruption is placed where only a multi-team archive can put it: after the first
    ``os.replace`` and before the second. What must hold afterwards is that the archive is
    *partially* migrated rather than damaged -- the moved team ingests, the unmoved ones refuse
    by name rather than silently re-baselining, and re-running the command finishes the job.
    """

    slugs = ("codex-one", "codex-two", "codex-three")
    sessions, archive = _legacy_multi_team_archive(tmp_path, slugs)
    store = default_store_root(archive)
    before = {slug: _tree(archive / "teams" / slug / "source_snapshots") for slug in slugs}
    real_replace = os.replace
    calls: list[int] = []

    def _replace_once(source: object, target: object) -> None:
        # Only the team moves count: the pointer and the marker are written durably, which is
        # also an `os.replace`, and interrupting one of those would be testing a different thing.
        if "source_snapshots" not in str(source):
            real_replace(source, target)  # type: ignore[arg-type]
            return
        calls.append(1)
        if len(calls) > 1:
            raise KeyboardInterrupt("power cut between two teams")
        real_replace(source, target)  # type: ignore[arg-type]

    monkeypatch.setattr(os, "replace", _replace_once)
    with pytest.raises(KeyboardInterrupt):
        migrate_snapshots(archive, move=True)
    monkeypatch.undo()

    assert _tree(store / "codex-one") == before["codex-one"]
    assert (archive / "teams" / "codex-two" / "source_snapshots").is_dir()
    # The moved team keeps working; the ones still inside say what is wrong and how to finish.
    assert resolve_snapshot_root(archive, "codex-one").root == store / "codex-one"
    with pytest.raises(SnapshotLocationError, match="interrupted migration"):
        resolve_snapshot_root(archive, "codex-two")

    result = migrate_snapshots(archive, move=True)

    assert sorted(result.moved) == ["codex-three", "codex-two"]
    for slug in slugs:
        assert _tree(store / slug) == before[slug]
        assert not (archive / "teams" / slug / "source_snapshots").exists()
        assert resolve_snapshot_root(archive, slug).root == store / slug
    _team, report = ingest_codex(archive, sessions, CODEX_ROOT, "codex-two", "UTC")
    assert report.snapshot_root_layout == "external"


def test_a_requested_root_is_refused_while_any_other_team_is_still_inside(
    tmp_path: Path,
) -> None:
    """One archive, one layout -- and that is a fact about the archive, not about one team.

    Asking only whether *this* team's directory was populated let a brand-new team be given
    ``--snapshot-root`` while every existing team's snapshots were still in the archive. The run
    succeeded and then recorded ``external`` in the archive-wide pointer, after which every
    previously ingested team hit the interrupted-migration refusal and was told to finish a
    migration nobody had started. On a twelve-team archive that is eleven teams bricked by one
    flag on a twelfth.
    """

    sessions, archive = _legacy_multi_team_archive(tmp_path, ("codex-one",))

    with pytest.raises(SnapshotLocationError, match="split this archive across two layouts"):
        ingest_codex(
            archive,
            sessions,
            CODEX_ROOT,
            "codex-two",
            "UTC",
            snapshot_root=tmp_path / "bulk",
        )

    # Nothing was recorded and nothing was created, so the archive is exactly as it was.
    assert not (archive / SNAPSHOT_POINTER_FILE).exists()
    assert not (tmp_path / "bulk").exists()
    assert resolve_snapshot_root(archive, "codex-one").layout == "in-archive-legacy"
    _team, report = ingest_codex(archive, sessions, CODEX_ROOT, "codex-one", "UTC")
    assert report.snapshot_root_layout == "in-archive-legacy"


def test_an_interrupted_copy_is_finished_by_re_running_the_command(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The interruptible path, interrupted -- and then simply run again.

    Copying straight into ``<store>/<slug>`` made this unrecoverable: the partial target made the
    planner refuse ("already holds snapshots; merging two trees is not something this command
    will guess at") while the pointer made every ingest refuse and point back at the planner. The
    two commands pointed at each other and neither named the partial tree as the thing to remove,
    and the plausible guess out of that -- delete the source, because the pointer says the store
    is authoritative and the copy looks finished -- is the only copy of the vendor logs.
    """

    _sessions, archive = _legacy_multi_team_archive(tmp_path, ("codex-one",))
    store = default_store_root(archive)
    legacy = archive / "teams" / "codex-one" / "source_snapshots"
    before = _tree(legacy)
    real_stat = Path.stat

    def _stat(self: Path, **kwargs: bool) -> os.stat_result:
        info = real_stat(self, **kwargs)
        if "source_snapshots" in self.parts:
            return os.stat_result(
                (info.st_mode, info.st_ino, info.st_dev + 1) + tuple(info)[3:]
            )
        return info

    monkeypatch.setattr(Path, "stat", _stat)
    real_copytree = shutil.copytree

    def _copytree_stops_halfway(source: object, target: object, **kwargs: object) -> None:
        del source, kwargs
        # Half a tree and then nothing, which is what ENOSPC or a signal actually leaves. Written
        # directly rather than by interrupting the real copy, because `shutil.copytree` recurses
        # through the module global and a wrapper would be re-entered with its internal signature.
        partial = Path(str(target))
        partial.mkdir(parents=True)
        (partial / "half-a-copy.jsonl").write_text("{}\n", encoding="utf-8")
        raise KeyboardInterrupt("out of space, or a power cut")

    monkeypatch.setattr(shutil, "copytree", _copytree_stops_halfway)
    with pytest.raises(KeyboardInterrupt):
        migrate_snapshots(archive, move=True, copy_across_devices=True)
    monkeypatch.setattr(shutil, "copytree", real_copytree)

    # The half-done copy is under the command's own scratch name, never at the team's path.
    assert not (store / "codex-one").exists()
    assert (store / ".migrating" / "codex-one").is_dir()
    assert _tree(legacy) == before

    result = migrate_snapshots(archive, move=True, copy_across_devices=True)

    assert result.copied == ("codex-one",)
    assert _tree(store / "codex-one") == before
    assert not legacy.exists()
    assert not (store / ".migrating" / "codex-one").exists()


# -- the archive directory gets renamed ------------------------------------------------------


def test_renaming_the_archive_does_not_read_as_two_archives_sharing_a_store(
    tmp_path: Path,
) -> None:
    """Renaming ``summary/widget`` was completely safe while the snapshots were inside it.

    The store's marker records the archive as a store-relative path, so a rename makes it
    disagree -- and the disagreement used to be reported as the one accident the marker exists to
    catch, two archives fighting over one store, with a remedy (``--snapshot-root``) that starts
    an empty second tree and then fails again on the append guard. There is no second archive
    here, and the marker naming a path where no archive is any more is the proof of that.
    """

    sessions, archive, _origin_path = _first_ingest(tmp_path)
    renamed = tmp_path / "archive-renamed"
    archive.rename(renamed)

    location = resolve_snapshot_root(renamed, "codex-test")

    assert location.store_root == default_store_root(archive)
    _team, report = ingest_codex(renamed, sessions, CODEX_ROOT, "codex-test", "UTC")
    assert report.snapshot_root_layout == "external"
    marker = json.loads(
        (default_store_root(archive) / ".snapshot-store.json").read_text(encoding="utf-8")
    )
    assert marker["archive"] == "../archive-renamed"


def test_a_second_archive_pointed_at_an_occupied_store_is_still_refused(
    tmp_path: Path,
) -> None:
    """The weakening above must not cost the check its actual job.

    A marker naming an archive that is *still there* is the collision it was written for: two
    archives sharing a store agree on nothing and overwrite each other the moment they share a
    team slug.
    """

    sessions, archive, _origin_path = _first_ingest(tmp_path)
    second = tmp_path / "second-archive"
    second.mkdir()

    with pytest.raises(SnapshotLocationError, match="still there"):
        ingest_codex(
            second,
            sessions,
            CODEX_ROOT,
            "codex-test",
            "UTC",
            snapshot_root=default_store_root(archive),
        )


def test_moving_the_archive_and_its_store_together_finds_the_store_and_never_fabricates_one(
    tmp_path: Path,
) -> None:
    """The obvious "keep the pair together" action, which used to invent a third directory.

    The pointer is archive-relative, so renaming the archive alone leaves it resolving; renaming
    both leaves it resolving to a name that is now nobody's. What happened then was worse than a
    refusal: ``ensure_snapshot_store`` *created* the recorded path, empty, in the parent of
    ``--output``, and the ingest failed several layers later complaining that a previously
    observed rollout had disappeared -- naming a directory the tool had just fabricated while the
    real store sat beside it under its new name. The store identifies itself with a marker, so
    adopting the sibling is a reading rather than a guess.
    """

    sessions, archive, _origin_path = _first_ingest(tmp_path)
    store = default_store_root(archive)
    renamed = tmp_path / "archive-renamed"
    archive.rename(renamed)
    store.rename(default_store_root(renamed))

    location = resolve_snapshot_root(renamed, "codex-test")

    assert location.store_root == default_store_root(renamed)
    assert not store.exists()
    _team, report = ingest_codex(renamed, sessions, CODEX_ROOT, "codex-test", "UTC")
    assert report.snapshot_root_layout == "external"
    assert not store.exists()


def test_a_recorded_store_that_is_simply_gone_refuses_instead_of_starting_a_new_one(
    tmp_path: Path,
) -> None:
    """No sibling to adopt, and teams that have been ingested: the run has to stop.

    Creating the store from the pointer alone would silently re-baseline every team's append-only
    guard against an empty tree, which is the failure mode that has no error message at all.
    """

    _sessions, archive, _origin_path = _first_ingest(tmp_path)
    shutil.rmtree(default_store_root(archive))

    with pytest.raises(SnapshotLocationError, match="there is no such directory"):
        resolve_snapshot_root(archive, "codex-test")

    assert not default_store_root(archive).exists()
