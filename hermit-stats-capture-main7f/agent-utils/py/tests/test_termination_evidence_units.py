"""A termination record must say which quantity crossed which limit.

The CPU guard compares cgroup ``cpu.stat`` CPU-seconds against a CPU-second budget, correctly.
The *record* of that decision used to print an unlabelled ``elapsed_s`` — which was WALL — beside
a ``limit_s`` that was CPU-seconds.  Side by side and unlabelled, the natural reading is that the
two are comparable, and they are not: for one step the wall figure keeps rising while the process
is descheduled and the CPU figure does not, so a CPU breach could be quoted as having consumed
more seconds than its own run's whole CPU rollup contained.

No bound moves here and no kill decision changes.  What changes is that the compared quantity is
recorded under its own name with its own unit, and the wall figure is labelled as the context it
always was.
"""

from __future__ import annotations

import json
import os
from pathlib import Path

from dagrun import DagConfig, Runner, Step
from dagrun.attribution import LOG_DIR_ENV
from dagrun.cgroup import NoopCgroups
from dagrun.scheduler import BudgetUnit

#: What the fake cgroup reports the step has burned. Deliberately NOT close to the wall time the
#: step will have accumulated when the monitor trips (~1 poll interval), so a record that quietly
#: substituted one for the other is visibly wrong rather than coincidentally right.
_FAKE_CPU_USED_S = 37.5
_CPU_BUDGET_S = 2


class _OverBudgetCgroups(NoopCgroups):
    """A boxed manager that reports a step already far past its CPU budget."""

    enabled = True

    def cpu_stats(self, tag: str) -> dict[str, int]:
        return {"usage_usec": int(_FAKE_CPU_USED_S * 1_000_000)}


def _run_one_step_over_cpu_budget(log_dir: Path) -> dict[str, str]:
    """Run a step that trips the CPU guard; return its ``cpu_timeout`` journal record."""
    cfg = DagConfig(
        steps=(
            Step(
                "g",
                "spin",
                "burn",
                "sleep 30",
                # Generous wall backstop: the CPU guard, not the wall clock, must be what fires.
                timeout=300,
                cpu_timeout=_CPU_BUDGET_S,
            ),
        ),
    )
    previous = os.environ.get(LOG_DIR_ENV)
    os.environ[LOG_DIR_ENV] = str(log_dir)
    try:
        runner = Runner(cfg, max_steps=1, max_cpus=1, cgroups=_OverBudgetCgroups())
        runner.run()
    finally:
        if previous is None:
            os.environ.pop(LOG_DIR_ENV, None)
        else:
            os.environ[LOG_DIR_ENV] = previous

    records = [
        json.loads(line)
        for line in (log_dir / "journal.jsonl").read_text().splitlines()
        if line.strip()
    ]
    breaches = [r for r in records if r.get("event") == "cpu_timeout"]
    assert len(breaches) == 1, f"expected exactly one cpu_timeout record, got {breaches}"
    record: dict[str, str] = breaches[0]
    return record


def test_cpu_breach_records_the_cpu_quantity_it_compared(tmp_path: Path) -> None:
    record = _run_one_step_over_cpu_budget(tmp_path / "evidence")

    # The compared quantity is the CPU reading the guard actually tripped on, not the wall clock.
    assert record["measured_s"] == f"{_FAKE_CPU_USED_S:.3f}"
    assert record["limit_s"] == str(_CPU_BUDGET_S)
    assert record["unit"] == BudgetUnit.CPU_SECONDS.value


def test_cpu_breach_keeps_wall_as_labelled_context_not_as_the_comparison(
    tmp_path: Path,
) -> None:
    record = _run_one_step_over_cpu_budget(tmp_path / "evidence")

    # Wall is still recorded — it is useful — but under a name that says what it is, and it is a
    # different number from the compared quantity. The step slept; it burned no real CPU.
    wall = float(record["wall_elapsed_s"])
    assert wall < _FAKE_CPU_USED_S, (
        "the fake CPU reading must exceed the wall time, or this test cannot tell the two apart"
    )
    assert float(record["measured_s"]) != wall

    # The ambiguous field is GONE. Retaining it would preserve the exact misreading this fixes:
    # an unlabelled seconds figure sitting next to a limit in a different unit.
    assert "elapsed_s" not in record


def test_wall_breach_reports_wall_against_a_wall_limit(tmp_path: Path) -> None:
    log_dir = tmp_path / "evidence"
    cfg = DagConfig(
        steps=(
            # No CPU guard (cpu_timeout stays at the DAG default and the manager is unboxed), so
            # the wall ceiling is the one that fires.
            Step("g", "hang", "hang", "sleep 30", timeout=1),
        ),
    )
    previous = os.environ.get(LOG_DIR_ENV)
    os.environ[LOG_DIR_ENV] = str(log_dir)
    try:
        Runner(cfg, max_steps=1, max_cpus=1, cgroups=NoopCgroups()).run()
    finally:
        if previous is None:
            os.environ.pop(LOG_DIR_ENV, None)
        else:
            os.environ[LOG_DIR_ENV] = previous

    records = [
        json.loads(line)
        for line in (log_dir / "journal.jsonl").read_text().splitlines()
        if line.strip()
    ]
    breaches = [r for r in records if r.get("event") == "step_timeout"]
    assert len(breaches) == 1, f"expected exactly one step_timeout record, got {breaches}"
    record = breaches[0]

    # A wall breach was never wrong; assert it just as hard, so the fix cannot be "label
    # everything cpu_seconds" and still pass.
    assert record["unit"] == BudgetUnit.WALL_SECONDS.value
    assert record["limit_s"] == "1"
    assert float(record["measured_s"]) >= 1.0
    assert float(record["measured_s"]) == float(record["wall_elapsed_s"])
