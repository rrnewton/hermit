"""A wall backstop should be derived from the CPU budget, not baked into the graph.

`Step.timeout` used to default to a hardcoded 1800 the moment a document was loaded, which is
exactly the load-sensitive number a derivation exists to remove: it is the same on a laptop and
on a 300-core host, and it tells you nothing about the step. parallel-experiment-runner already
solved this — explicit wall wins, else 3x the CPU budget, else the default — and this brings the
same idiom here rather than inventing a second policy.

Two design questions had to be ANSWERED, not merely implemented, and both are pinned below:

* derive from the DECLARED cpu_timeout, never the canonical (default-filled) one, or every
  undeclared step would silently drop from a 1800-second ceiling to a 30-second one; and
* derive from the PLATFORM-SCALED budget, or the backstop would race the CPU guard on exactly
  the slow platform `cpu_timeout_multiplier` exists for.

A third answer was added after review: the derivation is FLOORED at the default, so it can only
ever move a step's ceiling away from its CPU guard. Unfloored it retimed every already-authored
step that declared a CPU budget, and for anything that blocks — a fetch, a lock wait — wall time
is unbounded relative to CPU time, so three times a small budget is not a hang.
"""

from __future__ import annotations

import dataclasses

from parallel_experiment_runner.model import (
    WALL_CPU_BACKSTOP_FACTOR as PER_WALL_CPU_BACKSTOP_FACTOR,
)
from dagrun.io import dag_from_json, dag_to_json
from dagrun.model import (
    DEFAULT_SMALL_CPU_TIMEOUT,
    DEFAULT_STEP_TIMEOUT,
    WALL_CPU_BACKSTOP_FACTOR,
    DagConfig,
    Step,
    canonical_cpu_timeout,
    resolved_wall_timeout,
)
from dagrun.scheduler import steps_violating_run_timeout


def _step(**kwargs: int) -> Step:
    return Step("g", "a", "d", "true", **kwargs)  # type: ignore[arg-type]


def test_the_factor_is_the_one_the_other_runner_already_established() -> None:
    # Not a tautology: the two constants live in two independent packages and this is the only
    # thing stopping them from drifting into two policies with one name.
    assert WALL_CPU_BACKSTOP_FACTOR == PER_WALL_CPU_BACKSTOP_FACTOR
    assert WALL_CPU_BACKSTOP_FACTOR == 3


def test_an_explicit_step_budget_wins_over_everything() -> None:
    step = _step(timeout=42, cpu_timeout=7)
    assert resolved_wall_timeout(step, 600, 2.0) == 42


def test_a_document_default_wins_over_the_derivation() -> None:
    # An author who wrote a document-wide number said something; the derivation must not
    # second-guess it.
    assert resolved_wall_timeout(_step(cpu_timeout=7), 600) == 600


def test_a_declared_cpu_budget_derives_the_backstop() -> None:
    # 900 CPU-seconds is the case the rule exists for: a baked-in 1800 is only 2x that budget,
    # and the CPU guard can reach it. 2700 restores the 3x margin.
    assert resolved_wall_timeout(_step(cpu_timeout=900), 0) == 2700


def test_a_small_cpu_budget_does_not_retime_an_already_authored_step() -> None:
    # THE REGRESSION THE FLOOR PREVENTS. `{"cmd": "git fetch ...", "cpu_timeout": 5}` burns ~5
    # CPU-seconds and blocks for minutes on the network. Unfloored, rule 3 gives it a 15-second
    # wall ceiling and SIGTERMs it as a hang — a silent retiming of every existing step that
    # declared a CPU budget. Wall time is unbounded relative to CPU time for anything that
    # blocks, so the derivation is allowed to loosen and never to tighten.
    assert resolved_wall_timeout(_step(cpu_timeout=5), 0) != 15
    assert resolved_wall_timeout(_step(cpu_timeout=5), 0) == DEFAULT_STEP_TIMEOUT == 1800
    # A networkonly step is the same story, said by the schema: it is DECLARED to depend on a
    # resource whose latency has nothing to do with its CPU budget.
    net = dataclasses.replace(_step(cpu_timeout=5), networkonly=True)
    assert resolved_wall_timeout(net, 0) == 1800


def test_the_floor_is_exactly_where_the_derivation_overtakes_the_default() -> None:
    # Named literally on both sides of the boundary, so "always return 1800" and "never floor"
    # are each caught by one of these two lines.
    assert resolved_wall_timeout(_step(cpu_timeout=600), 0) == 1800
    assert resolved_wall_timeout(_step(cpu_timeout=601), 0) == 1803


def test_the_derivation_tracks_the_platform_scaled_budget() -> None:
    # 400 CPU-seconds on a platform 2.5x slower is a 1000-second enforced budget, so the wall
    # backstop is 3000 and keeps its 3x margin. Pinned to the DECLARED 400 it would be 1200,
    # which the floor would then round back up to 1800 — BELOW the 1000-second enforced guard's
    # 3x margin, i.e. racing it on exactly the platform the multiplier exists for.
    assert resolved_wall_timeout(_step(cpu_timeout=400), 0, 2.5) == 3000


def test_a_step_that_declared_nothing_keeps_the_1800_second_backstop() -> None:
    # THE ANSWER TO THE OPEN QUESTION. `canonical_cpu_timeout` fills in the small 10-second
    # default for such a step, and deriving from THAT would give it a 30-second wall ceiling.
    step = _step()
    assert canonical_cpu_timeout(step, DEFAULT_SMALL_CPU_TIMEOUT) == 10
    assert resolved_wall_timeout(step, 0) == DEFAULT_STEP_TIMEOUT == 1800


def test_the_sentinel_is_absence_and_round_trips_as_absence() -> None:
    doc = '{"steps": [{"group": "g", "job": "a", "cmd": "true", "cpu_timeout": 7}]}'
    cfg = dag_from_json(doc)
    assert cfg.steps[0].timeout == 0
    assert cfg.default_step_timeout == 0
    emitted = dag_to_json(cfg)
    assert '"timeout"' not in emitted, "0 written out would read as 'no wall bound'"
    assert '"default_step_timeout"' not in emitted
    # Reloading the emission must land on the same model, or the format has lost the sentinel.
    assert dag_from_json(emitted) == cfg
    assert dag_to_json(dag_from_json(emitted)) == emitted


def test_a_declared_budget_still_round_trips_as_a_number() -> None:
    # The other side: omission must not become "always omit".
    doc = '{"steps": [{"group": "g", "job": "a", "cmd": "true", "timeout": 42}]}'
    cfg = dag_from_json(doc)
    assert cfg.steps[0].timeout == 42
    assert '"timeout": 42' in dag_to_json(cfg)


def test_a_document_default_still_round_trips_as_a_number() -> None:
    doc = '{"default_step_timeout": 600, "steps": [{"group": "g", "job": "a", "cmd": "true"}]}'
    cfg = dag_from_json(doc)
    assert cfg.default_step_timeout == 600
    # The loader materializes it into the step, as it always has.
    assert cfg.steps[0].timeout == 600
    emitted = dag_to_json(cfg)
    assert '"default_step_timeout": 600' in emitted
    assert dag_from_json(emitted) == cfg


def test_the_run_budget_ordering_is_checked_on_the_resolved_value() -> None:
    # A 0 sentinel passes `>= run_timeout_s` trivially, so the fail-closed inner-below-outer
    # ordering has to be expressed on the value the step will actually run under.
    step = _step(cpu_timeout=900)  # derives a 2700-second wall backstop
    cfg = DagConfig(steps=(step,))
    assert steps_violating_run_timeout(cfg, 2000) == [("g.a", 2700)]
    assert steps_violating_run_timeout(cfg, 3000) == []


def test_an_undeclared_step_is_still_caught_by_the_run_budget_check() -> None:
    # Its resolved bound is 1800, so a 900-second run budget must still refuse it. Before the
    # resolved value was used, its declared 0 would have sailed through.
    cfg = DagConfig(steps=(_step(),))
    assert steps_violating_run_timeout(cfg, 900) == [("g.a", 1800)]


def test_the_derivation_does_not_quietly_admit_a_graph_the_ordering_check_refused() -> None:
    # The other direction, RESTATED after the floor landed. An unfloored rule 3 would derive a
    # 15-second ceiling for this step and let a 60-second run budget accept a graph that has
    # always been refused — a loosening of a fail-closed pre-flight check, obtained by pretending
    # a network-blocked step cannot outlive 3x its CPU budget. It is still refused, at 1800.
    cfg = DagConfig(steps=(dataclasses.replace(_step(), cpu_timeout=5),))
    assert steps_violating_run_timeout(cfg, 60) == [("g.a", 1800)]
