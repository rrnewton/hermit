"""Fixed-budget progress must terminate at the lifecycle-subject boundary."""

from __future__ import annotations

import json
import mmap
import os
import time
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path

import pytest

from wrkslots import cli


@dataclass(frozen=True)
class CensusFixture:
    config: cli.Config
    planned: dict[str, tuple[cli.CacheDirectory, ...]]
    state_path: Path

    def run(
        self, work_limit: int
    ) -> tuple[dict[str, cli._AuditCacheMeasurement], dict[str, int]]:
        measured, counters = cli._audit_cache_census(
            self.config,
            (cli.ActiveState("testhost", 1, ()),),
            self.planned,
            {},
            state_path=self.state_path,
            work_limit=work_limit,
            wall_seconds=5,
        )
        assert counters["work_limit"] == work_limit
        assert 0 <= counters["work_consumed"] <= work_limit
        assert counters["work_remaining"] == work_limit - counters["work_consumed"]
        for measurement in measured.values():
            if measurement.status != "complete":
                assert measurement.bytes is None
        return measured, counters

    def expected_bytes(self, subject: str) -> int:
        # An independent filesystem enumeration, not the census or old allocator.
        return sum(
            path.lstat().st_blocks * 512
            for cache in self.planned[subject]
            for path in (cache.path, *cache.path.rglob("*"))
        )

    def stored_roots(self) -> tuple[Mapping[str, object], ...]:
        state = cli._as_mapping(
            json.loads(self.state_path.read_text(encoding="utf-8")), "test state"
        )
        roots = cli._as_mapping(state["roots"], "test roots")
        return tuple(cli._as_mapping(root, "test root") for root in roots.values())


def _fixture(tmp_path: Path, sizes: Mapping[str, Sequence[int]]) -> CensusFixture:
    project = tmp_path / "project"
    project.mkdir()
    worktrees = project / "worktrees"
    worktrees.mkdir()
    config = cli.Config(
        root=project,
        config_path=project / ".wrkslots.yml",
        worktrees=worktrees,
        control=worktrees,
        machine="testhost",
        default_remote="origin",
        default_landed_ref="refs/remotes/origin/main",
        heartbeat_ttl_seconds=3600,
        liveness_command=project / "unused-liveness",
    )
    planned: dict[str, tuple[cli.CacheDirectory, ...]] = {}
    for subject, root_sizes in sizes.items():
        directories: list[cli.CacheDirectory] = []
        for index, files in enumerate(root_sizes):
            checkout = project / f"{subject}-{index}"
            cache = checkout / "target"
            cache.mkdir(parents=True)
            for entry in range(files):
                (cache / f"artifact-{entry:06d}").touch()
            identity = cli._open_directory_identity(checkout, "test checkout")
            directories.append(cli.CacheDirectory(cache, checkout, *identity))
        planned[subject] = tuple(directories)
    return CensusFixture(config, planned, tmp_path / "census.json")


def _stage_three_roots(fixture: CensusFixture) -> None:
    """Reach genuine staged progress using only the unchanged ten-unit budget."""

    for _ in range(20):
        measured, _counters = fixture.run(10)
        roots = fixture.stored_roots()
        if len(roots) == 3 and all(
            root["phase"] == "finalize" and root["status"] == "partial"
            for root in roots
        ):
            assert measured["subject"].status == "partial"
            return
    pytest.fail("three stable roots did not reach subject-level staged progress")


@pytest.mark.parametrize(("files", "work_limit"), ((9, 10), (1024, 1025)))
def test_exact_final_sweep_cost_completes_without_raising_fixed_budget(
    tmp_path: Path, files: int, work_limit: int
) -> None:
    fixture = _fixture(tmp_path, {"subject": (files,)})
    for _ in range(10):
        measured, counters = fixture.run(work_limit)
        assert measured["subject"].status != "error"
        if measured["subject"].status == "complete":
            assert measured["subject"].bytes == fixture.expected_bytes("subject")
            assert counters["entries_finalized"] == files
            assert counters["directories_finalized"] == 1
            assert counters["work_consumed"] == work_limit
            break
    else:
        pytest.fail("stable cache never completed at its exact fixed final-sweep cost")


def test_default_budget_large_root_has_finite_explicit_terminal_outcome(
    tmp_path: Path,
) -> None:
    # Keep the production default pinned independently from the boundary
    # algorithm. A smaller set of real regular files exercises the identical
    # N entries plus one root observation without manufacturing 100,000 inodes.
    assert cli._AUDIT_CACHE_WORK_LIMIT == 100000
    work_limit = 64
    fixture = _fixture(tmp_path, {"subject": (work_limit,)})
    cache = fixture.planned["subject"][0].path
    assert sum(1 for _ in cache.iterdir()) == work_limit
    for _ in range(12):
        measured, _counters = fixture.run(work_limit)
        assert measured["subject"].status != "complete"
        if measured["subject"].status == "error":
            assert measured["subject"].bytes is None
            detail = measured["subject"].error
            assert detail is not None
            assert f"{work_limit + 1} work units" in detail
            assert f"fixed allowance of {work_limit}" in detail
            break
    else:
        pytest.fail("default-budget large root remained partial without a terminal outcome")


@pytest.mark.parametrize(("root_count", "work_limit"), ((3, 10), (6, 12)))
def test_multiple_roots_repeatedly_complete_as_one_subject(
    tmp_path: Path, root_count: int, work_limit: int
) -> None:
    fixture = _fixture(tmp_path, {"subject": (1,) * root_count})
    completions = 0
    for _ in range(36):
        measured, counters = fixture.run(work_limit)
        assert measured["subject"].status != "error"
        if measured["subject"].status == "complete":
            completions += 1
            assert measured["subject"].bytes == fixture.expected_bytes("subject")
            assert counters["directories_finalized"] == root_count
            assert counters["entries_finalized"] == root_count
            if completions >= 4:
                break
    else:
        pytest.fail("stable subject fell into a post-completion partial cycle")
    assert completions >= 4, "stable subject fell into a post-completion partial cycle"


def test_subject_cursor_is_fair_with_different_root_counts_and_oversized_peer(
    tmp_path: Path,
) -> None:
    fixture = _fixture(
        tmp_path,
        {
            "small": (1,),
            "medium": (1, 1),
            "exact": (1, 1, 1),
            "oversized": (1, 1, 1, 1),
        },
    )
    completions = {subject: 0 for subject in ("small", "medium", "exact")}
    terminal_errors = 0
    for _ in range(80):
        measured, _counters = fixture.run(6)
        for subject in completions:
            assert measured[subject].status != "error"
            if measured[subject].status == "complete":
                assert measured[subject].bytes == fixture.expected_bytes(subject)
                completions[subject] += 1
        oversized = measured["oversized"]
        assert oversized.status != "complete"
        if oversized.status == "error":
            assert oversized.error is not None
            assert "8 work units" in oversized.error
            terminal_errors += 1
        if terminal_errors >= 3 and all(count >= 3 for count in completions.values()):
            break
    else:
        pytest.fail(
            f"subject cursor did not meet bounded fairness: "
            f"completions={completions}, terminal_errors={terminal_errors}"
        )
    assert all(count >= 3 for count in completions.values()), completions
    assert terminal_errors >= 3


@pytest.mark.parametrize("root_count", (1, 3, 10))
def test_empty_roots_complete_on_the_last_available_directory_observation(
    tmp_path: Path, root_count: int
) -> None:
    fixture = _fixture(tmp_path, {"subject": (0,) * root_count})
    for _ in range(30):
        measured, counters = fixture.run(root_count)
        assert measured["subject"].status != "error"
        if measured["subject"].status == "complete":
            assert measured["subject"].bytes == fixture.expected_bytes("subject")
            assert counters["work_consumed"] == root_count
            assert counters["directories_finalized"] == root_count
            assert counters["entries_finalized"] == 0
            break
    else:
        pytest.fail("empty roots required an extra allowance after their final observation")


def test_mutated_early_staged_root_cannot_supply_stale_subject_bytes(
    tmp_path: Path,
) -> None:
    fixture = _fixture(tmp_path, {"subject": (1, 1, 1)})
    _stage_three_roots(fixture)
    cache = fixture.planned["subject"][0].path
    before = cache.stat()
    old_total = fixture.expected_bytes("subject")
    (cache / "artifact-000000").write_bytes(b"x" * 65536)
    after = cache.stat()
    assert (before.st_ino, before.st_mtime_ns, before.st_ctime_ns) == (
        after.st_ino,
        after.st_mtime_ns,
        after.st_ctime_ns,
    )
    assert fixture.expected_bytes("subject") > old_total

    measured, _counters = fixture.run(10)

    assert measured["subject"].status == "error"
    assert measured["subject"].bytes is None
    assert measured["subject"].error is not None
    assert "entry changed before final publication" in measured["subject"].error


class FinalStatClock:
    """Keep synthetic time and observed paths visible across census workers.

    Only one process writes at a time, but the waiting parent may read time.
    Two slots publish complete snapshots by generation so a killed writer
    cannot leave an incomplete active record or hold a synchronization lock.
    The anonymous mapping also survives descriptor cleanup in the worker.
    """

    def __init__(
        self, now: float = 0, finalizing: bool = False, deadline: float = 5,
        expire_at: int | None = None, observed: list[Path] | None = None,
    ) -> None:
        self._shared = mmap.mmap(-1, 64 * 1024)
        self._write(
            {
                "now": now, "finalizing": finalizing, "deadline": deadline,
                "expire_at": expire_at,
                "observed": None if observed is None else [str(path) for path in observed],
            }
        )

    def _read(self) -> dict[str, object]:
        slot_size = (len(self._shared) - 8) // 2
        for _ in range(128):
            generation = self._shared[:8]
            offset = 8 + (int.from_bytes(generation, "big") % 2) * slot_size
            size = int.from_bytes(self._shared[offset:offset + 4], "big")
            encoded = self._shared[offset + 4:offset + 4 + size]
            if generation == self._shared[:8]:
                return dict(cli._as_mapping(json.loads(encoded), "test clock"))
        raise AssertionError("shared test clock changed during every snapshot attempt")

    def _write(self, value: Mapping[str, object]) -> None:
        encoded = json.dumps(value, separators=(",", ":")).encode("utf-8")
        slot_size = (len(self._shared) - 8) // 2
        assert len(encoded) <= slot_size - 4
        generation = int.from_bytes(self._shared[:8], "big") + 1
        offset = 8 + (generation % 2) * slot_size
        self._shared[offset + 4:offset + 4 + len(encoded)] = encoded
        self._shared[offset:offset + 4] = len(encoded).to_bytes(4, "big")
        self._shared[:8] = generation.to_bytes(8, "big")

    def _set(self, name: str, value: object) -> None:
        record = self._read()
        record[name] = value
        self._write(record)

    @property
    def now(self) -> float:
        value = self._read()["now"]
        assert isinstance(value, (int, float))
        return float(value)

    @now.setter
    def now(self, value: float) -> None:
        self._set("now", value)

    @property
    def finalizing(self) -> bool:
        value = self._read()["finalizing"]
        assert isinstance(value, bool)
        return value

    @finalizing.setter
    def finalizing(self, value: bool) -> None:
        self._set("finalizing", value)

    @property
    def deadline(self) -> float:
        value = self._read()["deadline"]
        assert isinstance(value, (int, float))
        return float(value)

    @deadline.setter
    def deadline(self, value: float) -> None:
        self._set("deadline", value)

    @property
    def expire_at(self) -> int | None:
        value = self._read()["expire_at"]
        assert value is None or isinstance(value, int)
        return value

    @expire_at.setter
    def expire_at(self, value: int | None) -> None:
        self._set("expire_at", value)

    @property
    def observed(self) -> list[Path] | None:
        value = self._read()["observed"]
        if value is None:
            return None
        return [
            Path(cli._as_str(path, "test clock observed path"))
            for path in cli._as_list(value, "test clock observed paths")
        ]

    @observed.setter
    def observed(self, value: list[Path] | None) -> None:
        self._set("observed", None if value is None else [str(path) for path in value])

    def record_observation(self, path: Path) -> None:
        observed = self.observed
        assert observed is not None
        observed.append(path)
        self.observed = observed

    def reset(self, expire_at: int | None) -> None:
        self.now = 0
        self.expire_at = expire_at
        self.observed = []

    def monotonic(self) -> float:
        return self.now


def _install_final_stat_clock(monkeypatch: pytest.MonkeyPatch) -> FinalStatClock:
    clock = FinalStatClock(observed=[])
    original_resume = cli._resume_audit_cache_root
    original_stat = os.stat

    def resume(
        cache: cli.CacheDirectory,
        root_identity: Sequence[int],
        root: dict[str, object],
        *,
        budget: cli._AuditWorkBudget,
        deadline: float,
        finalize: bool = True,
    ) -> bool:
        clock.finalizing = finalize and root["phase"] == "finalize"
        clock.deadline = deadline
        try:
            return original_resume(
                cache,
                root_identity,
                root,
                budget=budget,
                deadline=deadline,
                finalize=finalize,
            )
        finally:
            clock.finalizing = False

    def observe_stat(
        path: int | str | bytes | os.PathLike[str] | os.PathLike[bytes],
        *,
        dir_fd: int | None = None,
        follow_symlinks: bool = True,
    ) -> os.stat_result:
        result = original_stat(path, dir_fd=dir_fd, follow_symlinks=follow_symlinks)
        if (
            clock.finalizing
            and dir_fd is not None
            and isinstance(path, str)
            and path.startswith("artifact-")
        ):
            assert follow_symlinks is False
            assert clock.observed is not None
            clock.record_observation(Path(os.readlink(f"/proc/self/fd/{dir_fd}")) / path)
            if len(clock.observed) == clock.expire_at:
                # Advance after the real stat returns, catching a missing final
                # deadline check even when this was the last observation.
                clock.now = clock.deadline + 1
        return result

    monkeypatch.setattr(cli, "_resume_audit_cache_root", resume)
    monkeypatch.setattr(os, "stat", observe_stat)
    monkeypatch.setattr(time, "monotonic", clock.monotonic)
    return clock


@pytest.mark.parametrize("expire_at", (2, 3))
def test_full_finalization_wall_expiry_is_terminal_and_retry_is_fresh(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, expire_at: int
) -> None:
    fixture = _fixture(tmp_path, {"subject": (1, 1, 1)})
    _stage_three_roots(fixture)
    clock = _install_final_stat_clock(monkeypatch)
    clock.reset(expire_at)

    measured, counters = fixture.run(10)

    assert measured["subject"].status == "error"
    assert measured["subject"].error is not None
    assert "fixed wall allowance of 5 seconds" in measured["subject"].error
    assert counters["entries_finalized"] == expire_at
    assert counters["directories_finalized"] == expire_at
    assert counters["work_consumed"] == expire_at * 2
    assert all(root["status"] == "partial" for root in fixture.stored_roots())

    clock.reset(None)
    retried, counters = fixture.run(10)

    assert retried["subject"].status == "complete"
    assert retried["subject"].bytes == fixture.expected_bytes("subject")
    assert counters["entries_verified"] == 0
    assert counters["directories_verified"] == 0
    assert counters["entries_finalized"] == 3
    assert counters["directories_finalized"] == 3
    assert clock.observed is not None
    assert set(clock.observed) == {
        cache.path / "artifact-000000" for cache in fixture.planned["subject"]
    }


@pytest.mark.parametrize("mutate_before_retry", (False, True))
def test_interrupted_opportunistic_sweep_retains_staging_but_no_completion(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    mutate_before_retry: bool,
) -> None:
    fixture = _fixture(tmp_path, {"subject": (1, 1, 1)})
    clock = _install_final_stat_clock(monkeypatch)
    clock.reset(2)

    measured, counters = fixture.run(100)

    assert measured["subject"].status == "partial"
    assert counters["entries_finalized"] == 2
    assert counters["directories_finalized"] == 2
    assert all(
        root["phase"] == "finalize" and root["status"] == "partial"
        for root in fixture.stored_roots()
    )
    if mutate_before_retry:
        # This file already passed the interrupted sweep. Its old success must
        # provide no authority when the remaining roots get their next turn.
        assert clock.observed is not None
        clock.observed[0].write_bytes(b"x" * 65536)
    clock.reset(None)

    retried, counters = fixture.run(100)

    assert counters["entries_verified"] == 0
    assert counters["directories_verified"] == 0
    if mutate_before_retry:
        assert retried["subject"].status == "error"
        assert retried["subject"].bytes is None
        assert retried["subject"].error is not None
        assert "entry changed before final publication" in retried["subject"].error
    else:
        assert retried["subject"].status == "complete"
        assert retried["subject"].bytes == fixture.expected_bytes("subject")
        assert counters["work_consumed"] == 6
        assert counters["entries_finalized"] == 3
        assert counters["directories_finalized"] == 3


def test_root_binding_remains_inside_the_unchanged_wall_allowance(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fixture = _fixture(tmp_path, {"subject": (1, 1, 1), "empty": ()})
    original_identity = cli._audit_cache_root_identity
    now = 0.0
    bindings = 0

    def slow_identity(
        config: cli.Config, cache: cli.CacheDirectory, *, deadline: float | None = None
    ) -> tuple[int, int, int, int, int] | None:
        nonlocal now, bindings
        identity = original_identity(config, cache, deadline=deadline)
        now += 2.0
        bindings += 1
        return identity

    monkeypatch.setattr(time, "monotonic", lambda: now)
    monkeypatch.setattr(cli, "_audit_cache_root_identity", slow_identity)

    for _ in range(2):
        measured, counters = fixture.run(100000)
        assert measured["subject"].status == "error"
        assert measured["subject"].bytes is None
        assert "root binding exhausted the fixed wall allowance of 5 seconds" in (
            measured["subject"].error or ""
        )
        assert counters["work_consumed"] == 0
        assert counters["directories_visited"] == 0
        assert measured["empty"].status == "complete"
        assert measured["empty"].bytes == 0
    assert bindings == 6
