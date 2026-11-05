"""Tests for the stateful core-RESERVATION ledger.

Covers the three verifications the owner named for the allocator:

  1. DISJOINT under concurrency — two concurrent acquires never collide.
  2. COUNTED acquire/release — N sequential acquires each acquire AND release;
     N is stated and the ledger is empty afterward (guards against an allocator
     that trivially passes (1) by handing out nothing).
  3. DEAD-HOLDER reclaim — kill a holder mid-reservation; its cores are
     reclaimed, not leaked.

Plus PID-reuse fingerprinting and the InsufficientCores refusal.
"""

from __future__ import annotations

import json
import multiprocessing as mp
import os
import signal
import time
from pathlib import Path

import pytest

from dagrun import reservation as res


@pytest.fixture
def ledger(tmp_path: Path) -> Path:
    return tmp_path / "core-reservations.json"


def _n_cores() -> int:
    return len(os.sched_getaffinity(0))


@pytest.mark.parametrize("sample_s", [-1.0, float("nan"), float("inf")])
def test_acquire_rejects_invalid_sample_window(ledger: Path, sample_s: float) -> None:
    with pytest.raises(ValueError, match="sample_s must be finite and >= 0"):
        res.acquire(1, ledger=ledger, sample_s=sample_s)


def test_acquire_requires_a_real_sample_for_an_irq_budget(ledger: Path) -> None:
    with pytest.raises(ValueError, match="sample_s must be > 0"):
        res.acquire(1, ledger=ledger, sample_s=0.0, max_irq_rate=10.0)


def test_corrupt_ledger_fails_closed(ledger: Path) -> None:
    ledger.write_text("{")
    ledger.chmod(0o600)
    with pytest.raises(res.ReservationStateError, match="corrupt"):
        res.held_cores(ledger)


def _invalid_records() -> list[tuple[str, dict[str, object]]]:
    base: dict[str, object] = {
        "pid": 1,
        "starttime": 1,
        "cores": [0],
        "tag": "holder",
        "ts": 1.0,
    }

    def changed(**fields: object) -> dict[str, object]:
        return {**base, **fields}

    def missing(field: str) -> dict[str, object]:
        record = dict(base)
        del record[field]
        return record

    return [
        ("mixed-invalid-core", changed(cores=[0, "bad"])),
        ("missing-cores", missing("cores")),
        ("empty-cores", changed(cores=[])),
        ("negative-core", changed(cores=[-1])),
        ("overflow-core", changed(cores=[1 << 32])),
        ("boolean-core", changed(cores=[True])),
        ("fractional-core", changed(cores=[1.0])),
        ("duplicate-core", changed(cores=[1, 1])),
        ("zero-pid", changed(pid=0)),
        ("overflow-pid", changed(pid=1 << 32)),
        ("boolean-pid", changed(pid=True)),
        ("missing-pid", missing("pid")),
        ("missing-starttime", missing("starttime")),
        ("zero-starttime", changed(starttime=0)),
        ("string-starttime", changed(starttime="1")),
        ("overflow-starttime", changed(starttime=1 << 64)),
        ("non-string-tag", changed(tag=7)),
        ("missing-tag", missing("tag")),
        ("missing-ts", missing("ts")),
        ("nonfinite-ts", changed(ts=float("inf"))),
        ("string-ts", changed(ts="1.0")),
        ("boolean-ts", changed(ts=True)),
        ("negative-ts", changed(ts=-1)),
    ]


@pytest.mark.parametrize(("case", "record"), _invalid_records(), ids=lambda value: str(value))
def test_invalid_record_schema_fails_without_rewrite(
    ledger: Path, case: str, record: dict[str, object]
) -> None:
    original = json.dumps({"reservations": [record]})
    ledger.write_text(original)
    ledger.chmod(0o600)

    with pytest.raises(res.ReservationStateError, match="invalid record|corrupt"):
        res.held_cores(ledger)

    assert ledger.read_text() == original, f"{case} was silently repaired"


def test_fifo_lock_and_ledger_fail_without_blocking(ledger: Path) -> None:
    lock = ledger.with_suffix(ledger.suffix + ".lock")
    os.mkfifo(lock, mode=0o600)
    with pytest.raises(res.ReservationStateError, match="lock"):
        res.held_cores(ledger)
    lock.unlink()

    os.mkfifo(ledger, mode=0o600)
    with pytest.raises(res.ReservationStateError, match="regular file"):
        res.held_cores(ledger)


# --------------------------------------------------------------------------- #
# 1. DISJOINT under concurrency                                               #
# --------------------------------------------------------------------------- #

def _child_acquire(ledger: str, hold_s: float, out: str) -> None:
    """Child process: acquire 1 core, record it, hold, then release."""
    r = res.acquire(1, tag="conc", sample_s=0.03, ledger=Path(ledger))
    Path(out).write_text(",".join(str(c) for c in r.cores))
    time.sleep(hold_s)
    r.release()


def test_concurrent_acquires_never_collide(ledger: Path) -> None:
    """NEGATIVE (collision must not happen): two processes acquiring 1 core each
    AT THE SAME TIME must get DISJOINT cores. Sampling alone cannot guarantee
    this — the ledger lock + held-set exclusion must."""
    if _n_cores() < 2:
        pytest.skip(f"need >=2 allowed cores for a disjoint 2x1 acquire; have {_n_cores()}")
    ctx = mp.get_context("fork")
    out_a = str(ledger.parent / "a.out")
    out_b = str(ledger.parent / "b.out")
    # Each holds ~0.5s so the two reservations are provably live simultaneously.
    pa = ctx.Process(target=_child_acquire, args=(str(ledger), 0.5, out_a))
    pb = ctx.Process(target=_child_acquire, args=(str(ledger), 0.5, out_b))
    pa.start()
    pb.start()
    pa.join(30)
    pb.join(30)
    assert pa.exitcode == 0 and pb.exitcode == 0, (pa.exitcode, pb.exitcode)
    cores_a = set(int(x) for x in Path(out_a).read_text().split(","))
    cores_b = set(int(x) for x in Path(out_b).read_text().split(","))
    assert cores_a and cores_b, (cores_a, cores_b)
    assert cores_a.isdisjoint(cores_b), (
        f"concurrent reservations collided: A={cores_a} B={cores_b} — the ledger did not "
        "prevent two acquires from taking the same core"
    )


# --------------------------------------------------------------------------- #
# 2. COUNTED sequential acquire + release                                      #
# --------------------------------------------------------------------------- #

def test_n_sequential_acquire_and_release(ledger: Path) -> None:
    """POSITIVE + COUNTED: perform N=8 sequential acquire→release cycles. Each
    must return a non-empty core set AND leave the ledger empty after release
    (proving release actually frees, not merely that acquire never collides).
    N is stated so a hand-out-nothing allocator cannot pass by returning []."""
    N = 8
    completed = 0
    for i in range(N):
        r = res.acquire(1, tag=f"seq{i}", ledger=ledger, sample_s=0.03)
        assert len(r.cores) == 1, f"cycle {i}: expected 1 core, got {r.cores}"
        # Held while alive:
        assert set(r.cores).issubset(set(res.held_cores(ledger)))
        r.release()
        # Freed after release:
        assert res.held_cores(ledger) == [], f"cycle {i}: ledger not empty after release"
        completed += 1
    assert completed == N, f"expected {N} acquire/release cycles, completed {completed}"


def test_release_is_idempotent(ledger: Path) -> None:
    r = res.acquire(1, tag="idem", ledger=ledger, sample_s=0.03)
    r.release()
    r.release()  # must not raise or corrupt the ledger
    assert res.held_cores(ledger) == []


def test_releasing_one_same_tag_reservation_keeps_the_other(ledger: Path) -> None:
    if _n_cores() < 2:
        pytest.skip("need two cores for simultaneous reservations")
    first = res.acquire(1, tag="same", ledger=ledger, sample_s=0.01)
    second = res.acquire(1, tag="same", ledger=ledger, sample_s=0.01)
    assert first.cores != second.cores
    first.release()
    assert res.held_cores(ledger) == second.cores
    second.release()


def test_context_manager_releases_on_exception(ledger: Path) -> None:
    with pytest.raises(RuntimeError):
        with res.reserve_cores(1, tag="ctx", ledger=ledger, sample_s=0.03):
            assert res.held_cores(ledger) != []
            raise RuntimeError("boom")
    assert res.held_cores(ledger) == [], "reserve_cores must release even on exception"


# --------------------------------------------------------------------------- #
# 3. DEAD-HOLDER reclaim                                                       #
# --------------------------------------------------------------------------- #

def _child_acquire_and_hang(ledger: str, ready: str) -> None:
    """Acquire 1 core, signal ready, then hang forever (until SIGKILL)."""
    res.acquire(1, tag="victim", sample_s=0.03, ledger=Path(ledger))
    Path(ready).write_text("ready")
    time.sleep(3600)


def test_dead_holder_is_reclaimed(ledger: Path) -> None:
    """Kill a holder mid-reservation (SIGKILL → no release runs) and confirm its
    core is RECLAIMED, not leaked. This is the leaked-scope failure class: a
    crashed holder must not permanently subtract a core from the pool."""
    ctx = mp.get_context("fork")
    ready = str(ledger.parent / "ready")
    p = ctx.Process(target=_child_acquire_and_hang, args=(str(ledger), ready))
    p.start()
    # Wait until the child has recorded its reservation.
    for _ in range(300):
        if Path(ready).exists():
            break
        time.sleep(0.02)
    else:
        p.kill(); p.join()
        pytest.fail("child never acquired")

    held_before = res.held_cores(ledger)
    assert held_before, "child's reservation should be visible before the kill"

    # SIGKILL: the holder cannot release (simulates a crash / leaked reservation).
    assert p.pid is not None
    os.kill(p.pid, signal.SIGKILL)
    p.join(10)
    assert p.exitcode is not None

    # The record is still in the file (nobody released it)...
    reclaimed = res.reclaim_dead(ledger)
    assert reclaimed, "dead holder's record should have been reclaimed by the sweep"
    assert res.held_cores(ledger) == [], "cores must be freed after dead-holder reclaim"


def test_acquire_sweeps_dead_holder_and_reuses_core(ledger: Path) -> None:
    """acquire itself must reclaim a dead holder — even at full occupancy, a
    subsequent acquire succeeds by sweeping the corpse first."""
    ctx = mp.get_context("fork")
    ready = str(ledger.parent / "ready2")
    p = ctx.Process(target=_child_acquire_and_hang, args=(str(ledger), ready))
    p.start()
    for _ in range(300):
        if Path(ready).exists():
            break
        time.sleep(0.02)
    else:
        p.kill(); p.join()
        pytest.fail("child never acquired")

    assert p.pid is not None
    os.kill(p.pid, signal.SIGKILL)
    p.join(10)

    # Even without a manual reclaim, acquire's own sweep must free the dead core.
    r = res.acquire(1, tag="reuser", ledger=ledger, sample_s=0.03)
    assert len(r.cores) == 1
    r.release()


# --------------------------------------------------------------------------- #
# PID-reuse fingerprinting + refusal                                          #
# --------------------------------------------------------------------------- #

def test_pid_reuse_does_not_keep_stale_holder_alive(ledger: Path) -> None:
    """A record whose PID is recycled by an unrelated process (different
    starttime) must be treated as DEAD — bare kill(pid,0) would wrongly keep it."""
    # Hand-write a record for OUR pid but a WRONG starttime (as if the PID were reused).
    real_start = res._proc_starttime(os.getpid())
    with res._LedgerLock(ledger):
        res._store(ledger, [res._Record(pid=os.getpid(), starttime=(real_start or 0) + 999999,
                                        cores=[0], tag="stale", ts=1.0)])
    assert res.held_cores(ledger) == [], "stale (mismatched-starttime) record must be swept"


@pytest.mark.parametrize("failure", [PermissionError("denied"), b"malformed"])
def test_holder_identity_uncertainty_refuses_to_reclaim_cores(
    monkeypatch: pytest.MonkeyPatch, failure: BaseException | bytes,
) -> None:
    """Only positive procfs disappearance may release another run's reservation."""

    def read_bytes(_path: Path) -> bytes:
        if isinstance(failure, BaseException):
            raise failure
        return failure

    monkeypatch.setattr(Path, "read_bytes", read_bytes)
    record = res._Record(pid=4242, starttime=7, cores=[0], tag="live", ts=1.0)
    with pytest.raises(res.ReservationStateError, match="process 4242"):
        res._sweep([record])


def test_disappeared_holder_can_be_reclaimed_from_core_ledger(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """ENOENT remains positive evidence that the recorded process is gone."""

    def disappeared(_path: Path) -> bytes:
        raise FileNotFoundError("exited during procfs scan")

    monkeypatch.setattr(Path, "read_bytes", disappeared)
    record = res._Record(pid=4242, starttime=7, cores=[0], tag="dead", ts=1.0)
    assert res._sweep([record]) == ([], [record])


def test_insufficient_cores_refuses(ledger: Path) -> None:
    """When the free-and-unheld pool is smaller than K, acquire RAISES rather
    than returning an overlapping set."""
    n = _n_cores()
    with pytest.raises(res.InsufficientCoresError):
        res.acquire(n + 1, tag="toomany", ledger=ledger, sample_s=0.03)
    # Nothing should be recorded on failure.
    assert res.held_cores(ledger) == []
