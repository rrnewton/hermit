"""The root-binding allowance applies to real blocking filesystem operations."""

from __future__ import annotations

import contextlib
import json
import os
import signal
import time
from collections.abc import Mapping, Sequence
from pathlib import Path

import pytest

from wrkslots import cli
from wrkslots.tests.test_census_subject_progress import CensusFixture, _fixture


def _measure(
    fixture: CensusFixture,
    *,
    wall_seconds: float = 5,
    errors: dict[str, str] | None = None,
) -> tuple[dict[str, cli._AuditCacheMeasurement], dict[str, int]]:
    return cli._audit_cache_census(
        fixture.config,
        (cli.ActiveState("testhost", 1, ()),),
        fixture.planned,
        errors or {},
        state_path=fixture.state_path,
        work_limit=100000,
        wall_seconds=wall_seconds,
    )


def _delay_checkout_opens(
    monkeypatch: pytest.MonkeyPatch,
    fixture: CensusFixture,
    seconds: float,
    log: Path,
    *,
    only_subject: str | None = None,
) -> None:
    original_open = os.open
    checkout_paths = {
        str(cache.checkout_root)
        for subject, caches in fixture.planned.items()
        if only_subject is None or subject == only_subject
        for cache in caches
    }

    def delayed_open(
        path: str | bytes | os.PathLike[str] | os.PathLike[bytes],
        flags: int,
        mode: int = 0o777,
        *,
        dir_fd: int | None = None,
    ) -> int:
        raw = os.fspath(path)
        if raw in checkout_paths and dir_fd is None:
            # Open the log anew in the child: inherited test descriptors must
            # have been closed before the binding work starts.
            with log.open("a", encoding="utf-8") as output:
                output.write(
                    json.dumps({"path": raw, "started": time.monotonic()}) + "\n"
                )
            time.sleep(seconds)
        return original_open(path, flags, mode, dir_fd=dir_fd)

    monkeypatch.setattr(os, "open", delayed_open)


def _child_pids(monkeypatch: pytest.MonkeyPatch) -> list[int]:
    original_fork = os.fork
    children: list[int] = []

    def observed_fork() -> int:
        pid = original_fork()
        if pid > 0:
            children.append(pid)
        return pid

    monkeypatch.setattr(os, "fork", observed_fork)
    return children


def _assert_reaped(children: list[int]) -> None:
    assert children
    for pid in children:
        with pytest.raises(ChildProcessError):
            os.waitpid(pid, os.WNOHANG)


def test_twelve_real_roots_stop_binding_at_the_default_five_second_deadline(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    assert cli._AUDIT_CACHE_WALL_SECONDS == 5
    fixture = _fixture(
        tmp_path,
        {"subject": (1,) * 12, "empty": (), "refused": ()},
    )
    log = tmp_path / "actual-open-starts.jsonl"
    _delay_checkout_opens(monkeypatch, fixture, 0.1, log)
    original_identity = cli._audit_cache_root_identity
    calls: list[tuple[float, float]] = []

    def observed_identity(
        config: cli.Config,
        cache: cli.CacheDirectory,
        *,
        deadline: float | None = None,
    ) -> tuple[int, int, int, int, int] | None:
        assert deadline is not None
        calls.append((time.monotonic(), deadline))
        return original_identity(config, cache, deadline=deadline)

    monkeypatch.setattr(cli, "_audit_cache_root_identity", observed_identity)
    started = time.perf_counter()

    # The default is asserted above; a short explicit allowance proves the
    # same shared-deadline and no-late-start behavior without waiting five
    # wall-clock seconds.
    measured, counters = _measure(
        fixture, wall_seconds=0.5, errors={"refused": "original refusal"}
    )

    elapsed = time.perf_counter() - started
    # The short work allowance, at most the existing one-second reap
    # allowance, plus half a second of process scheduling/serialization.
    assert elapsed <= 2.0, elapsed
    assert 0 < len(calls) < 12
    assert all(began < deadline for began, deadline in calls)
    deadlines = {deadline for _began, deadline in calls}
    assert len(deadlines) == 1
    deadline = next(iter(deadlines))
    observed = [json.loads(line) for line in log.read_text().splitlines()]
    assert 0 < len(observed) < 12
    assert all(entry["started"] < deadline for entry in observed)
    assert counters["work_consumed"] == 0
    assert counters["directories_visited"] == 0
    assert measured["empty"] == cli._AuditCacheMeasurement(0, "complete")
    for subject, item in measured.items():
        if subject != "empty":
            assert item.status == "error"
            assert item.bytes is None
    assert measured["refused"].error == "original refusal"
    print(f"bounded binding deadline: {elapsed:.3f}s, {len(calls)}/12 bindings started")


def test_one_blocked_open_is_cancelled_before_its_sleep_finishes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fixture = _fixture(tmp_path, {"subject": (1,)})
    log = tmp_path / "blocked-open.jsonl"
    _delay_checkout_opens(monkeypatch, fixture, 3, log)
    children = _child_pids(monkeypatch)
    before_fds = set(os.listdir("/proc/self/fd"))
    started = time.perf_counter()

    with pytest.raises(cli.Refusal, match="root binding exceeded the fixed wall"):
        cli._audit_cache_root_identity(
            fixture.config,
            fixture.planned["subject"][0],
            deadline=time.monotonic() + 0.2,
        )

    elapsed = time.perf_counter() - started
    assert elapsed <= 1.7, elapsed
    assert len(log.read_text().splitlines()) == 1
    assert set(os.listdir("/proc/self/fd")) == before_fds
    _assert_reaped(children)


def test_unbound_subjects_remain_unavailable_on_repeated_deadline_exhaustion(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fixture = _fixture(tmp_path, {"first": (1,), "later": (1,), "empty": ()})
    log = tmp_path / "repeated-open.jsonl"
    _delay_checkout_opens(monkeypatch, fixture, 3, log)
    for selected in ("first", "later"):
        measured, counters = _measure(fixture, wall_seconds=0.15)
        assert measured["empty"] == cli._AuditCacheMeasurement(0, "complete")
        assert counters["work_consumed"] == 0
        for subject in ("first", "later"):
            assert measured[subject].status in {"partial", "error"}
            assert measured[subject].bytes is None
        assert measured[selected].status == "error"
        assert "wall allowance" in (measured[selected].error or "")
    entries = [json.loads(line) for line in log.read_text().splitlines()]
    assert len(entries) == 2
    assert [entry["path"] for entry in entries] == [
        str(fixture.planned[subject][0].checkout_root) for subject in ("first", "later")
    ]


def test_costly_binding_peer_does_not_prevent_cheap_subject_next_turn(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fixture = _fixture(tmp_path, {"aa-costly": (1,), "zz-cheap": (1,)})
    _delay_checkout_opens(
        monkeypatch,
        fixture,
        3,
        tmp_path / "costly-open.jsonl",
        only_subject="aa-costly",
    )
    first, _counters = _measure(fixture, wall_seconds=0.3)
    assert first["aa-costly"].status == "error"
    assert first["aa-costly"].bytes is None
    assert first["zz-cheap"].status in {"partial", "error"}
    assert first["zz-cheap"].bytes is None

    second, counters = _measure(fixture, wall_seconds=0.3)

    assert second["zz-cheap"].status == "complete"
    assert second["zz-cheap"].bytes == fixture.expected_bytes("zz-cheap")
    assert counters["directories_finalized"] == 1
    assert counters["entries_finalized"] == 1
    assert second["aa-costly"].bytes is None
    assert second["aa-costly"].status == "error"


def test_persisted_root_validation_does_not_start_after_deadline(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fixture = _fixture(tmp_path, {"subject": (0,) * 20, "empty": ()})
    warmup, _counters = _measure(fixture)
    assert warmup["subject"].status == "complete"
    assert len(fixture.stored_roots()) == 20
    original_validate = cli._validated_audit_cache_root
    now = 0.0
    started: list[float] = []

    def delayed_validation(
        value: Mapping[str, object],
        cache: cli.CacheDirectory,
        identity: Sequence[int],
        *,
        deadline: float | None = None,
    ) -> dict[str, object]:
        nonlocal now
        started.append(now)
        validated = original_validate(value, cache, identity, deadline=deadline)
        now += 1
        return validated

    monkeypatch.setattr(time, "monotonic", lambda: now)
    monkeypatch.setattr(cli, "_validated_audit_cache_root", delayed_validation)

    measured, counters = _measure(fixture)

    assert started == [0, 1, 2, 3, 4]
    assert measured["subject"].status == "error"
    assert measured["subject"].bytes is None
    assert measured["empty"] == cli._AuditCacheMeasurement(0, "complete")
    assert counters["work_consumed"] == 0


def test_one_large_persisted_root_stops_structural_checks_at_deadline(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fixture = _fixture(tmp_path, {"subject": (20,)})
    warmup, _counters = _measure(fixture)
    assert warmup["subject"].status == "complete"
    (root,) = fixture.stored_roots()
    directory = fixture.planned["subject"][0]
    identity = cli._audit_cache_root_identity(fixture.config, directory)
    assert identity is not None
    original_relative = cli._audit_cache_relative
    now = 0.0
    started: list[float] = []

    def delayed_relative(value: object, label: str) -> str:
        nonlocal now
        if label.startswith("audit cache root.verified_entries["):
            started.append(now)
            now += 1
        return original_relative(value, label)

    monkeypatch.setattr(time, "monotonic", lambda: now)
    monkeypatch.setattr(cli, "_audit_cache_relative", delayed_relative)

    with pytest.raises(cli.Refusal, match="wall allowance"):
        cli._validated_audit_cache_root(root, directory, identity, deadline=5)

    assert started == [0, 1, 2, 3, 4]


def test_expired_parent_budget_does_not_fork(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fixture = _fixture(tmp_path, {"subject": (1,)})

    def unexpected_fork() -> int:
        pytest.fail("root binding forked after its deadline")

    monkeypatch.setattr(os, "fork", unexpected_fork)
    with pytest.raises(cli.Refusal, match="root binding exceeded the fixed wall"):
        cli._audit_cache_root_identity(
            fixture.config,
            fixture.planned["subject"][0],
            deadline=time.monotonic() - 1,
        )


def test_descheduled_child_refuses_before_any_root_binding(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fixture = _fixture(tmp_path, {"subject": (1,)})
    marker = tmp_path / "late-child-entered-binding"
    original_fork = os.fork
    original_monotonic = time.monotonic
    original_identity = cli._audit_cache_root_identity_unbounded
    child_offset = 0.0

    def late_fork() -> int:
        nonlocal child_offset
        pid = original_fork()
        if pid == 0:
            # Only the child's clock advances, modelling it being scheduled
            # after the parent admitted the fork but after the work deadline.
            child_offset = 10
        return pid

    def observed_identity(
        config: cli.Config, cache: cli.CacheDirectory
    ) -> tuple[int, int, int, int, int] | None:
        marker.write_text("binding began\n", encoding="utf-8")
        return original_identity(config, cache)

    monkeypatch.setattr(os, "fork", late_fork)
    monkeypatch.setattr(time, "monotonic", lambda: original_monotonic() + child_offset)
    monkeypatch.setattr(cli, "_audit_cache_root_identity_unbounded", observed_identity)

    with pytest.raises(cli.Refusal, match="root binding exceeded the fixed wall"):
        cli._audit_cache_root_identity(
            fixture.config,
            fixture.planned["subject"][0],
            deadline=original_monotonic() + 1,
        )

    assert not marker.exists()


def test_binding_child_closes_inherited_control_descriptors(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fixture = _fixture(tmp_path, {"subject": (1,)})
    control = os.open(tmp_path / "parent-control", os.O_CREAT | os.O_RDWR, 0o600)
    try:
        identity = os.fstat(control)
        original_identity = cli._audit_cache_root_identity_unbounded

        def check_inherited_descriptors(
            config: cli.Config, cache: cli.CacheDirectory
        ) -> tuple[int, int, int, int, int] | None:
            for name in os.listdir("/proc/self/fd"):
                try:
                    current = os.fstat(int(name))
                except OSError:
                    continue
                if (current.st_dev, current.st_ino) == (identity.st_dev, identity.st_ino):
                    raise cli.Refusal("child retained a parent control descriptor")
            return original_identity(config, cache)

        monkeypatch.setattr(
            cli, "_audit_cache_root_identity_unbounded", check_inherited_descriptors
        )
        observed = cli._audit_cache_root_identity(
            fixture.config,
            fixture.planned["subject"][0],
            deadline=time.monotonic() + 2,
        )
        assert observed is not None
        cache_metadata = fixture.planned["subject"][0].path.stat()
        assert observed[:2] == (cache_metadata.st_dev, cache_metadata.st_ino)
        assert os.fstat(control).st_ino == identity.st_ino
    finally:
        os.close(control)


def test_pidfd_setup_failure_cancels_and_reaps_without_fd_leaks(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fixture = _fixture(tmp_path, {"subject": (1,)})
    _delay_checkout_opens(monkeypatch, fixture, 3, tmp_path / "pidfd-open.jsonl")
    children = _child_pids(monkeypatch)

    def fail_pidfd(_pid: int, _flags: int = 0) -> int:
        raise OSError("injected pidfd failure")

    monkeypatch.setattr(os, "pidfd_open", fail_pidfd)
    before = set(os.listdir("/proc/self/fd"))
    started = time.perf_counter()

    with pytest.raises(OSError, match="injected pidfd failure"):
        cli._audit_cache_root_identity(
            fixture.config,
            fixture.planned["subject"][0],
            deadline=time.monotonic() + 1,
        )

    assert time.perf_counter() - started <= 1.5
    assert set(os.listdir("/proc/self/fd")) == before
    _assert_reaped(children)


def test_cleanup_failure_still_closes_all_parent_descriptors(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fixture = _fixture(tmp_path, {"subject": (1,)})
    _delay_checkout_opens(monkeypatch, fixture, 3, tmp_path / "cleanup-open.jsonl")
    children = _child_pids(monkeypatch)
    original_kill = os.kill

    def failed_kill(pid: int, sig: int) -> None:
        # Deliver the signal before fault injection so this test never leaves
        # a deliberately live child behind if cancellation itself raises.
        original_kill(pid, sig)
        raise OSError("injected cancellation failure")

    monkeypatch.setattr(os, "kill", failed_kill)
    before = set(os.listdir("/proc/self/fd"))
    try:
        with pytest.raises(OSError, match="injected cancellation failure"):
            cli._audit_cache_root_identity(
                fixture.config,
                fixture.planned["subject"][0],
                deadline=time.monotonic() + 0.15,
            )
        assert set(os.listdir("/proc/self/fd")) == before
    finally:
        for pid in children:
            with contextlib.suppress(ChildProcessError):
                os.waitpid(pid, 0)


def test_uncancellable_child_has_a_bounded_reap_wait(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fixture = _fixture(tmp_path, {"subject": (1,)})
    _delay_checkout_opens(monkeypatch, fixture, 3, tmp_path / "unreaped-open.jsonl")
    children = _child_pids(monkeypatch)
    original_kill = os.kill
    before = set(os.listdir("/proc/self/fd"))
    # Model SIGKILL not promptly ending a kernel-blocked child. The test's own
    # finally uses the real signal after checking the bounded cleanup result.
    monkeypatch.setattr(os, "kill", lambda _pid, _sig: None)
    started = time.perf_counter()
    try:
        with pytest.raises(cli.Refusal, match="root binding exceeded the fixed wall"):
            cli._audit_cache_root_identity(
                fixture.config,
                fixture.planned["subject"][0],
                deadline=time.monotonic() + 0.15,
            )
        assert time.perf_counter() - started <= 1.65
        assert set(os.listdir("/proc/self/fd")) == before
        assert children
        for pid in children:
            assert os.waitpid(pid, os.WNOHANG) == (0, 0)
    finally:
        for pid in children:
            with contextlib.suppress(ProcessLookupError):
                original_kill(pid, signal.SIGKILL)
            with contextlib.suppress(ChildProcessError):
                os.waitpid(pid, 0)
