"""Traversal syscalls and worker results obey the shared census deadline."""

from __future__ import annotations

import json
import mmap
import os
import time
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path

import pytest

from wrkslots import cli
from wrkslots.tests.test_census_binding_deadline import _assert_reaped, _child_pids
from wrkslots.tests.test_census_subject_progress import CensusFixture, FinalStatClock, _fixture


@dataclass
class TraversalObservation:
    active: bool = False
    deadline: float = 0
    budget: cli._AuditWorkBudget | None = None


def _observe_traversal(monkeypatch: pytest.MonkeyPatch) -> TraversalObservation:
    observation = TraversalObservation()
    original = cli._resume_audit_cache_root_unbounded

    def tagged(
        cache: cli.CacheDirectory, identity: Sequence[int], root: dict[str, object],
        *, budget: cli._AuditWorkBudget, deadline: float, finalize: bool = True,
    ) -> bool:
        observation.active = True
        observation.deadline = deadline
        observation.budget = budget
        try:
            return original(
                cache, identity, root, budget=budget, deadline=deadline, finalize=finalize
            )
        finally:
            observation.active = False

    monkeypatch.setattr(cli, "_resume_audit_cache_root_unbounded", tagged)
    return observation


def _measure(
    fixture: CensusFixture, *, wall_seconds: float = 5,
) -> tuple[dict[str, cli._AuditCacheMeasurement], dict[str, int]]:
    return cli._audit_cache_census(
        fixture.config,
        (cli.ActiveState("testhost", 1, ()),),
        fixture.planned,
        {},
        state_path=fixture.state_path,
        work_limit=100000,
        wall_seconds=wall_seconds,
    )


def _append_event(path: Path, event: dict[str, object]) -> None:
    # The child closes inherited descriptors. Open this disposable observer
    # afresh and flush it before the injected blocking operation.
    with path.open("a", encoding="utf-8") as output:
        output.write(json.dumps(event) + "\n")


def _events(path: Path) -> list[dict[str, object]]:
    return [
        dict(cli._as_mapping(json.loads(line), "test operation"))
        for line in path.read_text().splitlines()
    ]


def test_default_budget_blocked_traversal_terminates_on_every_attempt_and_retries(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    fixture = _fixture(tmp_path, {"subject": (1,)})
    (fixture.planned["subject"][0].path / "artifact-000000").write_bytes(b"allocated")
    assert cli._AUDIT_CACHE_WORK_LIMIT == 100000
    assert cli._AUDIT_CACHE_WALL_SECONDS == 5
    observation = _observe_traversal(monkeypatch)
    children = _child_pids(monkeypatch)
    log = tmp_path / "traversal-opens.jsonl"
    original_open = os.open

    def blocked_open(
        path: str | bytes | os.PathLike[str] | os.PathLike[bytes],
        flags: int, mode: int = 0o777, *, dir_fd: int | None = None,
    ) -> int:
        began = time.monotonic()
        fd = original_open(path, flags, mode, dir_fd=dir_fd)
        if observation.active and path == "target" and dir_fd is not None:
            _append_event(log, {"began": began, "deadline": observation.deadline})
            time.sleep(10)
        return fd

    before_fds = set(os.listdir("/proc/self/fd"))
    with monkeypatch.context() as blocked:
        blocked.setattr(os, "open", blocked_open)
        for attempt in range(2):
            began = time.perf_counter()
            # The literal assertion above protects the production default;
            # this shorter allowance exercises the same cancellation, retry,
            # accounting, descriptor, and child-reaping contracts.
            measured, counters = _measure(fixture, wall_seconds=0.2)
            elapsed = time.perf_counter() - began
            assert elapsed <= 1.7, elapsed
            assert measured["subject"].status == "error"
            assert measured["subject"].bytes is None
            assert "wall allowance" in (measured["subject"].error or "")
            assert counters["work_limit"] == 100000
            assert counters["work_consumed"] == 0
            recorded = _events(log)
            assert len(recorded) == attempt + 1
            assert all(float(str(event["began"])) < float(str(event["deadline"])) for event in recorded)
            assert set(os.listdir("/proc/self/fd")) == before_fds
            _assert_reaped(children)

    healthy, _counters = _measure(fixture)
    assert healthy["subject"].status == "complete"
    assert healthy["subject"].bytes == fixture.expected_bytes("subject")
    assert len(_events(log)) == 2
    assert set(os.listdir("/proc/self/fd")) == before_fds
    _assert_reaped(children)


def test_killed_file_stat_keeps_all_three_debits_and_completed_directory_metric(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    fixture = _fixture(tmp_path, {"subject": (1,)})
    observation = _observe_traversal(monkeypatch)
    children = _child_pids(monkeypatch)
    log = tmp_path / "blocked-stat.jsonl"
    original_stat = os.stat

    def blocked_stat(
        path: int | str | bytes | os.PathLike[str] | os.PathLike[bytes],
        *, dir_fd: int | None = None, follow_symlinks: bool = True,
    ) -> os.stat_result:
        if observation.active and path == "artifact-000000" and dir_fd is not None:
            assert follow_symlinks is False
            assert observation.budget is not None
            _append_event(log, {"consumed": observation.budget.consumed})
            time.sleep(10)
        return original_stat(path, dir_fd=dir_fd, follow_symlinks=follow_symlinks)

    before_fds = set(os.listdir("/proc/self/fd"))
    with monkeypatch.context() as blocked:
        blocked.setattr(os, "stat", blocked_stat)
        began = time.perf_counter()
        measured, counters = _measure(fixture, wall_seconds=0.2)
        elapsed = time.perf_counter() - began
    assert elapsed <= 1.7, elapsed
    assert _events(log) == [{"consumed": 3}]
    assert measured["subject"].status == "error"
    assert measured["subject"].bytes is None
    assert "wall allowance" in (measured["subject"].error or "")
    assert counters["work_consumed"] == 3
    assert counters["work_remaining"] == 99997
    assert counters["directories_visited"] == 1
    assert counters["entries_visited"] == 0
    assert set(os.listdir("/proc/self/fd")) == before_fds
    _assert_reaped(children)


def test_expiry_after_first_real_open_prevents_second_traversal_open(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    fixture = _fixture(tmp_path, {"subject": (1,)})
    observation = _observe_traversal(monkeypatch)
    clock = FinalStatClock(observed=[])
    log = tmp_path / "open-starts.jsonl"
    original_open = os.open

    def expire_after_open(
        path: str | bytes | os.PathLike[str] | os.PathLike[bytes],
        flags: int, mode: int = 0o777, *, dir_fd: int | None = None,
    ) -> int:
        began = clock.monotonic()
        fd = original_open(path, flags, mode, dir_fd=dir_fd)
        if observation.active and path == "target" and dir_fd is not None:
            _append_event(log, {"began": began, "deadline": observation.deadline})
            clock.now = observation.deadline + 1
        return fd

    monkeypatch.setattr(time, "monotonic", clock.monotonic)
    monkeypatch.setattr(os, "open", expire_after_open)
    measured, _counters = _measure(fixture)
    assert _events(log) == [{"began": 0.0, "deadline": 5.0}]
    assert measured["subject"].status == "error"
    assert measured["subject"].bytes is None
    assert "wall allowance" in (measured["subject"].error or "")


def test_worker_rejects_oversized_real_child_result_and_reaps(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    children = _child_pids(monkeypatch)
    before_fds = set(os.listdir("/proc/self/fd"))
    with pytest.raises(cli.Refusal, match="result is oversized"):
        cli._run_audit_cache_worker(
            "test oversized result", lambda: {"padding": "x" * 512},
            deadline=time.monotonic() + 5, result_limit=64,
        )
    assert set(os.listdir("/proc/self/fd")) == before_fds
    _assert_reaped(children)


def test_worker_rejects_result_when_decoding_finishes_after_deadline(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    children = _child_pids(monkeypatch)
    original_loads = json.loads
    before_fds = set(os.listdir("/proc/self/fd"))
    with mmap.mmap(-1, 1) as clock, monkeypatch.context() as patched:
        patched.setattr(time, "monotonic", lambda: float(clock[0]))

        def late_decode(value: str | bytes | bytearray) -> object:
            decoded: object = original_loads(value)
            clock[0] = 6
            return decoded

        patched.setattr(json, "loads", late_decode)
        with pytest.raises(cli.Refusal, match="wall allowance"):
            cli._run_audit_cache_worker(
                "test late result", lambda: {"complete": True},
                deadline=5, result_limit=64,
            )
    assert set(os.listdir("/proc/self/fd")) == before_fds
    _assert_reaped(children)


def test_worker_accepts_timely_bounded_real_child_result(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    children = _child_pids(monkeypatch)
    before_fds = set(os.listdir("/proc/self/fd"))
    observed = cli._run_audit_cache_worker(
        "test positive result", lambda: {"answer": 7},
        deadline=time.monotonic() + 5, result_limit=64,
    )
    assert observed == {"answer": 7}
    assert set(os.listdir("/proc/self/fd")) == before_fds
    _assert_reaped(children)
