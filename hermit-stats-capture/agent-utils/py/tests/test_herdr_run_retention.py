"""Retention tests, bracketed on BOTH halves.

A prune that deletes everything passes any "was it deleted?" test while being a data-loss bug, so
every case here asserts what SURVIVED as well as what went. The scope tests matter just as much:
this is the one module that removes directories.
"""

from __future__ import annotations

import os
import stat
import time

from herdr_run.retention import MAX_RETENTION_DAYS, RETENTION_DAYS, prune_runs, runs_root

DAY = 86_400


def _plant(root: str, name: str, *, age_days: float) -> str:
    """Create a run directory with realistic contents, backdated by ``age_days``."""
    path = os.path.join(root, name)
    os.makedirs(path, exist_ok=True)
    for leaf in ("stdout", "stderr", "exit_code", "command"):
        with open(os.path.join(path, leaf), "w", encoding="utf-8") as handle:
            handle.write("0\n" if leaf == "exit_code" else f"{leaf} of {name}\n")
    stamp = time.time() - age_days * DAY
    os.utime(os.path.join(path, "exit_code"), (stamp, stamp))
    os.utime(path, (stamp, stamp))
    return path


# --- the owner's planting test: old GONE, recent SURVIVES ----------------------------------------


def test_five_day_run_is_pruned_and_one_day_run_survives(tmp_path: object) -> None:
    root = os.path.join(str(tmp_path), "runs")
    os.makedirs(root)
    old = _plant(root, "20260801T000000-agent-1", age_days=5)
    new = _plant(root, "20260805T000000-agent-2", age_days=1)

    result = prune_runs(root)

    assert not os.path.exists(old), "5-day-old run should have been pruned"
    assert os.path.isdir(new), "1-day-old run must SURVIVE -- this is the data-loss control"
    assert result.removed == (old,)
    assert result.kept == 1


def test_boundary_just_inside_and_just_outside_the_window(tmp_path: object) -> None:
    """The window is a real boundary, not 'delete roughly the old ones'."""
    root = os.path.join(str(tmp_path), "runs")
    os.makedirs(root)
    inside = _plant(root, "inside", age_days=RETENTION_DAYS - 0.1)
    outside = _plant(root, "outside", age_days=RETENTION_DAYS + 0.1)

    prune_runs(root)

    assert os.path.isdir(inside)
    assert not os.path.exists(outside)


def test_nothing_is_removed_when_everything_is_fresh(tmp_path: object) -> None:
    """Positive control: the prune must be capable of doing nothing."""
    root = os.path.join(str(tmp_path), "runs")
    os.makedirs(root)
    for i in range(4):
        _plant(root, f"fresh-{i}", age_days=i * 0.5)

    result = prune_runs(root)

    assert result.removed == ()
    assert result.kept == 4
    assert len(os.listdir(root)) == 4


def test_old_incomplete_run_survives_while_old_completed_run_is_removed(
    tmp_path: object,
) -> None:
    root = os.path.join(str(tmp_path), "runs")
    os.makedirs(root)
    completed = _plant(root, "completed", age_days=30)
    active = _plant(root, "active", age_days=30)
    os.unlink(os.path.join(active, "exit_code"))

    result = prune_runs(root)

    assert not os.path.exists(completed)
    assert os.path.isdir(active), "a concurrent or timed-out active run must never be pruned"
    assert result.removed == (completed,)
    assert result.kept == 1


def test_invalid_retention_windows_fail_closed(tmp_path: object) -> None:
    root = os.path.join(str(tmp_path), "runs")
    os.makedirs(root)
    old = _plant(root, "old", age_days=30)

    for invalid in (-1, MAX_RETENTION_DAYS + 1, 10**400):
        result = prune_runs(root, retention_days=invalid)
        assert result.removed == ()
        assert os.path.isdir(old)


def test_fifo_completion_marker_cannot_block_retention(tmp_path: object) -> None:
    root = os.path.join(str(tmp_path), "runs")
    os.makedirs(root)
    run = _plant(root, "corrupt", age_days=30)
    marker = os.path.join(run, "exit_code")
    os.unlink(marker)
    os.mkfifo(marker)

    result = prune_runs(root)

    assert os.path.isdir(run)
    assert stat.S_ISFIFO(os.lstat(marker).st_mode)
    assert result.kept == 1


# --- scope: this module deletes directories, so containment is asserted, not assumed --------------


def test_symlinks_are_skipped_never_followed(tmp_path: object) -> None:
    """A symlink planted in the spool must not become a path out of it."""
    base = str(tmp_path)
    root = os.path.join(base, "runs")
    os.makedirs(root)
    outside = os.path.join(base, "PRECIOUS")
    os.makedirs(outside)
    with open(os.path.join(outside, "keep.txt"), "w", encoding="utf-8") as handle:
        handle.write("must survive\n")

    link = os.path.join(root, "escape")
    os.symlink(outside, link)
    stamp = time.time() - 30 * DAY
    os.utime(link, (stamp, stamp), follow_symlinks=False)

    result = prune_runs(root)

    assert os.path.isdir(outside), "target of a symlink must never be removed"
    assert os.path.isfile(os.path.join(outside, "keep.txt"))
    assert os.path.islink(link), "the symlink itself is skipped, not deleted"
    assert link in result.skipped


def test_symlinked_runs_root_cannot_redefine_deletion_scope(tmp_path: object) -> None:
    """The configured root itself is untrusted; containment cannot follow it before checking."""
    base = str(tmp_path)
    victim = os.path.join(base, "victim")
    old = _plant(victim, "unrelated-old-directory", age_days=30)
    linked_root = os.path.join(base, "project", ".herdr-run", "runs")
    os.makedirs(os.path.dirname(linked_root))
    os.symlink(victim, linked_root)

    result = prune_runs(linked_root)

    assert result.removed == ()
    assert os.path.abspath(linked_root) in result.skipped
    assert os.path.isdir(old)


def test_the_runs_root_itself_is_never_removed(tmp_path: object) -> None:
    root = os.path.join(str(tmp_path), "runs")
    os.makedirs(root)
    stamp = time.time() - 99 * DAY
    os.utime(root, (stamp, stamp))

    prune_runs(root)

    assert os.path.isdir(root)


def test_siblings_of_the_runs_root_are_untouched(tmp_path: object) -> None:
    """Nothing above the capture root is reachable: the audit log lives next to runs/."""
    base = os.path.join(str(tmp_path), "spool")
    root = os.path.join(base, "runs")
    os.makedirs(root)
    audit = os.path.join(base, "audit.jsonl")
    with open(audit, "w", encoding="utf-8") as handle:
        handle.write('{"verdict":"RAN"}\n')
    stamp = time.time() - 60 * DAY
    os.utime(audit, (stamp, stamp))
    _plant(root, "ancient", age_days=60)

    prune_runs(root)

    assert os.path.isfile(audit), "the audit trail is evidence and is never pruned"
    with open(audit, encoding="utf-8") as handle:
        assert handle.read().strip() == '{"verdict":"RAN"}'


def test_loose_files_in_the_runs_root_are_skipped(tmp_path: object) -> None:
    root = os.path.join(str(tmp_path), "runs")
    os.makedirs(root)
    stray = os.path.join(root, "stray.txt")
    with open(stray, "w", encoding="utf-8") as handle:
        handle.write("x")
    stamp = time.time() - 60 * DAY
    os.utime(stray, (stamp, stamp))

    result = prune_runs(root)

    assert os.path.isfile(stray)
    assert stray in result.skipped


# --- never raises --------------------------------------------------------------------------------


def test_missing_root_is_not_an_error(tmp_path: object) -> None:
    result = prune_runs(os.path.join(str(tmp_path), "does-not-exist"))
    assert result.removed == ()
    assert result.kept == 0


def test_runs_root_resolution() -> None:
    assert runs_root(".herdr-run", "/proj") == "/proj/.herdr-run/runs"
    assert runs_root("/abs/spool", "/proj") == "/abs/spool/runs"
