"""An absent number is not a zero, in two places where the scheduler used to say it was.

A missing value defaulted to 0 reads, downstream, exactly like a real measured 0 — and in both
places below the default silently turned off the very thing it was standing in for.

*A resource cap.*  The readiness gate reads ``resource_avail.get(name, 0)``, which collapses two
conditions whose remedies are opposites: the author FORGOT to declare capacity for a resource a
step demands, versus the author DELIBERATELY capped it at 0 to block the step.  Both produced
byte-identical behaviour — the step never becomes ready, the ready-set loop keeps sleeping, and
the run sits at 0% CPU printing nothing until some outer deadline kills it.

*A CPU counter.*  The CPU-time guard read ``cpu.stat``'s ``usage_usec`` with a default of 0.  A
cgroup that does not publish that counter therefore reported "this step has burned no CPU"
forever, which made the budget comparison permanently unsatisfiable: a declared ``cpu_timeout``
enforced nothing, quietly, with no warning anywhere.

These tests bracket both distinctions BOTH ways.  A one-sided test would pass if the code simply
flagged every resource demand or refused every CPU budget, so the legitimate cases are asserted
just as hard as the broken ones.
"""

from __future__ import annotations

import dataclasses
import time

import pytest

from dagrun import (
    DagConfig,
    ResourceHint,
    Step,
    run_dag,
    undeclared_resource_demands,
)
from dagrun.cgroup import NoopCgroups
from dagrun.model import IntentionalSkipReason
from dagrun.scheduler import _cpu_seconds_from_stats


def _demanding_step(resource: str = "browser", count: int = 1) -> Step:
    return Step(
        "g",
        "needs",
        "wants a scarce resource",
        "true",
        hint=ResourceHint(resources={resource: count}),
    )


def test_an_undeclared_demand_is_named() -> None:
    cfg = DagConfig(steps=(_demanding_step(),))
    assert undeclared_resource_demands(cfg) == ["g.needs: browser"]


def test_a_cap_declared_as_zero_is_a_real_value_and_is_not_named() -> None:
    # Deliberately blocked. The author said what they meant; the gate must honour it silently.
    cfg = DagConfig(steps=(_demanding_step(),), resource_caps={"browser": 0})
    assert undeclared_resource_demands(cfg) == []

    # An ordinary cap is likewise not flagged.
    ample = DagConfig(steps=(_demanding_step(),), resource_caps={"browser": 4})
    assert undeclared_resource_demands(ample) == []


def test_a_zero_demand_and_an_intentionally_skipped_step_cannot_starve() -> None:
    # A demand of 0 is satisfied by the absent-cap default of 0, so it never blocks.
    zero_demand = DagConfig(steps=(_demanding_step(count=0),))
    assert undeclared_resource_demands(zero_demand) == []

    # An intentionally-skipped step is never launched, so its dormant demand cannot hang a run
    # and must not fail one either.
    skipped = Step(
        "g",
        "skipped",
        "never launched",
        "true",
        hint=ResourceHint(resources={"browser": 1}),
        skip_reason=IntentionalSkipReason.EMPTY_MANIFEST_BUCKET,
    )
    assert undeclared_resource_demands(DagConfig(steps=(skipped,))) == []


def test_a_run_with_an_undeclared_demand_is_refused_instead_of_hanging(
    capsys: pytest.CaptureFixture[str],
) -> None:
    step = _demanding_step()
    cfg = DagConfig(steps=(dataclasses.replace(step, timeout=1),))

    # The outer run budget exists so this test cannot HANG when the refusal is removed: without
    # it the ready-set loop sleeps forever and there is no failure to observe, only an
    # unresponsive suite. With it, the un-fixed behaviour is a 3-second run-timeout, which is a
    # visibly different (and worse) report than the refusal.
    started = time.monotonic()
    result = run_dag(cfg, jobs=1, verbosity=0, run_timeout_s=3)
    elapsed = time.monotonic() - started

    assert not result.ok
    assert not result.run_timed_out, (
        "the demand can never be satisfied, so this must be refused up front and named — not "
        "waited out until the run budget expires and blamed on the run"
    )
    assert elapsed < 3.0, "the run should refuse before any node starts, not wait on the gate"
    assert result.outcomes == ()

    message = capsys.readouterr().err
    assert "REFUSING to run before any node starts" in message
    assert "g.needs: browser" in message
    # Say the remedy, and say that the opposite reading is expressible: an author who really does
    # want the step blocked has somewhere to go.
    assert "resource_caps" in message
    assert "set the cap to 0 explicitly" in message


def test_a_run_whose_caps_are_declared_still_runs(
    capsys: pytest.CaptureFixture[str],
) -> None:
    # The refusal must not become a blanket ban on resource demands.
    cfg = DagConfig(steps=(_demanding_step(),), resource_caps={"browser": 1})
    result = run_dag(cfg, jobs=1, verbosity=0)
    assert result.ok
    assert "REFUSING" not in capsys.readouterr().err


class _CountersWithoutUsage(NoopCgroups):
    """A boxed manager whose ``cpu.stat`` publishes everything EXCEPT ``usage_usec``."""

    enabled = True

    def cpu_stats(self, tag: str) -> dict[str, int]:
        return {"nr_periods": 3, "throttled_usec": 0}


def test_an_absent_cpu_counter_is_unmeasurable_not_zero() -> None:
    # The measured case is asserted just as hard, so "always return None" cannot pass.
    assert _cpu_seconds_from_stats({"usage_usec": 2_500_000}) == 2.5
    # A genuine measured zero is still a measurement, and must survive as one.
    assert _cpu_seconds_from_stats({"usage_usec": 0}) == 0.0
    # Absent means CANNOT MEASURE. Reported as 0.0 it would make `>= budget` unsatisfiable and
    # disable the guard for the life of the step.
    assert _cpu_seconds_from_stats({"nr_periods": 3}) is None


def test_an_unenforceable_cpu_budget_says_so_once(
    capsys: pytest.CaptureFixture[str],
) -> None:
    cfg = DagConfig(
        steps=(
            Step("g", "quiet", "burns nothing measurable", "sleep 2.5", cpu_timeout=1, timeout=30),
        ),
    )
    result = run_dag(cfg, jobs=1, cgroups=_CountersWithoutUsage(), verbosity=0)

    # The step is not killed: an unmeasurable budget is unenforceable, not breached.
    assert result.ok

    message = capsys.readouterr().err
    assert "CANNOT be enforced" in message, (
        "a CPU budget that cannot be measured must say so; silently enforcing nothing is the "
        "defect"
    )
    # Once per step, not once per poll: the monitor ticks about twice during this step.
    assert message.count("CANNOT be enforced") == 1


@pytest.mark.parametrize("stage", ["registration", "observation"])
def test_uncontained_cpu_failure_reason_is_reported_once(
    stage: str, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    from dagrun import scheduler
    from dagrun.proccpu import Unavailable

    class RefusingReader:
        def __init__(self, pid: int) -> None:
            if stage == "registration":
                raise Unavailable("injected registration descriptor reserve")

        def seconds(self) -> float:
            raise Unavailable("injected observation member bound")

        def close(self) -> None:
            pass

    monkeypatch.setattr(scheduler, "ProcessGroupCpu", RefusingReader)
    cfg = DagConfig(steps=(Step("g", "quiet", "known refusal", "sleep 2.5", cpu_timeout=1, timeout=30),))
    result = run_dag(cfg, jobs=1, cgroups=NoopCgroups(), verbosity=0)
    assert result.ok
    message = capsys.readouterr().err
    assert message.count("CANNOT be enforced") == 1
    assert f"{stage} failed: injected {stage}" in message
    if stage == "registration":
        assert "identity was not retained" in message
        assert "not retried after waiting starts" in message
    assert "only the wall timeout still applies" in message
