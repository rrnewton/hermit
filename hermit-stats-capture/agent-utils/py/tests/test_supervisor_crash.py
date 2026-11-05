"""A step supervisor that dies must FAIL LOUDLY, never wedge the run.

#80 runner-supervisor-crash-loud. Every test here has a deadline, because the defect being
pinned is a HANG: a regression must report as a failed assertion inside the deadline rather
than wedging the suite it lives in.
"""

from __future__ import annotations

import json
import threading
import time
from pathlib import Path

import pytest

from dagrun.attribution import RunEvidence
from dagrun.cgroup import NoopCgroups
from dagrun.model import DagConfig, ResourceHint, Step
from dagrun.scheduler import Runner

#: Wall budget for one whole in-test run. Every step below is `true` or a 0.1s sleep, so a
#: healthy run finishes in well under a second; anything approaching this is the wedge.
#:
#: DELIBERATELY GENEROUS, and the reason matters because the obvious instinct is to tighten it.
#: This bound exists to tell FINITE from INFINITE — a wedged scheduler waits on an outcome that is
#: never published, so it does not take 30s, it takes forever. It does not exist to tell fast from
#: slow. A tight bound therefore buys nothing against the failure it guards (a wedge fails either
#: way, just sooner) while buying a false positive every time the host is busy.
#:
#: It was 20.0, and was raised on the theory that the failures seen under a full `make validate`
#: were a loaded machine. **That theory was wrong**, and the correction matters more than the
#: number: measured standalone on an idle machine, this test wedges at roughly one run in fifteen
#: to twenty, hitting the deadline exactly and reporting the assertion below. It is not slow, it
#: hangs — `runner.run()` never returns.
#:
#: So the deadline is doing its job and is not the thing to adjust. What remains is a real,
#: low-rate race in the scheduler, reachable only through the retire-then-crash window this test
#: opens: layer one publishes the outcome (the CRASHED traceback appears every time, including on
#: the failing runs), and yet the ready-set loop does not observe it and terminate. Reproducing it
#: outside pytest has not succeeded in 40 consecutive direct runs, and disabling capture does not
#: prevent it, so neither the harness nor output capture is the mechanism.
#:
#: Do not raise this bound again to make the failure rarer. A gate that hangs one run in twenty is
#: the defect; the number here only decides how long it takes to say so.
_DEADLINE_S = 120.0


def _step(job: str, cmd: str = "true", **kwargs: object) -> Step:
    return Step("g", job, "", cmd, **kwargs)  # type: ignore[arg-type]


def _run_with_deadline(runner: Runner) -> bool:
    """Drive ``runner.run()`` on a side thread so a wedge fails the test instead of hanging it."""
    box: list[bool] = []
    thread = threading.Thread(target=lambda: box.append(runner.run()), daemon=True)
    thread.start()
    thread.join(timeout=_DEADLINE_S)
    assert not thread.is_alive(), (
        f"runner.run() did not return within {_DEADLINE_S}s: the scheduler is WEDGED waiting for "
        "a supervisor that will never publish an outcome"
    )
    assert box, "runner.run() ended without producing a verdict"
    return box[0]


def test_an_exception_inside_the_supervisor_fails_the_step_by_name(
    capsys: pytest.CaptureFixture[str],
) -> None:
    """Layer one: any escaping exception becomes a named failure, with a traceback."""
    boom = _step("boom")
    peer = _step("peer")
    runner = Runner(
        DagConfig(steps=(boom, peer)),
        max_steps=2,
        max_cpus=1,
        cgroups=NoopCgroups(),
        verbosity=0,
        keep_going=True,
    )
    real = runner._supervise_step

    def crash_only_boom(step: Step) -> None:
        if step.tag == boom.tag:
            raise RuntimeError("planted supervisor defect")
        real(step)

    runner._supervise_step = crash_only_boom  # type: ignore[method-assign]

    assert not _run_with_deadline(runner)

    outcomes = {o.tag: o for o in runner.result().outcomes}
    assert set(outcomes) == {boom.tag, peer.tag}
    assert outcomes[peer.tag].ok, "an unrelated step must still report its own result"
    crashed = outcomes[boom.tag]
    assert not crashed.ok
    assert not crashed.aborted, "a supervisor crash is a FAILURE, not a peer-triggered abort"
    # The exception must be NAMED, not merely counted: "something went wrong" is the state this
    # whole issue exists to eliminate.
    assert "SUPERVISOR CRASHED" in crashed.reason
    assert "RuntimeError" in crashed.reason
    assert "planted supervisor defect" in crashed.reason

    captured = capsys.readouterr()
    assert "planted supervisor defect" in captured.err
    assert "Traceback" in captured.err, "stderr must carry the traceback for a CI log"
    assert "Traceback" in captured.out, "stdout must carry it too, beside the step's own output"


def test_a_supervisor_that_vanishes_without_a_traceback_is_still_reaped(
    capsys: pytest.CaptureFixture[str],
) -> None:
    """Layer two: the sweep, which covers a failure of layer one itself.

    ``_run_step`` is replaced wholesale, so the ``except BaseException`` guard inside it never
    runs: the thread simply ends with nothing in ``done``. Only the dead-supervisor sweep in the
    ready-set loop can end this run.
    """
    lost = _step("lost")
    runner = Runner(
        DagConfig(steps=(lost,)),
        max_steps=1,
        max_cpus=1,
        cgroups=NoopCgroups(),
        verbosity=0,
    )

    def die_without_publishing(step: Step) -> None:
        raise RuntimeError("layer one is not present on this thread")

    runner._run_step = die_without_publishing  # type: ignore[method-assign]

    assert not _run_with_deadline(runner)

    outcomes = {o.tag: o for o in runner.result().outcomes}
    assert set(outcomes) == {lost.tag}
    assert "SUPERVISOR VANISHED" in outcomes[lost.tag].reason
    assert "UNKNOWN" in outcomes[lost.tag].reason
    combined = capsys.readouterr()
    assert "SUPERVISOR VANISHED" in combined.err


def test_the_sweep_sees_a_crash_that_lands_after_retirement_before_done() -> None:
    """The sweep must find every finished supervisor that has no outcome.

    ``_retire`` gives admission accounting back before ``self.done`` is written. A supervisor
    that dies in that window remains in ``running`` to prevent a relaunch, but only the thread
    sweep can turn that unfinished publication into a terminal outcome.
    """
    ghost = _step("ghost", hint=ResourceHint(resources={"slot": 1}))
    runner = Runner(
        DagConfig(steps=(ghost,), resource_caps={"slot": 1}),
        max_steps=1,
        max_cpus=1,
        cgroups=NoopCgroups(),
        verbosity=0,
    )

    def retire_then_die(step: Step) -> None:
        with runner.lock:
            runner._retire(step)
        # Dead here: retired but absent from `done`. `_run_step` is bypassed, so layer one cannot
        # catch this either.
        raise RuntimeError("died after retiring, before publishing")

    runner._run_step = retire_then_die  # type: ignore[method-assign]

    assert not _run_with_deadline(runner)
    outcomes = {o.tag: o for o in runner.result().outcomes}
    assert "SUPERVISOR VANISHED" in outcomes[ghost.tag].reason


class _ExplodingRows(list):  # type: ignore[type-arg]
    """A ``step_profile_rows`` that raises on the FIRST append.

    That append sits between ``_retire(step)`` and ``self.done[step.tag] = outcome``, inside one
    held lock — the exact window where the step has already given its resources back but has no
    terminal outcome yet. It is the only window in which the crash path can be reached with the
    step already retired, so it is the only place the once-only guard can be observed.
    """

    def __init__(self, runner: Runner) -> None:
        super().__init__()
        self.runner = runner
        self.append_calls = 0
        self.running_at_crash: set[str] = set()

    def append(self, item: object) -> None:
        self.append_calls += 1
        # The caller holds runner.lock. Snapshot directly rather than trying to reacquire the
        # non-reentrant lock from this planted failure.
        self.running_at_crash = set(self.runner.running)
        raise RuntimeError("planted defect between retiring and publishing")


def test_a_crash_after_a_release_does_not_release_a_second_time() -> None:
    """``_retire`` is once-only, so a crash after it cannot drift ``resource_avail`` above its cap.

    Without the guard the crash path gives the slot back a SECOND time and the declared cap of 1
    reads as 2 — worse than a leak, because a cap that silently stopped being a cap over-admits
    the next run with no visible cause.
    """
    late = _step("late", cmd="sleep 0.1", hint=ResourceHint(resources={"slot": 1}))
    runner = Runner(
        DagConfig(steps=(late,), resource_caps={"slot": 1}),
        max_steps=1,
        max_cpus=1,
        cgroups=NoopCgroups(),
        verbosity=0,
    )
    exploding_rows = _ExplodingRows(runner)
    runner.step_profile_rows = exploding_rows

    assert not _run_with_deadline(runner)
    outcomes = {o.tag: o for o in runner.result().outcomes}
    assert "SUPERVISOR CRASHED" in outcomes[late.tag].reason
    assert exploding_rows.append_calls == 1, "a retired step must never be launched again"
    assert exploding_rows.running_at_crash == {late.tag}, (
        "retirement may release resources, but the supervisor must remain in `running` until "
        "its terminal outcome is published"
    )
    assert runner.resource_avail == {"slot": 1}, (
        "the slot must be released exactly once; a second release reads as capacity the declared "
        "cap never had"
    )
    assert runner.cores_used == 0
    assert runner.active_processes == 0, (
        "the child was uncounted when proc.wait() returned; retiring again must not take the "
        "count negative and make max_concurrent_steps meaningless"
    )


class _CgroupsWithABrokenThreadCount(NoopCgroups):
    """A manager whose ``thread_count`` raises, which kills the per-step monitor thread."""

    def thread_count(self, tag: str) -> int | None:
        raise RuntimeError("planted defect in the monitor's first cgroup read")


def test_a_dead_cpu_budget_monitor_says_the_budget_is_no_longer_enforced(
    capsys: pytest.CaptureFixture[str],
) -> None:
    """The monitor is the ONLY enforcer of the CPU-time budget; its death must be audible.

    Nothing ever joins the monitor for a result, so a monitor that dies leaves a guard that still
    reads as configured but enforces nothing — an enforcement switch turned off with no warning
    anywhere. The step itself still succeeds; the point is entirely the warning.
    """
    slow = _step("slow", cmd="sleep 1.5")
    runner = Runner(
        DagConfig(steps=(slow,)),
        max_steps=1,
        max_cpus=1,
        cgroups=_CgroupsWithABrokenThreadCount(),
        verbosity=0,
    )

    assert _run_with_deadline(runner), "the step's own result is unaffected by the monitor dying"
    err = capsys.readouterr().err
    assert "monitor thread DIED" in err
    assert "NO LONGER" in err and "ENFORCED" in err
    assert "RuntimeError" in err


def test_the_crash_record_in_the_journal_names_the_cause(tmp_path: Path) -> None:
    """A run reconstructed from evidence alone must still say WHAT went wrong.

    The console line is gone by the time anyone reads a journal, so an event carrying only a tag
    and a duration reports that something failed without naming the cause — the state this whole
    guard exists to eliminate. The sibling engine writes the same three fields under the same
    event name, and ``make cross`` compares journals.
    """
    victim = _step("boom")
    runner = Runner(
        DagConfig(steps=(victim,)),
        max_steps=1,
        max_cpus=1,
        cgroups=NoopCgroups(),
        verbosity=0,
    )
    runner.evidence = RunEvidence.open(tmp_path / "evidence")
    assert runner.evidence is not None
    assert runner._publish_supervisor_failure(
        victim,
        reason="SUPERVISOR CRASHED (planted supervisor defect)",
        summary="planted supervisor defect",
        duration_s=0.25,
    )

    records = [
        json.loads(line)
        for line in (tmp_path / "evidence" / "journal.jsonl").read_text().splitlines()
    ]
    crash = [r for r in records if r.get("event") == "supervisor_crash"]
    assert len(crash) == 1, records
    assert crash[0]["reason"] == "SUPERVISOR CRASHED (planted supervisor defect)"
    assert crash[0]["step"] == victim.tag
    assert crash[0]["elapsed_s"] == "0.250"


def test_a_crash_after_a_normal_completion_does_not_overwrite_the_published_outcome() -> None:
    """A crash in the REPORTING TAIL is a runner bug, not evidence that the step failed."""
    late = _step("late")
    runner = Runner(
        DagConfig(steps=(late,)),
        max_steps=1,
        max_cpus=1,
        cgroups=NoopCgroups(),
        verbosity=0,
    )
    real_emit = runner._emit

    def crash_on_the_pass_line(line: str) -> None:
        if "PASS" in line:
            raise RuntimeError("planted defect in the reporting tail")
        real_emit(line)

    runner._emit = crash_on_the_pass_line  # type: ignore[method-assign]

    started = time.monotonic()
    _run_with_deadline(runner)
    assert time.monotonic() - started < _DEADLINE_S
    outcomes = {o.tag: o for o in runner.result().outcomes}
    assert outcomes[late.tag].ok, (
        "the step really did exit 0 and said so before the crash; the crash path must not "
        "relabel a recorded success as a failure"
    )
