"""Pure planning tests for graph-wide parallel-scaling sweeps."""

from __future__ import annotations

import dataclasses
from pathlib import Path

import pytest

from dagrun.model import CmdType, Step
from dagrun.sweep import (
    CpuTopology,
    WidthSource,
    cpu_topology_from_core_ids,
    detect_cpu_topology,
    initial_widths,
    labeled_width_grid_for_pass,
    limit_topology,
    parse_target_duration,
    parse_widths,
    refine_width_grid,
    stable_topological_steps,
    workload_digest,
    width_grid_for_pass,
)


def test_stable_topological_order_preserves_ready_registration_order() -> None:
    steps = (
        Step("g", "independent", "", "true"),
        Step("g", "leaf", "", "true", deps=["g.root"]),
        Step("g", "root", "", "true"),
        Step("g", "after-independent", "", "true", deps=["g.independent"]),
    )

    ordered = stable_topological_steps(steps)

    assert [step.tag for step in ordered] == [
        "g.independent",
        "g.root",
        "g.leaf",
        "g.after-independent",
    ]


@pytest.mark.parametrize(
    ("steps", "message"),
    [
        (
            (Step("g", "same", "", "true"), Step("g", "same", "", "true")),
            "duplicate step tag",
        ),
        ((Step("g", "a", "", "true", deps=["g.missing"]),), "unknown dependencies"),
        (
            (
                Step("g", "a", "", "true", deps=["g.b"]),
                Step("g", "b", "", "true", deps=["g.a"]),
            ),
            "dependency cycle",
        ),
    ],
)
def test_stable_topological_order_rejects_invalid_graphs(
    steps: tuple[Step, ...], message: str
) -> None:
    with pytest.raises(ValueError, match=message):
        stable_topological_steps(steps)


def test_workload_digest_is_stable_and_sensitive_to_execution_inputs() -> None:
    step = Step(
        "g",
        "j",
        "",
        "echo hi",
        env={"B": "2", "A": "1"},
        jobs_flag="--workers=",
        jobs_env="N",
    )

    assert workload_digest(step, "-j", "") == "424d54398eaf4baa"
    reordered = Step(
        "g",
        "j",
        "",
        "echo hi",
        env={"A": "1", "B": "2"},
        jobs_flag="--workers=",
        jobs_env="N",
    )
    assert workload_digest(reordered, "-j", "") == "424d54398eaf4baa"
    changed = dataclasses.replace(step, cmd="echo changed")
    assert workload_digest(changed, "-j", "") != "424d54398eaf4baa"
    typed = dataclasses.replace(step, cmdtype=CmdType.GENERIC_WITH_FLAG)
    assert workload_digest(typed, "-j", "") != "424d54398eaf4baa"


def test_cpu_topology_counts_only_allowed_logical_cpus() -> None:
    # CPUs 0/4 and 1/5 are SMT siblings. The affinity excludes 4, so three allowed threads expose
    # three represented physical cores even though the host-wide map contains four threads.
    topology = cpu_topology_from_core_ids(
        (0, 1, 2),
        {0: (0, 0), 4: (0, 0), 1: (0, 1), 5: (0, 1), 2: (0, 2)},
    )

    assert topology.allowed_logical_cpus == (0, 1, 2)
    assert topology.logical_thread_count == 3
    assert topology.physical_core_count == 3


def test_cpu_topology_does_not_guess_from_partial_core_ids() -> None:
    topology = cpu_topology_from_core_ids((3, 7), {3: (0, 1)})

    assert topology.logical_thread_count == 2
    assert topology.physical_core_count is None


def _write_topology(root: Path, cpu: int, package: int, core: int) -> None:
    directory = root / f"cpu{cpu}" / "topology"
    directory.mkdir(parents=True)
    (directory / "physical_package_id").write_text(f"{package}\n", encoding="ascii")
    (directory / "core_id").write_text(f"{core}\n", encoding="ascii")


def test_detect_cpu_topology_reads_injected_sysfs_and_affinity(tmp_path: Path) -> None:
    _write_topology(tmp_path, 2, 0, 7)
    _write_topology(tmp_path, 6, 0, 7)
    _write_topology(tmp_path, 9, 1, 3)
    # cpu99 exists nowhere and is outside the injected affinity, so it is irrelevant.

    topology = detect_cpu_topology((9, 2, 6), sysfs_root=tmp_path)

    assert topology.allowed_logical_cpus == (2, 6, 9)
    assert topology.logical_thread_count == 3
    assert topology.physical_core_count == 2


def test_initial_widths_match_316_thread_158_core_machine() -> None:
    topology = CpuTopology(tuple(range(316)), physical_core_count=158)

    assert initial_widths(topology) == (1, 2, 4, 8, 16, 32, 64, 128, 158, 316)


def test_limit_topology_caps_logical_and_physical_counts() -> None:
    topology = CpuTopology(tuple(range(316)), physical_core_count=158)

    quota_limited = limit_topology(topology, 64)

    assert quota_limited.logical_thread_count == 64
    assert quota_limited.physical_core_count == 64
    assert initial_widths(quota_limited) == (1, 2, 4, 8, 16, 32, 64)


def test_limit_topology_keeps_unknown_physical_count_unknown() -> None:
    topology = CpuTopology(tuple(range(32)), physical_core_count=None)

    limited = limit_topology(topology, 12)

    assert limited.logical_thread_count == 12
    assert limited.physical_core_count is None
    assert limit_topology(topology, 64) is topology


def test_limit_topology_rejects_nonpositive_budget() -> None:
    with pytest.raises(ValueError, match="logical CPU limit must be positive"):
        limit_topology(CpuTopology((0,), physical_core_count=1), 0)


def test_initial_width_labels_retain_overlapping_reasons() -> None:
    topology = CpuTopology(tuple(range(16)), physical_core_count=8)
    points = {point.inner_jobs: point for point in labeled_width_grid_for_pass(topology, 1)}

    assert points[1].source_label == "power-of-two"
    assert points[8].sources == (WidthSource.POWER_OF_TWO, WidthSource.PHYSICAL_CORES)
    assert points[8].source_label == "power-of-two+physical-cores"
    assert points[16].source_label == "logical-threads"
    assert all(point.introduced_in_pass == 1 for point in points.values())


def test_explicit_width_source_has_a_stable_label() -> None:
    assert WidthSource.EXPLICIT.value == "explicit"


def test_unknown_physical_topology_uses_powers_through_logical_limit() -> None:
    topology = CpuTopology(tuple(range(12)), physical_core_count=None)

    assert initial_widths(topology) == (1, 2, 4, 8, 12)


def test_second_pass_is_the_cumulative_midpoint_grid() -> None:
    first = (1, 2, 4, 8, 16, 32, 64, 128, 158, 316)

    assert refine_width_grid(first) == (
        1,
        2,
        3,
        4,
        6,
        8,
        12,
        16,
        24,
        32,
        48,
        64,
        96,
        128,
        143,
        158,
        237,
        316,
    )
    assert width_grid_for_pass(first, 2) == refine_width_grid(first)


def test_later_passes_retain_width_origin_and_introduction_pass() -> None:
    topology = CpuTopology(tuple(range(8)), physical_core_count=4)

    second = {point.inner_jobs: point for point in labeled_width_grid_for_pass(topology, 2)}
    third = {point.inner_jobs: point for point in labeled_width_grid_for_pass(topology, 3)}

    assert tuple(second) == (1, 2, 3, 4, 6, 8)
    assert second[3].source_label == "midpoint"
    assert second[3].introduced_in_pass == 2
    assert third[3].introduced_in_pass == 2
    assert third[5].introduced_in_pass == 3
    assert third[4].source_label == "power-of-two+physical-cores"


def test_refinement_reaches_a_fixed_point_when_every_width_is_present() -> None:
    assert refine_width_grid((1, 2, 3, 4)) == (1, 2, 3, 4)
    assert width_grid_for_pass((1, 4), 3) == (1, 2, 3, 4)
    assert width_grid_for_pass((1, 4), 20) == (1, 2, 3, 4)


@pytest.mark.parametrize(
    ("raw", "widths"),
    [
        ("4", (1, 2, 3, 4)),
        ("2..4", (2, 3, 4)),
        ("1,4,8", (1, 4, 8)),
        ("1,3..5,8,3", (1, 3, 4, 5, 8)),
    ],
)
def test_parse_widths_supports_legacy_ranges_and_sparse_target_grids(
    raw: str, widths: tuple[int, ...]
) -> None:
    assert parse_widths(raw) == widths


@pytest.mark.parametrize("raw", ["", "0", "5..2", "1,,4", "1,two", "1..2..3"])
def test_parse_widths_rejects_invalid_specs(raw: str) -> None:
    with pytest.raises(ValueError, match="invalid --jobs"):
        parse_widths(raw)


@pytest.mark.parametrize(
    ("raw", "seconds"),
    [
        ("0", 0.0),
        ("0s", 0.0),
        ("250ms", 0.25),
        ("1", 1.0),
        ("1.5s", 1.5),
        ("2m", 120.0),
        ("1.25H", 4500.0),
    ],
)
def test_parse_target_duration(raw: str, seconds: float) -> None:
    assert parse_target_duration(raw) == seconds


@pytest.mark.parametrize("raw", ["", "-1s", "nan", "inf", "1 day", "1m30s"])
def test_parse_target_duration_rejects_negative_nonfinite_or_malformed(raw: str) -> None:
    with pytest.raises(ValueError, match="invalid --target-time"):
        parse_target_duration(raw)
