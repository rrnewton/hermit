"""The terminal step record must carry what the step actually consumed.

``step_end`` exists so the journal alone can answer "what was this run doing" without the
end-of-run profile rows, which a hard kill destroys.  It said how long the step took and whether
it passed, but not what it consumed nor what it was allowed to consume — so the one record
guaranteed to survive a kill could not be used to judge a budget.

The cgroup CPU counters are already read before ``cleanup()`` removes the step's cgroup, so
journalling them costs nothing.  A counter the kernel does not publish stays ABSENT rather than
becoming a measured zero.
"""

from __future__ import annotations

import json
import os
import shlex
import time
from pathlib import Path

from dagrun import DagConfig, Runner, Step
from dagrun.attribution import LOG_DIR_ENV
from dagrun.cgroup import NoopCgroups
from dagrun.model import (
    ABORTED_BY_PEER_FAILURE_REASON,
    ABORTED_BY_RUN_BUDGET_REASON,
    DEFAULT_SMALL_CPU_TIMEOUT,
)
from dagrun.scheduler import _cpu_journal_fields

JournalValue = str | bool


class _FullCounters(NoopCgroups):
    enabled = True

    def cpu_stats(self, tag: str) -> dict[str, int]:
        return {
            "usage_usec": 259_926_893,
            "nr_throttled": 994,
            "throttled_usec": 431_942_000,
            "user_usec": 1,
        }


class _PartialCounters(NoopCgroups):
    enabled = True

    def cpu_stats(self, tag: str) -> dict[str, int]:
        return {"usage_usec": 7}


def _step_end(
    log_dir: Path,
    cgroups: NoopCgroups,
    cpu_timeout: int,
    default_cpu_timeout: int = DEFAULT_SMALL_CPU_TIMEOUT,
) -> dict[str, JournalValue]:
    cfg = DagConfig(
        steps=(Step("g", "quick", "does a little", "true", timeout=45, cpu_timeout=cpu_timeout),),
        default_step_cpu_timeout=default_cpu_timeout,
    )
    previous = os.environ.get(LOG_DIR_ENV)
    os.environ[LOG_DIR_ENV] = str(log_dir)
    try:
        Runner(cfg, max_steps=1, max_cpus=1, cgroups=cgroups).run()
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
    ends = [r for r in records if r.get("event") == "step_end"]
    assert len(ends) == 1, f"expected exactly one step_end record, got {ends}"
    record: dict[str, JournalValue] = ends[0]
    return record


def test_absent_counters_stay_absent_and_present_ones_keep_their_units() -> None:
    assert _cpu_journal_fields(None) == []
    assert _cpu_journal_fields(
        {"usage_usec": 259_926_893, "nr_throttled": 994, "throttled_usec": 431_942_000}
    ) == [
        ("cpu_usage_usec", "259926893"),
        ("cpu_nr_throttled", "994"),
        ("cpu_throttled_usec", "431942000"),
    ]
    # A kernel that publishes only some counters contributes only those. Inventing the rest as 0
    # would put a measurement in the record that was never measured.
    assert _cpu_journal_fields({"usage_usec": 7}) == [("cpu_usage_usec", "7")]


def test_a_boxed_step_journals_what_it_consumed_and_what_it_was_allowed(
    tmp_path: Path,
) -> None:
    record = _step_end(tmp_path / "full", _FullCounters(), cpu_timeout=30)

    assert record["ok"] is True
    assert type(record["ok"]) is bool

    assert record["cpu_usage_usec"] == "259926893"
    assert record["cpu_nr_throttled"] == "994"
    assert record["cpu_throttled_usec"] == "431942000"

    # Both ceilings, each named for the quantity it bounds. Without them the consumption figures
    # above are unjudgeable: a number with no bound beside it is not evidence about a budget.
    assert record["cpu_limit_s"] == "30"
    assert record["wall_limit_s"] == "45"

    # The step's own duration keeps its unit in its name, like everything else beside a limit.
    assert float(record["wall_elapsed_s"]) >= 0.0
    assert "elapsed_s" not in record


def test_a_partially_reporting_kernel_contributes_only_what_it_reported(
    tmp_path: Path,
) -> None:
    record = _step_end(tmp_path / "partial", _PartialCounters(), cpu_timeout=30)
    assert record["cpu_usage_usec"] == "7"
    assert "cpu_nr_throttled" not in record
    assert "cpu_throttled_usec" not in record


def test_an_unboxed_step_journals_no_cpu_counters_at_all(tmp_path: Path) -> None:
    # Unboxed there are no cgroup counters to read. Absent is the honest record; zeroes would be
    # a claim that the step consumed nothing.
    record = _step_end(tmp_path / "unboxed", NoopCgroups(), cpu_timeout=0)
    assert "cpu_usage_usec" not in record
    # The budget recorded is the one actually IN FORCE, so a step that declares none still shows
    # the DAG's small default rather than looking unbounded.
    assert record["cpu_limit_s"] == str(DEFAULT_SMALL_CPU_TIMEOUT)
    assert record["wall_limit_s"] == "45"


def test_a_genuinely_disabled_cpu_budget_is_absent_not_zero(tmp_path: Path) -> None:
    record = _step_end(
        tmp_path / "nobudget", NoopCgroups(), cpu_timeout=0, default_cpu_timeout=0
    )
    # No budget was in force at all. A `cpu_limit_s` of 0 would read as "bounded at zero seconds",
    # which is the opposite of unbounded.
    assert "cpu_limit_s" not in record


def _records(
    log_dir: Path,
    steps: tuple[Step, ...],
    *,
    jobs: int,
    run_timeout_s: int | None = None,
) -> list[dict[str, JournalValue]]:
    """Run a real DAG with its journal in ``log_dir`` and return the ``step_end`` records."""
    cfg = DagConfig(steps=steps)
    previous = os.environ.get(LOG_DIR_ENV)
    os.environ[LOG_DIR_ENV] = str(log_dir)
    try:
        Runner(
            cfg,
            max_steps=jobs,
            max_cpus=jobs,
            cgroups=NoopCgroups(),
            run_timeout_s=run_timeout_s,
        ).run()
    finally:
        if previous is None:
            os.environ.pop(LOG_DIR_ENV, None)
        else:
            os.environ[LOG_DIR_ENV] = previous
    records: list[dict[str, JournalValue]] = [
        json.loads(line)
        for line in (log_dir / "journal.jsonl").read_text().splitlines()
        if line.strip()
    ]
    return [r for r in records if r.get("event") == "step_end"]


def _by_step(
    records: list[dict[str, JournalValue]], tag: str
) -> dict[str, JournalValue]:
    match = [r for r in records if r.get("step") == tag]
    assert len(match) == 1, f"expected exactly one step_end for {tag}, got {records}"
    return match[0]


def test_a_step_that_fails_records_the_cause_it_already_computed(tmp_path: Path) -> None:
    # Without this the record says a step was not ok and never what happened, so a failure could
    # be COUNTED and never NAMED -- and an unknown nobody can name is one nobody can drive down.
    records = _records(
        tmp_path / "fail",
        (Step("a", "boom", "fails", "exit 3", timeout=45, cpu_timeout=30),),
        jobs=1,
    )
    record = _by_step(records, "a.boom")
    assert record["ok"] is False, record
    assert record["reason"] == "exit 3", record


def test_a_passing_step_carries_no_reason_key_at_all(tmp_path: Path) -> None:
    record = _step_end(tmp_path / "control", NoopCgroups(), cpu_timeout=30)
    assert record["ok"] is True, record
    # Absent rather than empty, for the same reason an unset budget is absent above: a
    # ``"reason": ""`` would read as a cause that was looked for and not found.
    assert "reason" not in record


def test_a_peer_failure_cancellation_names_the_peer_and_not_the_run_budget(
    tmp_path: Path,
) -> None:
    records = _records(
        tmp_path / "peer",
        (
            Step("a", "slow", "outlives its peer", "sleep 20", timeout=45, cpu_timeout=30),
            Step("a", "boom", "fails first", "exit 1", timeout=45, cpu_timeout=30),
        ),
        jobs=2,
    )
    record = _by_step(records, "a.slow")
    assert record["aborted"] == "true", record
    reason = record["reason"]
    assert isinstance(reason, str), record
    assert "eager-exit after another step failed" in reason, record
    assert "OUTER run budget" not in reason, record


def test_an_outer_budget_cut_names_the_budget_and_never_a_peer(tmp_path: Path) -> None:
    # No step here fails, so any mention of a failing peer would describe something that never
    # happened. The predecessor spends most of the budget so the run bound fires while the
    # successor is still inside its own step budget.
    records = _records(
        tmp_path / "outer",
        (
            Step("a", "first", "spends the budget", "sleep 4", timeout=7, cpu_timeout=30),
            Step(
                "a",
                "long",
                "is cut by the run bound",
                "sleep 30",
                deps=["a.first"],
                timeout=7,
                cpu_timeout=30,
            ),
        ),
        jobs=1,
        run_timeout_s=8,
    )
    record = _by_step(records, "a.long")
    assert record["aborted"] == "true", record
    reason = record["reason"]
    assert isinstance(reason, str), record
    assert "cut short by the OUTER run budget" in reason, record
    # The control that makes the assertion above mean something: before the reason distinguished
    # the two cancellations this record claimed a peer had failed, and a reader who only has the
    # record -- the reader it exists for -- would have gone looking for one.
    assert "eager-exit" not in reason, record


class _CleanupAfterDeadline(NoopCgroups):
    """Delay only result collection; children, fail-fast, clock and signals stay real.

    This is not a containment test. The no-op manager still returns false from kill,
    so the scheduler reaps actual process groups. Its cleanup callback makes the
    otherwise timing-dependent completion/deadline overlap reproducible.
    """

    enabled = True

    def __init__(self, journal: Path) -> None:
        self.journal = journal
        self.observed_deadline = False

    def cleanup(self, tag: str) -> None:
        if tag != "a.peer":
            return
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            for line in self.journal.read_text().splitlines():
                try:
                    record = json.loads(line)
                except json.JSONDecodeError:
                    continue  # A concurrent journal append may not have finished.
                if record.get("event") == "run_timeout":
                    self.observed_deadline = True
                    return
            time.sleep(0.01)
        raise AssertionError("the real run deadline never fired during peer cleanup")


def test_peer_cancellation_survives_a_later_deadline_in_the_same_run(tmp_path: Path) -> None:
    logs = tmp_path / "mixed"
    ready = shlex.quote(str(tmp_path / "ready"))
    cfg = DagConfig(
        steps=(
            Step("a", "first", "spends the budget", "sleep 3", timeout=6, cpu_timeout=30),
            Step(
                "a", "peer", "cancelled by its failing peer", f"touch {ready}; sleep 30",
                deps=["a.first"], timeout=6, cpu_timeout=30, fail_fast_family="failed",
            ),
            Step(
                "a", "boom", "fails after its peer starts",
                f"while [ ! -f {ready} ]; do sleep 0.01; done; exit 7",
                deps=["a.first"], timeout=6, cpu_timeout=30, fail_fast_family="failed",
            ),
            Step(
                "a", "independent", "cancelled only by the later run deadline", "sleep 30",
                deps=["a.first"], timeout=6, cpu_timeout=30, fail_fast_family="independent",
            ),
        )
    )
    manager = _CleanupAfterDeadline(logs / "journal.jsonl")
    previous = os.environ.get(LOG_DIR_ENV)
    os.environ[LOG_DIR_ENV] = str(logs)
    try:
        runner = Runner(
            cfg, max_steps=3, max_cpus=3, cgroups=manager, run_timeout_s=7,
        )
        runner.run()
        result = runner.result()
    finally:
        if previous is None:
            os.environ.pop(LOG_DIR_ENV, None)
        else:
            os.environ[LOG_DIR_ENV] = previous
    assert not result.ok and result.run_timed_out
    assert manager.observed_deadline
    outcomes = {outcome.tag: outcome for outcome in result.outcomes}
    assert set(outcomes) == {"a.first", "a.peer", "a.boom", "a.independent"}
    assert outcomes["a.first"].ok
    assert outcomes["a.boom"].returncode == 7 and not outcomes["a.boom"].aborted
    records = [json.loads(line) for line in manager.journal.read_text().splitlines()]
    ends = [record for record in records if record.get("event") == "step_end"]
    for tag, reason in [
        ("a.peer", ABORTED_BY_PEER_FAILURE_REASON),
        ("a.independent", ABORTED_BY_RUN_BUDGET_REASON),
    ]:
        assert outcomes[tag].aborted and outcomes[tag].reason == reason
        code = outcomes[tag].returncode
        assert code is not None and code < 0, "the real child must have been signalled"
        assert not outcomes[tag].timed_out and not outcomes[tag].cpu_timed_out
        assert _by_step(ends, tag)["reason"] == reason
    timeout_index = next(
        i for i, record in enumerate(records) if record.get("event") == "run_timeout"
    )
    peer_failure_index = next(
        i for i, record in enumerate(records)
        if record.get("event") == "step_end" and record.get("step") == "a.boom"
    )
    peer_end_index = next(
        i for i, record in enumerate(records)
        if record.get("event") == "step_end" and record.get("step") == "a.peer"
    )
    assert peer_failure_index < timeout_index < peer_end_index, (
        "peer failure, real deadline, delayed peer completion must occur in that order"
    )
