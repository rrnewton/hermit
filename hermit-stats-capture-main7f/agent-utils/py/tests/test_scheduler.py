"""End-to-end scheduler tests: real subprocess steps verifying observable behavior."""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time

import pytest

from dagrun.cgroup import DELEGATED_CGROUP_ENV, DELEGATED_UNBOXED_ENV, NoopCgroups
from dagrun.attribution import LOG_DIR_ENV
from dagrun.model import (
    DEFAULT_SMALL_CPU_COUNT,
    DEFAULT_SMALL_CPU_TIMEOUT,
    DEFAULT_SMALL_MEM_CAP_BYTES,
    DagConfig,
    IntentionalSkipReason,
    ResourceHint,
    Step,
    StructuredTestResultsManifest,
    graph_structure_violations,
)
from dagrun.protocols import RunResult
from dagrun.scheduler import (
    _ABSENT,
    Runner,
    run_dag,
    steps_violating_run_timeout,
    _ungrantable_resources,
)


def _step(
    group: str,
    job: str,
    cmd: str,
    *,
    deps: list[str] | None = None,
    resources: dict[str, int] | None = None,
    est: float = 0.0,
    skip_reason: IntentionalSkipReason | None = None,
) -> Step:
    return Step(
        group,
        job,
        "",
        cmd,
        deps=list(deps or []),
        hint=ResourceHint(resources=dict(resources or {}), est_duration_s=est),
        skip_reason=skip_reason,
    )


def test_default_config_enables_small_forcing_caps() -> None:
    cfg = DagConfig(steps=())
    assert cfg.default_step_mem_cap_bytes == DEFAULT_SMALL_MEM_CAP_BYTES
    assert cfg.default_step_cpu_count == DEFAULT_SMALL_CPU_COUNT
    assert cfg.default_step_cpu_timeout == DEFAULT_SMALL_CPU_TIMEOUT


@pytest.mark.parametrize(
    "spawn_error",
    (OSError(2, "planted spawn refusal"), ValueError("planted invalid environment")),
    ids=("oserror", "valueerror"),
)
def test_spawn_failure_releases_admission_state_and_returns_without_traceback(
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    spawn_error: Exception,
) -> None:
    failing = Step(
        "g",
        "spawn",
        "spawn refusal",
        "never runs",
        hint=ResourceHint(resources={"slot": 1}, preferred_inner_jobs=2),
    )
    cfg = DagConfig(
        steps=(failing, _step("g", "dependent", "true", deps=[failing.tag])),
        resource_caps={"slot": 1},
    )
    runner = Runner(
        cfg,
        max_steps=1,
        cpu_jobs=2,
        cgroups=NoopCgroups(),
        verbosity=0,
    )

    def refuse_spawn(*_args: object, **_kwargs: object) -> None:
        raise spawn_error

    monkeypatch.setattr(subprocess, "Popen", refuse_spawn)
    started = time.monotonic()
    assert not runner.run()
    assert time.monotonic() - started < 5.0

    result = runner.result()
    assert not result.ok
    assert len(result.outcomes) == 1
    assert result.outcomes[0].tag == failing.tag
    assert "spawn failed" in result.outcomes[0].summary
    assert str(spawn_error) in result.outcomes[0].summary
    assert result.max_concurrent_steps == 0
    assert result.skipped == ("g.dependent",)
    assert runner.running == set()
    assert runner.running_procs == {}
    assert runner.running_nonces == {}
    assert runner.resource_avail == {"slot": 1}
    assert runner.cores_used == 0

    captured = capsys.readouterr()
    combined = captured.out + captured.err
    assert "spawn failed" in combined
    assert "Traceback" not in combined
    assert "Exception in thread" not in combined


def test_spawn_failure_eager_aborts_an_inflight_sibling() -> None:
    slow = _step("g", "slow", "sleep 10", est=100.0)
    gate = _step("g", "gate", "sleep 0.3", est=50.0)
    invalid = Step(
        "g",
        "invalid-env",
        "",
        "true",
        deps=[gate.tag],
        env={"BAD": "embedded\0nul"},
    )
    started = time.monotonic()
    result = run_dag(DagConfig(steps=(slow, gate, invalid)), jobs=2, verbosity=0)
    elapsed = time.monotonic() - started

    assert not result.ok
    assert elapsed < 5.0, "spawn failure should eager-cancel the ten-second sibling"
    outcomes = {outcome.tag: outcome for outcome in result.outcomes}
    assert outcomes[slow.tag].aborted
    assert outcomes[gate.tag].ok
    assert "spawn failed" in outcomes[invalid.tag].summary


def test_simple_dag_all_pass_respects_deps() -> None:
    with tempfile.TemporaryDirectory() as d:
        order = os.path.join(d, "order")
        cfg = DagConfig(
            steps=(
                _step("g", "A", f"echo A >> {order}"),
                _step("g", "B", f"echo B >> {order}", deps=["g.A"]),
            )
        )
        res = run_dag(cfg, jobs=4, verbosity=0)
        assert res.ok
        assert {o.tag for o in res.outcomes} == {"g.A", "g.B"}
        assert all(o.ok for o in res.outcomes)
        with open(order, encoding="utf-8") as fh:
            assert fh.read().split() == ["A", "B"]  # dependent runs strictly after its dep


def test_delegation_is_exported_only_to_the_opted_in_step(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    class DelegatingManager(NoopCgroups):
        enabled = True

        def prepare_delegated_command(
            self,
            tag: str,
            cmd: str,
            mem_max: int | None = None,
            cpu_count: int | None = None,
        ) -> tuple[str, str | None]:
            return cmd, "/verified/outer-step-root"

    delegated = tmp_path / "delegated"
    ordinary = tmp_path / "ordinary"
    monkeypatch.setenv(DELEGATED_CGROUP_ENV, "/ambient-value-must-not-leak")
    cfg = DagConfig(
        steps=(
            Step(
                "g",
                "delegated",
                "",
                f"printf '%s' \"${{{DELEGATED_CGROUP_ENV}}}\" > {delegated}",
                delegated_children=True,
            ),
            Step(
                "g",
                "ordinary",
                "",
                f"printf '%s' \"${{{DELEGATED_CGROUP_ENV}-absent}}\" > {ordinary}",
            ),
        )
    )
    result = run_dag(cfg, jobs=2, verbosity=0, cgroups=DelegatingManager())
    assert result.ok
    assert delegated.read_text() == "/verified/outer-step-root"
    assert ordinary.read_text() == "absent"


def test_unboxed_delegation_fallback_is_exported_only_to_the_opted_in_step(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    delegated = tmp_path / "delegated"
    ordinary = tmp_path / "ordinary"
    monkeypatch.setenv(DELEGATED_UNBOXED_ENV, "ambient-value-must-not-leak")
    cfg = DagConfig(
        steps=(
            Step(
                "g",
                "delegated",
                "",
                f"printf '%s' \"${{{DELEGATED_UNBOXED_ENV}-absent}}\" > {delegated}",
                delegated_children=True,
            ),
            Step(
                "g",
                "ordinary",
                "",
                f"printf '%s' \"${{{DELEGATED_UNBOXED_ENV}-absent}}\" > {ordinary}",
            ),
        )
    )
    result = run_dag(cfg, jobs=2, verbosity=0, cgroups=NoopCgroups())
    assert result.ok
    assert delegated.read_text() == "1"
    assert ordinary.read_text() == "absent"


def test_dep_failure_skips_dependent() -> None:
    cfg = DagConfig(
        steps=(
            _step("g", "A", "exit 1"),
            _step("g", "B", "true", deps=["g.A"]),
        )
    )
    res = run_dag(cfg, jobs=2, verbosity=0)
    assert not res.ok
    outcomes = {o.tag: o for o in res.outcomes}
    assert outcomes["g.A"].ok is False
    assert "g.B" in res.skipped
    assert "g.B" not in outcomes  # a skipped step never produces an outcome


def test_intentional_skip_never_spawns_and_nonempty_peer_runs() -> None:
    with tempfile.TemporaryDirectory() as d:
        forbidden = os.path.join(d, "forbidden")
        ran = os.path.join(d, "ran")
        cfg = DagConfig(
            steps=(
                _step(
                    "g",
                    "empty",
                    f"touch {forbidden}",
                    skip_reason=IntentionalSkipReason.EMPTY_MANIFEST_BUCKET,
                ),
                _step("g", "nonempty", f"touch {ran}"),
            )
        )
        res = run_dag(cfg, jobs=2, verbosity=0)
        assert res.ok
        assert not os.path.exists(forbidden)
        assert os.path.exists(ran)
        assert res.intentional_skips == (
            ("g.empty", IntentionalSkipReason.EMPTY_MANIFEST_BUCKET),
        )
        assert res.skipped == ()
        assert [outcome.tag for outcome in res.outcomes] == ["g.nonempty"]


def test_eager_exit_aborts_inflight_step() -> None:
    cfg = DagConfig(
        steps=(
            _step("g", "fast", "sleep 0.2; exit 1", est=0.0),
            _step("g", "slow", "sleep 5", est=100.0),
        )
    )
    res = run_dag(cfg, jobs=2, verbosity=0)
    assert not res.ok
    outcomes = {o.tag: o for o in res.outcomes}
    assert outcomes["g.fast"].ok is False
    assert outcomes["g.slow"].aborted is True  # eager-exit cancelled the in-flight step
    assert outcomes["g.slow"].ok is False


def test_fail_fast_reports_independent_step_as_not_launched() -> None:
    """Under the default eager-exit, later independent work lands in NO existing bucket.

    That is the coverage hole this feature is about: ``g.independent`` is neither an outcome nor
    dependency-skipped, so a caller counting only those two would read the run as fully accounted
    for. ``not_launched`` names it.
    """
    cfg = DagConfig(
        steps=(
            _step("g", "fail", "exit 1", est=100.0),
            _step("g", "dependent", "true", deps=["g.fail"], est=90.0),
            _step("g", "independent", "true", est=80.0),
        )
    )
    res = run_dag(cfg, jobs=1, keep_going=False, verbosity=0)
    assert not res.ok
    assert [o.tag for o in res.outcomes] == ["g.fail"]
    assert res.skipped == ("g.dependent",)
    assert res.not_launched == ("g.independent",)


def test_journal_uses_boolean_verdicts_and_accounts_for_unrun_steps(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The durable journal alone must expose failure and the whole planned population."""
    log_dir = tmp_path / "journal"
    monkeypatch.setenv(LOG_DIR_ENV, str(log_dir))
    cfg = DagConfig(
        steps=(
            _step("g", "fail", "exit 1", est=100.0),
            _step("g", "dependent", "true", deps=["g.fail"], est=90.0),
            _step("g", "independent", "true", est=80.0),
            _step(
                "g",
                "intentional",
                "exit 99",
                skip_reason=IntentionalSkipReason.EMPTY_MANIFEST_BUCKET,
            ),
        )
    )

    result = run_dag(cfg, jobs=1, keep_going=False, verbosity=0)
    assert not result.ok
    records = [
        json.loads(line)
        for line in (log_dir / "journal.jsonl").read_text().splitlines()
        if line.strip()
    ]
    ends = [row for row in records if row.get("event") == "step_end"]
    assert len(ends) == 1
    assert ends[0]["step"] == "g.fail"
    assert ends[0]["ok"] is False
    assert type(ends[0]["ok"]) is bool

    skips = {
        row["step"]: row["reason"]
        for row in records
        if row.get("event") == "step_skip"
    }
    assert skips == {
        "g.dependent": "dependency_failed",
        "g.independent": "not_launched",
        "g.intentional": "empty-manifest-bucket",
    }
    assert {row["step"] for row in ends} | set(skips) == {step.tag for step in cfg.steps}


def test_scoped_eager_exit_cancels_its_family_and_completes_an_independent_family() -> None:
    fail = _step("family", "fail", "sleep 0.2; exit 1", est=100.0)
    peer = _step("family", "peer", "sleep 5", est=90.0)
    independent = _step("independent", "ok", "sleep 0.5; true", est=80.0)
    dependent = _step("family", "dependent", "true", deps=[fail.tag], est=70.0)
    fail.fail_fast_family = "family-a"
    peer.fail_fast_family = "family-a"
    dependent.fail_fast_family = "family-a"
    independent.fail_fast_family = "family-b"
    cfg = DagConfig(steps=(fail, peer, independent, dependent))

    res = run_dag(cfg, jobs=3, keep_going=False, verbosity=0)
    outcomes = {outcome.tag: outcome for outcome in res.outcomes}

    assert not res.ok
    assert outcomes[fail.tag].ok is False and outcomes[fail.tag].aborted is False
    assert outcomes[peer.tag].aborted is True, "the failed family must still be cancelled"
    assert res.skipped == (dependent.tag,), "a true dependent must remain dependency-skipped"
    assert outcomes[independent.tag].ok is True
    assert outcomes[independent.tag].aborted is False, "an independent family must complete"


def test_scoped_eager_exit_does_not_launch_a_queued_family_peer() -> None:
    fail = _step("family", "fail", "exit 1", est=100.0)
    peer = _step("family", "queued", "true", est=90.0)
    independent = _step("independent", "ok", "true", est=80.0)
    fail.fail_fast_family = "family-a"
    peer.fail_fast_family = "family-a"
    independent.fail_fast_family = "family-b"
    cfg = DagConfig(steps=(fail, peer, independent))

    res = run_dag(cfg, jobs=1, keep_going=False, verbosity=0)
    outcomes = {outcome.tag: outcome for outcome in res.outcomes}

    assert not res.ok
    assert outcomes[independent.tag].ok is True
    assert peer.tag not in outcomes
    assert res.not_launched == (peer.tag,)


def test_keep_going_launches_independent_step_after_failure() -> None:
    cfg = DagConfig(
        steps=(
            _step("g", "fail", "exit 1", est=100.0),
            _step("g", "dependent", "true", deps=["g.fail"], est=90.0),
            _step("g", "independent", "true", est=80.0),
        )
    )
    res = run_dag(cfg, jobs=1, keep_going=True, verbosity=0)
    assert not res.ok  # the genuine failure still fails the run
    outcomes = {o.tag: o for o in res.outcomes}
    assert outcomes["g.fail"].ok is False
    assert outcomes["g.independent"].ok is True  # launched AFTER the failure
    assert res.skipped == ("g.dependent",)  # a true dependent is still not run
    assert res.not_launched == ()
    # Every configured step is now accounted for in exactly one bucket.
    assert len(res.outcomes) + len(res.skipped) + len(res.intentional_skips) == len(
        cfg.steps
    )


def test_keep_going_collects_every_independent_failure_in_one_run() -> None:
    """The point of the option: one run, every independent failure, not just the first."""
    cfg = DagConfig(
        steps=(
            _step("g", "fail_a", "exit 1", est=100.0),
            _step("g", "fail_b", "exit 1", est=90.0),
            _step("g", "fail_c", "exit 1", est=80.0),
        )
    )
    res = run_dag(cfg, jobs=1, keep_going=True, verbosity=0)
    assert not res.ok
    assert sorted(o.tag for o in res.outcomes if not o.ok and not o.aborted) == [
        "g.fail_a",
        "g.fail_b",
        "g.fail_c",
    ]
    assert res.not_launched == ()


def test_resource_cap_serializes_concurrent_steps() -> None:
    with tempfile.TemporaryDirectory() as d:
        log = os.path.join(d, "intervals")

        def cmd(tag: str) -> str:
            return (
                f"echo S {tag} $(date +%s.%N) >> {log}; "
                f"sleep 0.3; "
                f"echo E {tag} $(date +%s.%N) >> {log}"
            )

        cfg = DagConfig(
            steps=(
                _step("g", "one", cmd("one"), resources={"slot": 1}, est=1.0),
                _step("g", "two", cmd("two"), resources={"slot": 1}, est=1.0),
            ),
            resource_caps={"slot": 1},
        )
        res = run_dag(cfg, jobs=4, verbosity=0)
        assert res.ok
        starts: dict[str, float] = {}
        ends: dict[str, float] = {}
        with open(log, encoding="utf-8") as fh:
            for line in fh:
                kind, tag, ts = line.split()
                (starts if kind == "S" else ends)[tag] = float(ts)
        # A cap of 1 must serialize the two: their run intervals cannot overlap.
        assert ends["one"] <= starts["two"] or ends["two"] <= starts["one"]


def _timed_step(job: str, cmd: str, timeout: int) -> Step:
    """A step with an explicit wall budget (the INNER bound of the two-level ordering)."""
    return Step("a", job, "", cmd, deps=[], timeout=timeout, cpu_timeout=600)


def test_outer_run_budget_cuts_a_long_run_early_and_still_reports() -> None:
    """A run longer than its outer budget is stopped BY THE RUNNER and still returns rows.

    Per-step budgets cannot bound a run: any number of individually-legal steps can sum past any
    ceiling. Before an outer bound existed the only thing that stopped such a run was an external
    job kill, and an external kill discards the logs that would explain why it was needed. So the
    bound that fires first must be one the scheduler enforces on itself.
    """
    cfg = DagConfig(
        steps=(
            _timed_step("one", "sleep 4", 5),
            _timed_step("two", "sleep 4", 5),
            _timed_step("three", "sleep 4", 5),
        )
    )
    started = time.time()
    res = run_dag(cfg, jobs=1, verbosity=0, run_timeout_s=6)
    elapsed = time.time() - started

    assert res.ok is False
    assert res.run_timed_out is True
    # EARLY is the load-bearing word: ~12s would mean the budget did nothing.
    assert elapsed < 10.0, f"expected a cut near 6s, took {elapsed:.1f}s"
    # The evidence survives, which is the whole difference from an outside kill.
    assert len(res.step_profile_rows) == 2
    outcomes = {o.tag: o for o in res.outcomes}
    assert outcomes["a.one"].ok is True
    assert outcomes["a.two"].aborted is True


def test_misordered_budgets_are_refused_before_anything_runs() -> None:
    """A step allowed to outlive the run is refused: its breach could not be attributed."""
    cfg = DagConfig(steps=(_timed_step("wide", "sleep 1", 600),))
    started = time.time()
    res = run_dag(cfg, jobs=1, verbosity=0, run_timeout_s=6)
    elapsed = time.time() - started

    assert res.ok is False
    assert res.run_timed_out is False  # nothing ran, so nothing timed out
    assert res.outcomes == ()
    assert elapsed < 3.0, f"the refusal must precede execution, took {elapsed:.1f}s"
    assert steps_violating_run_timeout(cfg, 6) == [("a.wide", 600)]


def test_clean_run_inside_its_outer_budget_is_untouched() -> None:
    """The control. A fix that only shows the positive would cut every run short."""
    cfg = DagConfig(
        steps=(
            _timed_step("one", "sleep 1", 10),
            _timed_step("two", "sleep 1", 10),
        )
    )
    res = run_dag(cfg, jobs=1, verbosity=0, run_timeout_s=60)
    assert res.ok is True
    assert res.run_timed_out is False
    assert all(not o.aborted for o in res.outcomes)
    assert len(res.step_profile_rows) == 2


# ----------------------------------------------------------- terminal-starve brackets
#
# Both directions, with counts, because each leg alone is passed by a broken guard: a detector
# that refuses EVERY run passes the negative legs, and a detector wired to nothing passes the
# positive leg.
#
# Every negative leg goes through `_run_dag_bounded`, never `run_dag` directly. The defect these
# guard against is an INFINITE SLEEP, and a test that hangs reports nothing at all -- it stalls
# the suite instead of failing it. Bounding the call converts the regression into a named red.


def _run_dag_bounded(cfg: DagConfig, *, jobs: int = 4, budget_s: float = 60.0) -> RunResult:
    """Run ``cfg`` on a daemon thread and FAIL if it has not returned within ``budget_s``."""
    box: list[RunResult] = []
    worker = threading.Thread(
        target=lambda: box.append(run_dag(cfg, jobs=jobs, verbosity=0)), daemon=True
    )
    worker.start()
    worker.join(budget_s)
    if worker.is_alive():
        pytest.fail(
            f"run_dag did not return within {budget_s}s: the terminal-starve detector has "
            "regressed and the scheduler is sleeping on a state no future event can change"
        )
    assert box, "the worker exited without producing a RunResult"
    return box[0]


def test_positive_satisfiable_caps_yield_no_refusal_and_the_dag_still_runs() -> None:
    a = _step("g", "a", "true", resources={"hg": 1})
    b = _step("g", "b", "true", resources={"hg": 1})
    cfg = DagConfig(steps=(a, b), resource_caps={"hg": 1})
    assert (
        _ungrantable_resources({"hg": 1}, {a.tag: a, b.tag: b}, [a.tag, b.tag]) == []
    ), "positive: a satisfiable demand must produce ZERO refusals"
    res = _run_dag_bounded(cfg)
    assert res.ok is True, "positive: a satisfiable DAG must still run green"
    assert len(res.outcomes) == 2
    assert list(res.skipped) == []


def test_absent_cap_reads_differently_from_a_declared_zero() -> None:
    # Same BEHAVIOR (both block), different DIAGNOSTIC. `.get(r, 0)` alone makes these two
    # literally indistinguishable, which is the reason the `<absent>` token exists.
    needs_gpu = _step("g", "needs_gpu", "true", resources={"gpu": 1})
    steps = {needs_gpu.tag: needs_gpu}

    missing = _ungrantable_resources({"hg": 4}, steps, [needs_gpu.tag])
    assert len(missing) == 1, "absent: exactly 1 refusal"
    assert _ABSENT in missing[0], f"absent must render distinctly: {missing[0]}"
    assert "gpu=1" in missing[0], f"must name the demand: {missing[0]}"
    assert "hg=4" in missing[0], f"must show what WAS declared: {missing[0]}"

    zero = _ungrantable_resources({"gpu": 0}, steps, [needs_gpu.tag])
    assert len(zero) == 1, "declared zero: exactly 1 refusal"
    assert "gpu=0" in zero[0], f"a declared 0 must show its value: {zero[0]}"
    assert _ABSENT not in zero[0], f"a declared 0 must NOT read as absent: {zero[0]}"


def test_ungrantable_cap_refuses_before_anything_runs(
    capsys: pytest.CaptureFixture[str],
) -> None:
    # A DECLARED cap that is too small, deliberately — not an undeclared resource.
    #
    # This USED to be owned by the terminal-starve detector, which could only see it after every
    # unrelated step had already run: "the satisfiable step still ran" was the pinned behaviour,
    # and it meant a full build was spent before the graph was called broken.
    # `graph_structure_violations` sees it in pre-flight, from the document alone, so NOTHING runs.
    cfg = DagConfig(
        steps=(
            _step("g", "needs_gpu", "true", resources={"gpu": 4}),
            _step("g", "plain", "true"),
        ),
        resource_caps={"gpu": 1},
    )
    res = _run_dag_bounded(cfg)
    assert res.ok is False, "an ungrantable demand must REFUSE, not hang"
    assert len(res.outcomes) == 0, "pre-flight refused, so NOTHING may have run"
    err = capsys.readouterr().err
    assert "REFUSING to run before any node starts" in err, err
    assert "demands gpu=4 but resource_caps declares gpu=1" in err, err


def test_python_runner_refuses_structured_results_before_any_node_starts(
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
) -> None:
    marker = tmp_path / "ran"
    producer = _step("test", "counts", f"touch {marker}")
    producer.result_manifests = [
        StructuredTestResultsManifest.current("test.counts")
    ]
    cfg = DagConfig(steps=(producer,))

    result = _run_dag_bounded(cfg)

    assert result.ok is False
    assert result.outcomes == ()
    assert not marker.exists(), "the unsupported Python path must refuse before execution"
    error = capsys.readouterr().err
    assert "REFUSING to run before any node starts" in error
    assert "Python runner does not implement structured test-result capture" in error
    assert "test.counts" in error


def test_a_cap_declared_as_zero_stays_the_deliberate_block_the_guide_documents(
    capsys: pytest.CaptureFixture[str],
) -> None:
    """The boundary of the pre-flight check, asserted from the other side.

    A cap of exactly 0 is documented as "blocked on purpose", so it is NOT a pre-flight error;
    the terminal-starve detector still owns it, which is why that detector is not dead code.
    Without this case, "refuse every demand" would pass the test above.
    """
    cfg = DagConfig(
        steps=(
            _step("g", "blocked", "true", resources={"gpu": 1}),
            _step("g", "plain", "true"),
        ),
        resource_caps={"gpu": 0},
    )
    assert graph_structure_violations(cfg) == []
    res = _run_dag_bounded(cfg)
    assert res.ok is False, "the blocked step still cannot run"
    assert len(res.outcomes) == 1, (
        "and the starve is still found the late way, by the detector that owns it"
    )
    err = capsys.readouterr().err
    assert "terminal starve" in err, err
    assert "starved step(s) (1): g.blocked" in err, err


def test_an_undeclared_resource_is_refused_before_anything_runs(
    capsys: pytest.CaptureFixture[str],
) -> None:
    """The overlapping case, pinned to the EARLIER mechanism so the two cannot silently swap.

    Recovered from two different abandoned branches that each addressed the same silent-hang
    defect. Pre-flight wins: nothing runs, so nothing has written partial state by the time the
    operator is told.
    """
    cfg = DagConfig(
        steps=(
            _step("g", "needs_gpu", "true", resources={"gpu": 1}),
            _step("g", "plain", "true"),
        ),
        resource_caps={"hg": 4},
    )
    res = _run_dag_bounded(cfg)
    assert res.ok is False
    assert len(res.outcomes) == 0, "pre-flight refused, so NOTHING may have run"
    err = capsys.readouterr().err
    assert "REFUSING to run before any node starts" in err, err
    assert "g.needs_gpu: gpu" in err, err


def test_dependency_cycle_refuses_before_anything_runs_and_names_the_cycle() -> None:
    # A cycle is not merely a starve: the planner's bottom-level walk recurses along it until the
    # interpreter raises RecursionError (the Rust edition ABORTS WITH A CORE DUMP). So it cannot
    # be left to a detector that runs after the acyclic steps have gone; it is refused up front,
    # and the refusal NAMES the cycle, because "there is a cycle" in a large graph is not
    # actionable.
    cfg = DagConfig(
        steps=(
            _step("g", "a", "true", deps=["g.b"]),
            _step("g", "b", "true", deps=["g.a"]),
            _step("g", "ok", "true"),
        )
    )
    assert graph_structure_violations(cfg) == ["dependency cycle: g.a -> g.b -> g.a"]
    res = _run_dag_bounded(cfg)
    assert res.ok is False, "a cycle must REFUSE, not hang and not crash"
    assert len(res.outcomes) == 0, "pre-flight refused, so NOTHING may have run"


def test_dangling_dep_refuses_instead_of_sleeping_forever() -> None:
    cfg = DagConfig(steps=(_step("g", "a", "true", deps=["g.nonexistent"]),))
    res = _run_dag_bounded(cfg)
    assert res.ok is False, "a dangling dep must REFUSE, not hang"
    assert list(res.outcomes) == []
