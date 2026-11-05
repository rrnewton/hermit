"""Per-platform CPU-budget multiplier: one canonical graph, platform-scaled enforcement.

WHY THIS EXISTS. A CPU second is load-immune but NOT clock-immune, so identical work burns more
CPU-seconds on an underpowered hosted runner than on a fast dev box. Without scaling, one
canonical `cpu_timeout` is either too loose locally (hiding the hangs it exists to catch) or too
tight on the slow platform (false kills). The wrong fix is a second column of pre-multiplied
numbers: a step has exactly ONE `cpu_timeout` field, so a per-platform column forces the
declaration author to pick one of the two bad numbers, and two timeout tables drift.

So the multiplier is applied at EXECUTION time, and every assertion below is about keeping that
property honest:
  * 1.0 is a STRICT no-op (nothing changes for a platform that never opts in),
  * a disabled budget stays disabled and a scaled budget never rounds down to 0
    (scaling must never silently become an opt-out),
  * a breach names the canonical budget, the multiplier and the platform
    (a kill must stay attributable to a policy, not an anonymous number),
  * a bad multiplier is REFUSED, not ignored (a typo must not silently loosen enforcement).
"""

from __future__ import annotations

import pytest

from dagrun.model import (
    CPU_TIMEOUT_MULTIPLIER_ENV,
    CPU_TIMEOUT_PLATFORM_ENV,
    DEFAULT_CPU_TIMEOUT_MULTIPLIER,
    DEFAULT_SMALL_CPU_TIMEOUT,
    DagConfig,
    Step,
    canonical_cpu_timeout,
    effective_cpu_timeout,
    resolve_cpu_timeout_multiplier,
    scale_cpu_timeout,
    step_failure_reason,
)
from dagrun.protocols import StepOutcome


def step(cpu_timeout: int = 0) -> Step:
    return Step(group="g", job="j", desc="d", cmd="true", cpu_timeout=cpu_timeout)


def breach(
    *,
    returncode: int | None = -9,
    oomed: bool = False,
    oom_kills: int = 0,
    timed_out: bool = False,
    timeout: int = 600,
    pids_guard_tripped: bool = False,
    pids_guard_reason: str | None = None,
    detail_write_failure: tuple[str, ...] = (),
    cpu_timed_out: bool = True,
    cpu_timeout: int = 30,
    cpu_timeout_canonical: int = 0,
    cpu_timeout_multiplier: float = DEFAULT_CPU_TIMEOUT_MULTIPLIER,
    cpu_timeout_platform: str = "",
) -> str:
    return step_failure_reason(
        returncode=returncode,
        oomed=oomed,
        oom_kills=oom_kills,
        timed_out=timed_out,
        timeout=timeout,
        pids_guard_tripped=pids_guard_tripped,
        pids_guard_reason=pids_guard_reason,
        detail_write_failure=detail_write_failure,
        cpu_timed_out=cpu_timed_out,
        cpu_timeout=cpu_timeout,
        cpu_timeout_canonical=cpu_timeout_canonical,
        cpu_timeout_multiplier=cpu_timeout_multiplier,
        cpu_timeout_platform=cpu_timeout_platform,
    )


def test_step_outcome_retains_every_failure_fact_hidden_by_reason_precedence() -> None:
    outcome = StepOutcome.failed(
        "g.j",
        duration_s=1.0,
        summary="",
        returncode=-9,
        oomed=True,
        oom_kills=3,
        timed_out=True,
        timeout=30,
        pids_guard_tripped=False,
        pids_guard_reason=None,
        detail_write_failure=(),
        cpu_timed_out=True,
        cpu_timeout=10,
        cpu_timeout_canonical=10,
    )
    assert outcome.reason.startswith("OOM-KILLED")
    assert outcome.oomed
    assert outcome.oom_kills == 3
    assert outcome.timed_out
    assert outcome.cpu_timed_out


class TestUnityIsAStrictNoOp:
    """A platform that never opts in must behave exactly as it did before this mechanism."""

    def test_default_multiplier_is_one(self) -> None:
        assert DEFAULT_CPU_TIMEOUT_MULTIPLIER == 1.0
        assert DagConfig(steps=()).cpu_timeout_multiplier == 1.0
        assert DagConfig(steps=()).cpu_timeout_platform == ""

    def test_effective_equals_canonical_without_a_multiplier(self) -> None:
        s = step(cpu_timeout=30)
        assert canonical_cpu_timeout(s, DEFAULT_SMALL_CPU_TIMEOUT) == 30
        assert effective_cpu_timeout(s, DEFAULT_SMALL_CPU_TIMEOUT) == 30

    def test_undeclared_step_still_gets_the_small_default(self) -> None:
        assert effective_cpu_timeout(step(), DEFAULT_SMALL_CPU_TIMEOUT) == DEFAULT_SMALL_CPU_TIMEOUT

    def test_breach_message_is_unchanged_at_unity(self) -> None:
        assert breach(cpu_timeout=30, cpu_timeout_canonical=30) == "CPU-TIMEOUT >30s cpu"


class TestScaling:
    def test_declared_budget_scales(self) -> None:
        s = step(cpu_timeout=30)
        assert effective_cpu_timeout(s, DEFAULT_SMALL_CPU_TIMEOUT, 2.0) == 60
        assert effective_cpu_timeout(s, DEFAULT_SMALL_CPU_TIMEOUT, 1.5) == 45

    def test_the_small_default_scales_too(self) -> None:
        # The forcing-function floor is a budget like any other; a slow platform must not
        # false-kill undeclared steps that the fast platform tolerates.
        assert effective_cpu_timeout(step(), 10, 2.0) == 20

    def test_scaling_rounds_to_whole_seconds(self) -> None:
        # Enforcement polls at 1 Hz, so sub-second precision is not meaningful.
        assert scale_cpu_timeout(3, 1.5) == 5  # 4.5 -> 5
        assert scale_cpu_timeout(7, 1.5) == 11  # 10.5 -> 11 (round-half-even would give 10)


class TestScalingCannotBecomeAnOptOut:
    """The dangerous direction. Scaling must never turn enforcement OFF."""

    def test_a_disabled_budget_stays_disabled(self) -> None:
        # canonical 0 means "no CPU guard"; multiplying must not invent one.
        assert scale_cpu_timeout(0, 2.0) == 0
        assert effective_cpu_timeout(step(cpu_timeout=0), 0, 2.0) == 0

    def test_a_live_budget_never_rounds_down_to_zero(self) -> None:
        # A sub-unity multiplier on a small budget would round to 0 = guard silently removed.
        assert scale_cpu_timeout(3, 0.1) == 1
        assert scale_cpu_timeout(1, 0.01) == 1

    def test_a_negative_canonical_is_treated_as_disabled_not_negative(self) -> None:
        assert scale_cpu_timeout(-5, 2.0) == 0


class TestBreachAttribution:
    """A kill under a scaled budget must be traceable to the policy that scaled it."""

    def test_scaled_breach_names_canonical_multiplier_and_platform(self) -> None:
        message = breach(
            cpu_timeout=60,
            cpu_timeout_canonical=30,
            cpu_timeout_multiplier=2.0,
            cpu_timeout_platform="github-hosted",
        )
        assert message == "CPU-TIMEOUT >60s cpu (canonical 30s x2 github-hosted)"

    def test_the_enforced_number_still_leads(self) -> None:
        # Keep the historical `CPU-TIMEOUT >Ns cpu` prefix: other tooling greps for it.
        assert breach(
            cpu_timeout=60, cpu_timeout_canonical=30, cpu_timeout_multiplier=2.0
        ).startswith("CPU-TIMEOUT >60s cpu")

    def test_platform_label_is_optional(self) -> None:
        assert breach(
            cpu_timeout=45, cpu_timeout_canonical=30, cpu_timeout_multiplier=1.5
        ) == "CPU-TIMEOUT >45s cpu (canonical 30s x1.5)"

    def test_oom_still_outranks_a_scaled_cpu_timeout(self) -> None:
        # Precedence is cross-language load-bearing; scaling must not perturb it.
        assert breach(
            oomed=True,
            oom_kills=2,
            cpu_timeout=60,
            cpu_timeout_canonical=30,
            cpu_timeout_multiplier=2.0,
        ).startswith("OOM-KILLED")


class TestResolution:
    def test_no_configuration_resolves_to_unity(self) -> None:
        assert resolve_cpu_timeout_multiplier(None, {}) == (1.0, "")

    def test_environment_sets_the_platform_policy(self) -> None:
        assert resolve_cpu_timeout_multiplier(
            None,
            {CPU_TIMEOUT_MULTIPLIER_ENV: "2", CPU_TIMEOUT_PLATFORM_ENV: "github-hosted"},
        ) == (2.0, "github-hosted")

    def test_explicit_value_beats_the_environment(self) -> None:
        value, _ = resolve_cpu_timeout_multiplier(1.5, {CPU_TIMEOUT_MULTIPLIER_ENV: "2"})
        assert value == 1.5

    @pytest.mark.parametrize("raw", ["nonsense", "2x", ""])
    def test_a_malformed_environment_value_is_refused_not_ignored(self, raw: str) -> None:
        # Silently falling back to 1.0 on a typo would loosen enforcement invisibly — the exact
        # failure mode this mechanism exists to prevent. Empty is the one benign case.
        env = {CPU_TIMEOUT_MULTIPLIER_ENV: raw}
        if raw == "":
            assert resolve_cpu_timeout_multiplier(None, env) == (1.0, "")
        else:
            with pytest.raises(ValueError):
                resolve_cpu_timeout_multiplier(None, env)

    @pytest.mark.parametrize("raw", ["0", "-1"])
    def test_a_nonpositive_environment_value_is_refused(self, raw: str) -> None:
        with pytest.raises(ValueError):
            resolve_cpu_timeout_multiplier(None, {CPU_TIMEOUT_MULTIPLIER_ENV: raw})

    def test_a_nonpositive_explicit_value_is_refused(self) -> None:
        with pytest.raises(ValueError):
            resolve_cpu_timeout_multiplier(0.0, {})


class TestTheGraphStaysCanonical:
    """The multiplier is caller/platform policy, not a property of the pipeline."""

    def test_the_step_field_is_never_rewritten(self) -> None:
        s = step(cpu_timeout=30)
        assert effective_cpu_timeout(s, DEFAULT_SMALL_CPU_TIMEOUT, 2.0) == 60
        # The declared number is untouched, so re-serializing the graph cannot leak a
        # platform-specific value back into the canonical table.
        assert s.cpu_timeout == 30
