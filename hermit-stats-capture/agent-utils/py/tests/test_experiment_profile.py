"""Tests for stable profile keys + the sample store: mandatory-dim folding, ephemeral keys,
and the DERIVED-vs-UNSET cost estimate (requirement 3: never a fabricated constant)."""

from __future__ import annotations

from pathlib import Path

from parallel_experiment_runner.model import ExperimentSpec, HitCondition, WorkerLimits
from parallel_experiment_runner.profile import (
    CONSERVATIVE_PCTL,
    MAX_SAMPLES_PER_KEY,
    ProfileStore,
    Sample,
    profile_identity,
)


def _spec(**kw: object) -> ExperimentSpec:
    base: dict[str, object] = {
        "name": "s",
        "command": ("widget", "run", "--seed", "{seed}", "./demo"),
    }
    base.update(kw)
    return ExperimentSpec(**base)  # type: ignore[arg-type]


def test_ephemeral_when_no_identity_or_key() -> None:
    ident = profile_identity(_spec())
    assert ident.ephemeral is True
    assert ident.key.startswith("ephemeral.")


def test_auto_key_when_identity_present() -> None:
    ident = profile_identity(_spec(identity={"backend": "ptrace"}))
    assert ident.ephemeral is False
    assert ident.key.startswith("auto.")


def test_seed_free_command_hashes_equal() -> None:
    # The key excludes the seed, so two rounds over different seed ranges share history.
    a = profile_identity(_spec(identity={"backend": "ptrace"}))
    b = profile_identity(_spec(identity={"backend": "ptrace"}))
    assert a.key == b.key


def test_mandatory_dim_splits_even_a_manual_key() -> None:
    # A manual label cannot merge two runs that differ on a MANDATORY hard dimension.
    ptrace = profile_identity(
        _spec(profile_key="mylabel", identity={"backend": "ptrace"})
    )
    kvm = profile_identity(_spec(profile_key="mylabel", identity={"backend": "kvm"}))
    assert ptrace.key != kvm.key
    assert ptrace.key.startswith("mylabel.")
    assert kvm.key.startswith("mylabel.")


def test_hit_semantics_change_the_key() -> None:
    a = profile_identity(
        _spec(identity={"backend": "x"}, hit=HitCondition(regex="panic"))
    )
    b = profile_identity(
        _spec(identity={"backend": "x"}, hit=HitCondition(regex="DIVERGENCE"))
    )
    assert a.key != b.key


def test_detached_resource_accounting_splits_profile_history() -> None:
    plain = _spec(identity={"backend": "ptrace"})
    detached = _spec(
        identity={"backend": "ptrace"},
        worker_limits=WorkerLimits(memory_bytes=1024, cpu_timeout_s=5, pids_max=10),
        detached_resource_report="/tmp/resource-{seed}.txt",
        detached_unit_prefix="worker-",
    )
    assert profile_identity(plain).key != profile_identity(detached).key
    assert (
        profile_identity(_spec(profile_key="same", identity={"backend": "ptrace"})).key
        != profile_identity(
            _spec(
                profile_key="same",
                identity={"backend": "ptrace"},
                worker_limits=WorkerLimits(
                    memory_bytes=1024, cpu_timeout_s=5, pids_max=10
                ),
                detached_resource_report="/tmp/resource-{seed}.txt",
                detached_unit_prefix="worker-",
            )
        ).key
    )


def test_estimate_unset_with_no_samples(tmp_path: Path) -> None:
    store = ProfileStore(tmp_path / "p.json")
    est = store.estimate("auto.deadbeef")
    assert est.is_set is False
    assert est.wall_s is None and est.cpu_s is None and est.peak_mem_bytes is None
    assert est.samples == 0


def test_estimate_derived_after_record(tmp_path: Path) -> None:
    store = ProfileStore(tmp_path / "p.json")
    key = "auto.cafef00d"
    store.record(
        key,
        [
            Sample(
                wall_s=10.0, cpu_s=5.0, peak_bytes=100, disk_bytes=None, tasks_peak=3
            ),
            Sample(
                wall_s=20.0, cpu_s=9.0, peak_bytes=200, disk_bytes=None, tasks_peak=5
            ),
            Sample(
                wall_s=30.0, cpu_s=15.0, peak_bytes=300, disk_bytes=None, tasks_peak=7
            ),
        ],
    )
    est = store.estimate(key)
    assert est.is_set is True
    assert est.samples == 3
    # Wall uses the median (contention-inflated tail should not dominate the central figure).
    assert est.wall_s == 20.0
    # CPU/mem use the conservative high percentile (don't under-provision).
    assert CONSERVATIVE_PCTL == 90
    assert est.cpu_s == 15.0
    assert est.peak_mem_bytes == 300
    assert est.peak_tasks == 7


def test_estimate_survives_a_fresh_store_instance(tmp_path: Path) -> None:
    path = tmp_path / "p.json"
    ProfileStore(path).record(
        "auto.k", [Sample(wall_s=1.0, cpu_s=1.0, peak_bytes=1, disk_bytes=None)]
    )
    # A new process (new store object) reads the persisted samples.
    assert ProfileStore(path).estimate("auto.k").samples == 1


def test_store_bounds_sample_count(tmp_path: Path) -> None:
    store = ProfileStore(tmp_path / "p.json")
    store.record(
        "auto.k",
        [
            Sample(wall_s=float(i), cpu_s=None, peak_bytes=None, disk_bytes=None)
            for i in range(MAX_SAMPLES_PER_KEY + 50)
        ],
    )
    assert store.estimate("auto.k").samples == MAX_SAMPLES_PER_KEY


def test_unreadable_store_behaves_as_empty(tmp_path: Path) -> None:
    path = tmp_path / "corrupt.json"
    path.write_text("{ this is not json", encoding="utf-8")
    store = ProfileStore(path)  # must not raise
    assert store.estimate("auto.k").is_set is False
