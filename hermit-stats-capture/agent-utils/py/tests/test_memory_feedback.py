"""The profile-to-admission path must never learn a smaller cap from a censored sample.

A peak recorded while a ``memory.max`` was clamping proves only that the step used all it was
allowed. Deriving a cap from it re-derives the cap that produced it, so the workload is pinned
at whatever ceiling it was first given. These tests pin the three ways that is prevented: a
censored sample is a floor and never a central estimate, an unprovenanced sample is not
evidence at all, and with no uncensored evidence the authored hint survives untouched.
"""

from __future__ import annotations

import csv
from pathlib import Path

import pytest

from dagrun.memory_feedback import (
    DEFAULT_MARGIN_PCT,
    DEFAULT_MIN_UNCENSORED_SAMPLES,
    Censoring,
    apply_memory_admissions,
    load_memory_admissions,
    memory_admission_from_rows,
    peak_observation_from_row,
)
from dagrun.model import DagConfig, IntentionalSkipReason, ResourceHint, Step

GIB = 1024**3


def _row(
    step: str = "g.build",
    *,
    peak: str = "1073741824",
    cap: str = "8589934592",
    reclaim: str = "0",
    oom: str = "0",
    high: str = "0",
    oom_invoked: str = "0",
    **extra: str,
) -> dict[str, str]:
    row = {
        "step": step,
        "peak_bytes": peak,
        "memory_max_bytes": cap,
        "memory_events_high": high,
        "memory_events_max": reclaim,
        "memory_events_oom": oom_invoked,
        "memory_events_oom_kill": oom,
        "memory_events_oom_group_kill": "0",
    }
    row.update(extra)
    return row


def _step(tag: str, *, rss: int | None, hard: int | None = None) -> Step:
    group, job = tag.split(".", 1)
    return Step(
        group,
        job,
        "",
        "true",
        hint=ResourceHint(rss_baseline_bytes=rss, hard_mem_max_bytes=hard),
    )


# ---------------------------------------------------------------------------
# Classifying one recorded peak.
# ---------------------------------------------------------------------------


def test_a_peak_under_a_known_quiet_cap_is_a_real_observation() -> None:
    observed = peak_observation_from_row(_row(peak=str(2 * GIB), cap=str(8 * GIB)))

    assert observed.verdict is Censoring.UNCENSORED
    assert observed.peak_bytes == 2 * GIB
    assert observed.applied_cap_bytes == 8 * GIB


@pytest.mark.parametrize(
    ("why", "kwargs"),
    [
        ("peak reached the cap", {"peak": str(8 * GIB), "cap": str(8 * GIB)}),
        ("peak above a page-rounded cap", {"peak": str(8 * GIB), "cap": str(8 * GIB - 4096)}),
        ("held at the ceiling by reclaim", {"reclaim": "17"}),
        ("killed at the ceiling", {"oom": "2"}),
        ("throttled at a soft ceiling", {"high": "3"}),
        ("the oom killer was invoked without a recorded kill", {"oom_invoked": "1"}),
        ("cut short by the wall guard", {"timed_out": "true"}),
        ("cut short by the cpu guard", {"cpu_timed_out": "true"}),
    ],
)
def test_a_peak_that_met_a_ceiling_is_censored(why: str, kwargs: dict[str, str]) -> None:
    assert peak_observation_from_row(_row(**kwargs)).verdict is Censoring.CENSORED, why


def test_every_pressure_counter_the_writer_records_is_read() -> None:
    """The profile persists every actionable ``memory.events`` counter. A reader that consults
    only some of them
    calls a throttled or OOM-invoked step comfortable, which is the exact failure this path
    exists to prevent.

    ``low`` is deliberately NOT censoring: it counts reclaim that breached a ``memory.low``
    PROTECTION, which does not bound the cgroup's own peak.
    """
    censoring = {"memory_events_high", "memory_events_max", "memory_events_oom",
                 "memory_events_oom_kill", "memory_events_oom_group_kill"}
    for column in censoring:
        row = _row()
        row[column] = "1"
        assert peak_observation_from_row(row).verdict is Censoring.CENSORED, column
    low = _row()
    low["memory_events_low"] = "9"
    assert peak_observation_from_row(low).verdict is Censoring.UNCENSORED


@pytest.mark.parametrize(
    ("why", "kwargs"),
    [
        ("no applied cap recorded", {"cap": ""}),
        ("no event counters recorded", {"reclaim": ""}),
        ("no peak recorded", {"peak": ""}),
        ("a cap cell that is neither max nor a number", {"cap": "unbounded"}),
    ],
)
def test_a_row_that_cannot_answer_is_unknown_not_uncensored(
    why: str, kwargs: dict[str, str]
) -> None:
    """Silence must not read as comfort. An unprovenanced row is excluded, never assumed safe."""
    assert peak_observation_from_row(_row(**kwargs)).verdict is Censoring.UNKNOWN, why


def test_an_unbounded_step_that_never_reclaimed_is_a_real_observation() -> None:
    """``max`` is a KNOWN ceiling of none, so its peak is genuine demand."""
    observed = peak_observation_from_row(_row(peak=str(3 * GIB), cap="max"))

    assert observed.cap_known
    assert observed.cap_unbounded
    assert observed.verdict is Censoring.UNCENSORED


@pytest.mark.parametrize(
    ("why", "cells"),
    [
        ("killed by a signal (137 == SIGKILL)", {"returncode": "137"}),
        ("ordinary non-zero exit", {"returncode": "2"}),
        ("ok recorded false", {"ok": "false"}),
        ("ok recorded False", {"ok": "False"}),
    ],
)
def test_a_row_that_records_a_failed_step_is_censored(why: str, cells: dict[str, str]) -> None:
    """A peak measured by a run that DIED is the least trustworthy sample in the store.

    ``returncode`` 137 is SIGKILL, which is exactly what an OOM kill delivered by an ANCESTOR
    cgroup looks like from here: this step's own ``memory.events`` counters are all zero and the
    peak sits comfortably under its own cap, so nothing else in the row gives the failure away.
    """
    observed = peak_observation_from_row(_row(**cells))

    assert observed.run_failed, why
    assert observed.verdict is Censoring.CENSORED, why


def test_a_row_that_says_nothing_about_the_verdict_is_not_treated_as_a_failure() -> None:
    """Silence is not failure: a store carrying no verdict columns is still usable evidence."""
    assert not peak_observation_from_row(_row()).run_failed
    assert peak_observation_from_row(_row()).verdict is Censoring.UNCENSORED
    passed = peak_observation_from_row(_row(ok="true", returncode="0"))
    assert not passed.run_failed
    assert passed.verdict is Censoring.UNCENSORED


def test_a_step_whose_recorded_runs_all_failed_keeps_the_authored_hint() -> None:
    """Six runs killed at ~1 GiB by something outside this cgroup must not license a 1.2 GiB cap.

    That is the exact ratchet-down this path exists to prevent, and the reason must not blame an
    applied cap these peaks never came near.
    """
    rows = [_row(peak=str(GIB), cap=str(8 * GIB), returncode="137", ok="false") for _ in range(6)]

    admission = memory_admission_from_rows(rows)["g.build"]

    assert admission.source == "hint"
    assert admission.rss_baseline_bytes is None
    assert admission.uncensored_samples == 0
    assert admission.censored_samples == 6
    assert "6 sample(s) recorded a step that FAILED, counted as censored" in admission.reason
    assert "by its applied cap" not in admission.reason


def test_the_legacy_oom_column_still_censors_when_the_event_counter_is_absent() -> None:
    """A store written before the event counters existed still records ``oom_kills``."""
    row = _row(peak=str(8 * GIB), oom="")
    row["oom_kills"] = "1"

    assert peak_observation_from_row(row).verdict is Censoring.CENSORED


# ---------------------------------------------------------------------------
# Aggregating into an admission.
# ---------------------------------------------------------------------------


def test_enough_quiet_samples_produce_a_percentile_estimate_with_margin() -> None:
    """The expected bytes are written out LITERALLY. Recomputing them from
    ``DEFAULT_MARGIN_PCT`` would pin the arithmetic to whatever the constant happens to be, so
    changing 20 to 0 would leave the test green while every learned cap lost its headroom."""
    rows = [_row(peak=str(2 * GIB)) for _ in range(6)]

    admission = memory_admission_from_rows(rows)["g.build"]

    assert admission.source == "profile"
    assert admission.uncensored_samples == 6
    assert admission.margin_pct == 20
    # Every sample is exactly 2147483648 B, so the 9/10 percentile is 2147483648 B and the whole
    # difference is the 20% margin: 2147483648 + 429496729 = 2576980377.
    assert admission.rss_baseline_bytes == 2576980377
    assert not admission.censoring_excluded_samples()


def test_the_default_margin_is_twenty_percent() -> None:
    """Named once, here, so a change to the shipped headroom has to be made deliberately."""
    assert DEFAULT_MARGIN_PCT == 20


def test_the_default_minimum_uncensored_sample_count_is_five() -> None:
    """The shipped default, named LITERALLY, for the same reason the margin is.

    Every other test here either passes a threshold in explicitly or supplies six samples, so
    nothing else would notice the shipped number changing. The user guide, the commit body and
    the report all state five; this is where that claim is enforced.
    """
    assert DEFAULT_MIN_UNCENSORED_SAMPLES == 5


def test_the_shipped_default_threshold_refuses_four_samples_and_accepts_five() -> None:
    """The constant is a number until something reads it.

    Neither call passes a threshold, so both exercise whatever the build ships, and the boundary
    either side of it is named literally: moving the constant off 5 flips one of these verdicts.
    """
    four = memory_admission_from_rows([_row(peak=str(GIB)) for _ in range(4)])["g.build"]
    five = memory_admission_from_rows([_row(peak=str(GIB)) for _ in range(5)])["g.build"]

    assert four.source == "hint"
    assert "only 4 uncensored sample(s); 5 required" in four.reason
    assert five.source == "profile"


def test_the_estimate_never_falls_below_the_largest_censored_peak() -> None:
    """A censored peak proves demand was AT LEAST that large, so it can only raise the cap.

    Five quiet 1 GiB samples would otherwise justify 1.2 GiB. One sample that hit a 8 GiB
    ceiling says the step has wanted 8 GiB at least once; the recommendation must not be a
    number that step has already exceeded.
    """
    rows = [_row(peak=str(GIB), cap=str(8 * GIB)) for _ in range(5)]
    rows.append(_row(peak=str(8 * GIB), cap=str(8 * GIB), oom="1"))

    admission = memory_admission_from_rows(rows)["g.build"]

    assert admission.source == "profile"
    assert admission.uncensored_samples == 5
    assert admission.censored_samples == 1
    assert admission.censored_floor_bytes == 8 * GIB
    assert admission.rss_baseline_bytes == 8 * GIB
    assert admission.censoring_excluded_samples()


def test_the_estimate_never_falls_below_the_largest_uncensored_peak() -> None:
    """The percentile can sit under the maximum; the maximum is a thing that really happened.

    Nine 1 GiB samples and one 6 GiB sample: n=10, so the 9/10 nearest-rank percentile is the
    9th smallest (1073741824 B) and +20% is 1288490188 B — under the 6442450944 B this step
    really used. The floor, not the percentile, must decide, and the number is named literally.
    """
    rows = [_row(peak=str(GIB)) for _ in range(9)]
    rows.append(_row(peak=str(6 * GIB)))

    admission = memory_admission_from_rows(rows)["g.build"]

    assert admission.observed_peak_bytes == 6 * GIB
    assert admission.rss_baseline_bytes is not None
    assert admission.rss_baseline_bytes >= 6 * GIB
    assert admission.rss_baseline_bytes == 6442450944


def test_every_peak_censored_keeps_the_static_hint_and_says_so() -> None:
    """This is the fully-censored case: a step whose whole history sits on its own ceiling."""
    rows = [_row(peak=str(8 * GIB), cap=str(8 * GIB)) for _ in range(30)]

    admission = memory_admission_from_rows(rows)["g.build"]

    assert admission.source == "hint"
    assert admission.rss_baseline_bytes is None
    assert admission.censored_samples == 30
    assert admission.uncensored_samples == 0
    assert "censored by its applied cap" in admission.reason


def test_unprovenanced_samples_alone_keep_the_static_hint_and_say_so() -> None:
    """A store written before the applied cap was recorded cannot license a smaller cap."""
    rows = [_row(peak=str(GIB), cap="", reclaim="") for _ in range(30)]

    admission = memory_admission_from_rows(rows)["g.build"]

    assert admission.source == "hint"
    assert admission.rss_baseline_bytes is None
    assert admission.unknown_samples == 30
    assert "no applied-cap or event provenance" in admission.reason


def test_too_few_uncensored_samples_keeps_the_static_hint_and_says_how_many() -> None:
    rows = [_row(peak=str(GIB)) for _ in range(2)]

    admission = memory_admission_from_rows(rows, min_uncensored_samples=5)["g.build"]

    assert admission.source == "hint"
    assert admission.rss_baseline_bytes is None
    assert "only 2 uncensored sample(s); 5 required" in admission.reason


def test_samples_from_another_revision_are_excluded_and_counted() -> None:
    """A cap learned across a change that moved memory is a cap learned from two workloads."""
    rows = [_row(peak=str(GIB), profile_base_sha="new") for _ in range(5)]
    rows += [_row(peak=str(64 * GIB), profile_base_sha="old") for _ in range(20)]

    admission = memory_admission_from_rows(rows, profile_base_sha="new")["g.build"]

    assert admission.source == "profile"
    assert admission.uncensored_samples == 5
    assert admission.rss_baseline_bytes is not None
    # The 20 huge samples belong to the other revision and must not raise this cap.
    assert admission.rss_baseline_bytes < 2 * GIB
    assert "20 sample(s) excluded as recorded against another source revision" in admission.reason


def test_a_step_with_no_rows_at_all_is_absent_rather_than_zero() -> None:
    assert memory_admission_from_rows([]) == {}


# ---------------------------------------------------------------------------
# Applying an admission to a DAG.
# ---------------------------------------------------------------------------


def _cfg(*steps: Step) -> DagConfig:
    return DagConfig(steps=tuple(steps))


def test_only_a_profile_backed_admission_replaces_the_authored_hint() -> None:
    rows = [_row("g.learned", peak=str(2 * GIB)) for _ in range(6)]
    rows += [_row("g.censored", peak=str(8 * GIB), cap=str(8 * GIB)) for _ in range(6)]
    admissions = memory_admission_from_rows(rows)
    cfg = _cfg(_step("g.learned", rss=99), _step("g.censored", rss=99), _step("g.absent", rss=99))

    applied = apply_memory_admissions(cfg, admissions)

    by_tag = {step.tag: step for step in applied.steps}
    assert by_tag["g.learned"].hint.rss_baseline_bytes == admissions["g.learned"].rss_baseline_bytes
    assert by_tag["g.censored"].hint.rss_baseline_bytes == 99
    assert by_tag["g.absent"].hint.rss_baseline_bytes == 99


def test_a_declined_step_is_restored_to_the_baseline_its_author_wrote() -> None:
    """The fully-censored case at the seam where it actually bites.

    This unit exercises the defensive seam directly with a pre-lowered config, as an older caller
    or persisted plan may still provide. Declining has to restore the authored 42949672960 B hint,
    not leave 8589934592 B in place under the words "keeping the authored hint".
    """
    rows = [_row("g.pinned", peak=str(8 * GIB), cap=str(8 * GIB)) for _ in range(6)]
    admissions = memory_admission_from_rows(rows)
    assert admissions["g.pinned"].source == "hint"
    planned = Step("g", "pinned", "", "true", timeout=1234,
                   hint=ResourceHint(rss_baseline_bytes=8 * GIB))

    applied = apply_memory_admissions(
        _cfg(planned), admissions, {"g.pinned": 42949672960}
    )

    assert applied.steps[0].hint.rss_baseline_bytes == 42949672960
    # Clone-and-override: nothing else moves.
    assert applied.steps[0].timeout == 1234


def test_a_step_with_no_authored_baseline_at_all_is_restored_to_none() -> None:
    """The same undo when the author wrote nothing: the plan's learned number must not survive as
    a hint the author never gave.

    Six UNPROVENANCED rows — a peak but no applied cap — so nothing here is proof of anything,
    not even a floor. The censoring-BLIND plan feedback still reads their ``peak_bytes`` and
    learns 8589934592 from them, which is the number that must not survive.
    """
    rows = [_row("g.pinned", peak=str(8 * GIB), cap="") for _ in range(6)]
    planned = _step("g.pinned", rss=8 * GIB)

    applied = apply_memory_admissions(
        _cfg(planned), memory_admission_from_rows(rows), {"g.pinned": None}
    )

    assert applied.steps[0].hint.rss_baseline_bytes is None


def test_a_decline_keeps_the_censored_floor_when_it_is_above_the_authored_hint() -> None:
    """A decline says "we do not know the peak", never "we know nothing".

    Six runs pinned to a 34359738368 B ceiling prove demand was AT LEAST that, whatever the
    true maximum is. The author guessed 1073741824 B. Restoring that guess would use the flag
    to model the step at a thirty-second of its proven demand — the ratchet this path exists to
    prevent, running the other way. The declined baseline is the floor, named literally, with
    NO margin: a floor is a fact about the past, not an estimate of the next run.
    """
    rows = [_row("g.pinned", peak="34359738368", cap="34359738368") for _ in range(6)]
    admissions = memory_admission_from_rows(rows)
    assert admissions["g.pinned"].source == "hint"
    assert admissions["g.pinned"].proven_floor_bytes() == 34359738368
    planned = _step("g.pinned", rss=34359738368)

    applied = apply_memory_admissions(_cfg(planned), admissions, {"g.pinned": 1073741824})

    assert applied.steps[0].hint.rss_baseline_bytes == 34359738368


def test_a_decline_keeps_sub_threshold_uncensored_evidence_as_a_floor() -> None:
    """Four comfortable samples are too thin to fit a distribution to, and still happened.

    Under a 68719476736 B cap, four uncensored peaks of 34359738368 B do not reach the
    five-sample threshold, so no estimate is made. They are nonetheless proof that the step has
    already used 34359738368 B, which the author's 1073741824 B guess does not cover.
    """
    rows = [_row("g.scarce", peak="34359738368", cap="68719476736") for _ in range(4)]
    admissions = memory_admission_from_rows(rows)
    assert admissions["g.scarce"].source == "hint"
    assert admissions["g.scarce"].censored_floor_bytes is None
    assert admissions["g.scarce"].proven_floor_bytes() == 34359738368

    applied = apply_memory_admissions(
        _cfg(_step("g.scarce", rss=99)), admissions, {"g.scarce": 1073741824}
    )

    assert applied.steps[0].hint.rss_baseline_bytes == 34359738368


def test_a_decline_never_lowers_a_baseline_the_author_already_set_high_enough() -> None:
    """The floor only ever raises. An author who wrote more than the evidence proves keeps it."""
    rows = [_row("g.pinned", peak=str(8 * GIB), cap=str(8 * GIB)) for _ in range(6)]

    applied = apply_memory_admissions(
        _cfg(_step("g.pinned", rss=8 * GIB)),
        memory_admission_from_rows(rows),
        {"g.pinned": 42949672960},
    )

    assert applied.steps[0].hint.rss_baseline_bytes == 42949672960


def test_a_step_with_no_authored_baseline_still_takes_the_floor_its_peaks_prove() -> None:
    """No authored hint is not a reason to forget a 8589934592 B peak the step really reached."""
    rows = [_row("g.pinned", peak=str(8 * GIB), cap=str(8 * GIB)) for _ in range(6)]

    applied = apply_memory_admissions(
        _cfg(_step("g.pinned", rss=8 * GIB)),
        memory_admission_from_rows(rows),
        {"g.pinned": None},
    )

    assert applied.steps[0].hint.rss_baseline_bytes == 8589934592


def test_the_proven_floor_is_the_larger_of_the_censored_and_uncensored_peaks() -> None:
    """Both kinds of peak are lower bounds, so the floor is whichever of them is bigger."""
    censored_wins = memory_admission_from_rows(
        [_row(peak=str(2 * GIB)) for _ in range(4)]
        + [_row(peak=str(8 * GIB), cap=str(8 * GIB))]
    )["g.build"]
    uncensored_wins = memory_admission_from_rows(
        [_row(peak=str(6 * GIB)) for _ in range(4)]
        + [_row(peak=str(2 * GIB), cap=str(2 * GIB))]
    )["g.build"]

    assert censored_wins.proven_floor_bytes() == 8589934592
    assert uncensored_wins.proven_floor_bytes() == 6442450944
    assert memory_admission_from_rows([_row(cap="")])["g.build"].proven_floor_bytes() is None


def test_a_skipped_step_is_left_alone_even_when_the_authored_baseline_is_known() -> None:
    """A skip is not a licence to rewrite authored hints in either direction."""
    planned = Step("g", "pinned", "", "true",
                   hint=ResourceHint(rss_baseline_bytes=8 * GIB),
                   skip_reason=IntentionalSkipReason.EMPTY_MANIFEST_BUCKET)

    applied = apply_memory_admissions(_cfg(planned), {}, {"g.pinned": 42949672960})

    assert applied.steps[0].hint.rss_baseline_bytes == 8 * GIB


def test_an_explicit_hard_cap_is_never_rewritten_by_the_profile_path() -> None:
    """A hard cap is an instruction. Learning may move the baseline under it, never the cap."""
    rows = [_row("g.learned", peak=str(2 * GIB)) for _ in range(6)]
    cfg = _cfg(_step("g.learned", rss=99, hard=5 * GIB))

    applied = apply_memory_admissions(cfg, memory_admission_from_rows(rows))

    step = applied.steps[0]
    assert step.hint.hard_mem_max_bytes == 5 * GIB
    assert step.hint.rss_baseline_bytes != 99


def test_applying_admissions_carries_every_other_step_field(tmp_path: Path) -> None:
    """The clone-and-override rule: a newly added Step field must not silently reset here."""
    rows = [_row("g.learned", peak=str(2 * GIB)) for _ in range(6)]
    original = Step(
        "g",
        "learned",
        "a description",
        "make build",
        deps=[],
        timeout=1234,
        cpu_timeout=567,
        hint=ResourceHint(rss_baseline_bytes=99, est_duration_s=42.0, preferred_inner_jobs=8),
    )

    applied = apply_memory_admissions(_cfg(original), memory_admission_from_rows(rows))

    step = applied.steps[0]
    assert step.timeout == 1234
    assert step.cpu_timeout == 567
    assert step.desc == "a description"
    assert step.hint.est_duration_s == 42.0
    assert step.hint.preferred_inner_jobs == 8


# ---------------------------------------------------------------------------
# Reading a real store.
# ---------------------------------------------------------------------------


def _write_store(store: Path, machine: str, container: str, rows: list[dict[str, str]]) -> None:
    path = store / f"step_profiles_{machine}_{container}.csv"
    fieldnames = sorted({key for row in rows for key in row})
    with path.open("w", newline="", encoding="utf-8") as handle:
        writer = csv.DictWriter(handle, fieldnames=fieldnames, lineterminator="\n")
        writer.writeheader()
        writer.writerows(rows)


def test_a_store_is_read_for_the_named_identity_only(tmp_path: Path) -> None:
    """A cap learned on one container class does not transfer to another."""
    _write_store(tmp_path, "hostA", "affinity8_cpu-max-max", [_row(peak=str(GIB)) for _ in range(6)])

    mine = load_memory_admissions(tmp_path, "hostA", "affinity8_cpu-max-max")
    theirs = load_memory_admissions(tmp_path, "hostA", "affinity64_cpu-max-max")

    assert mine["g.build"].source == "profile"
    assert theirs == {}


def test_a_missing_store_means_keep_every_hint_rather_than_no_memory(tmp_path: Path) -> None:
    admissions = load_memory_admissions(tmp_path, "hostA", "affinity8_cpu-max-max")
    cfg = _cfg(_step("g.build", rss=7 * GIB))

    assert admissions == {}
    assert apply_memory_admissions(cfg, admissions).steps[0].hint.rss_baseline_bytes == 7 * GIB


# ---------------------------------------------------------------------------
# The opt-in CLI surface.
# ---------------------------------------------------------------------------


def _run_cli(args: list[str], env: dict[str, str]) -> tuple[int, str, str]:
    import os
    import subprocess
    import sys

    child = os.environ.copy()
    child.update(env)
    child["PYTHONPATH"] = str(Path(__file__).resolve().parents[1])
    proc = subprocess.run(
        [sys.executable, "-m", "dagrun", *args],
        text=True,
        capture_output=True,
        check=False,
        env=child,
    )
    return proc.returncode, proc.stdout, proc.stderr


def test_the_flag_is_off_by_default_and_reports_every_decision_when_on(tmp_path: Path) -> None:
    """The report must name the steps that did NOT move, or silence reads as agreement.

    ``g.pinned`` authors 42949672960 B, comfortably above the 8589934592 B its censored peaks
    prove, so the decline really does leave it where its author put it and the line says so.
    """
    machine, container = "synthhost", "affinity4_cpu-max-max"
    store = tmp_path / "store"
    store.mkdir()
    rows = [_row("g.learned", peak=str(2 * GIB)) for _ in range(6)]
    rows += [_row("g.pinned", peak=str(8 * GIB), cap=str(8 * GIB)) for _ in range(6)]
    _write_store(store, machine, container, rows)
    dag = tmp_path / "dag.json"
    dag.write_text(
        '{"steps": ['
        '{"group": "g", "job": "learned", "cmd": "true", "deps": []},'
        '{"group": "g", "job": "pinned", "cmd": "true", "deps": [],'
        ' "hint": {"rss_baseline_bytes": 42949672960}}]}',
        encoding="utf-8",
    )
    env = {
        "DAGRUN_MACHINE_ID": machine,
        "DAGRUN_CONTAINER_CLASS": container,
    }
    common = ["run", "--dag", str(dag), "--perf-dir", str(store), "--no-profile",
              "--unsafe-no-cgroups", "-q"]

    silent_rc, _out, silent_err = _run_cli(common, env)
    loud_rc, _out2, loud_err = _run_cli([*common, "--profile-memory-feedback"], env)

    assert silent_rc == 0, silent_err
    assert loud_rc == 0, loud_err
    assert "--profile-memory-feedback" not in silent_err
    assert "g.learned: rss_baseline_bytes=" in loud_err
    assert "g.pinned: keeping the authored hint" in loud_err
    assert "censored by its applied cap" in loud_err


def test_the_derived_baseline_reaches_max_mem_admission(tmp_path: Path) -> None:
    """The point of the whole path: the number it derives must change what admission does.

    One step, so the modeled worst case is that step's own cap and is independent of this host's
    CPU count. Its six uncensored samples peaked at 21474836480 B under a 107374182400 B cap.

    * Without the flag, the ordinary feedback takes the peak at face value: 21474836480 B, and
      ``--max-mem`` models a worst case of 26843545600 B.
    * With it, the censoring-aware estimate adds the 20% margin (25769803776 B) and the modeled
      worst case rises to 32212254720 B.

    Both numbers are named literally. A wiring change that applied the estimate AFTER admission
    had already read the config would leave the second equal to the first.
    """
    machine, container = "synthhost", "affinity4_cpu-max-max"
    store = tmp_path / "store"
    store.mkdir()
    _write_store(store, machine, container,
                 [_row("g.learned", peak="21474836480", cap="107374182400") for _ in range(6)])
    dag = tmp_path / "dag.json"
    dag.write_text(
        '{"steps": [{"group": "g", "job": "learned", "cmd": "true", "deps": [],'
        ' "hint": {"rss_baseline_bytes": 104857600}}]}',
        encoding="utf-8",
    )
    env = {
        "DAGRUN_MACHINE_ID": machine,
        "DAGRUN_CONTAINER_CLASS": container,
    }
    common = ["run", "--dag", str(dag), "--perf-dir", str(store), "--no-profile",
              "--unsafe-no-cgroups", "-q", "--max-mem", "400G"]

    off_rc, _o1, off_err = _run_cli(common, env)
    on_rc, _o2, on_err = _run_cli([*common, "--profile-memory-feedback"], env)

    assert off_rc == 0, off_err
    assert on_rc == 0, on_err
    assert "worst-case 26843545600 bytes" in off_err, off_err
    assert "rss_baseline_bytes=25769803776" in on_err, on_err
    assert "worst-case 32212254720 bytes" in on_err, on_err


def test_a_censored_peak_cannot_reach_max_mem_admission_once_the_flag_is_on(
    tmp_path: Path,
) -> None:
    """Turning the flag ON must STOP the runner learning a cap from censored peaks.

    One step, six recorded runs, every one of them pinned to its 8589934592 B cap: the
    all-censored history this whole path exists for. The DAG authors 42949672960 B.

    * Without the flag, the authored 42949672960 B floor prevents ordinary feedback from lowering
      the step, so ``--max-mem`` models a worst case of 53687091200 B.
    * With the flag the step is DECLINED because every sample is censored, and the same authored
      floor remains what admission sees.

    The opt-in report must still say why it declined; equal footprints prove the safe ordinary
    floor does not depend on the caller remembering the flag.
    """
    machine, container = "synthhost", "affinity4_cpu-max-max"
    store = tmp_path / "store"
    store.mkdir()
    _write_store(store, machine, container,
                 [_row("g.pinned", peak="8589934592", cap="8589934592") for _ in range(6)])
    dag = tmp_path / "dag.json"
    dag.write_text(
        '{"steps": [{"group": "g", "job": "pinned", "cmd": "true", "deps": [],'
        ' "hint": {"rss_baseline_bytes": 42949672960}}]}',
        encoding="utf-8",
    )
    env = {
        "DAGRUN_MACHINE_ID": machine,
        "DAGRUN_CONTAINER_CLASS": container,
    }
    common = ["run", "--dag", str(dag), "--perf-dir", str(store), "--no-profile",
              "--unsafe-no-cgroups", "-q", "--max-mem", "400G"]

    off_rc, _o1, off_err = _run_cli(common, env)
    on_rc, _o2, on_err = _run_cli([*common, "--profile-memory-feedback"], env)

    assert off_rc == 0, off_err
    assert on_rc == 0, on_err
    assert "worst-case 53687091200 bytes" in off_err, off_err
    assert "g.pinned: keeping the authored hint" in on_err, on_err
    assert "worst-case 53687091200 bytes" in on_err, on_err


def test_a_proven_floor_reaches_max_mem_admission_even_though_the_step_is_declined(
    tmp_path: Path,
) -> None:
    """Turning the flag ON must never model a step BELOW what its own history proves it used.

    One step, ten recorded runs, every one of them pinned to the ceiling it was given: nine at
    8589934592 B and one at 34359738368 B. The DAG authors 1073741824 B.

    * Without the flag the censoring-blind feedback fits the 9/10 percentile of those peaks —
      the ninth smallest, 8589934592 B — and ``--max-mem`` models 10737418240 B.
    * With the flag the step is DECLINED, because every peak is censored and none of them says
      what the step actually wanted. But one of them says the step reached 34359738368 B, so
      that is the floor the decline keeps, and admission models 42949672960 B.

    The flag therefore RAISES the modelled footprint here, which is the direction a censored
    sample is allowed to move it. Restoring the authored 1073741824 B instead — throwing the
    floor away with the estimate — makes the second number far smaller than the first, and both
    are named literally so that cannot pass.
    """
    machine, container = "synthhost", "affinity4_cpu-max-max"
    store = tmp_path / "store"
    store.mkdir()
    rows = [_row("g.pinned", peak="8589934592", cap="8589934592") for _ in range(9)]
    rows.append(_row("g.pinned", peak="34359738368", cap="34359738368"))
    _write_store(store, machine, container, rows)
    dag = tmp_path / "dag.json"
    dag.write_text(
        '{"steps": [{"group": "g", "job": "pinned", "cmd": "true", "deps": [],'
        ' "hint": {"rss_baseline_bytes": 1073741824}}]}',
        encoding="utf-8",
    )
    env = {
        "DAGRUN_MACHINE_ID": machine,
        "DAGRUN_CONTAINER_CLASS": container,
    }
    common = ["run", "--dag", str(dag), "--perf-dir", str(store), "--no-profile",
              "--unsafe-no-cgroups", "-q", "--max-mem", "400G"]

    off_rc, _o1, off_err = _run_cli(common, env)
    on_rc, _o2, on_err = _run_cli([*common, "--profile-memory-feedback"], env)

    assert off_rc == 0, off_err
    assert on_rc == 0, on_err
    assert "worst-case 10737418240 bytes" in off_err, off_err
    assert (
        "g.pinned: no estimate; rss_baseline_bytes=34359738368, the proven floor" in on_err
    ), on_err
    assert "worst-case 42949672960 bytes" in on_err, on_err


def test_asking_for_memory_feedback_with_reading_disabled_says_it_is_inert(
    tmp_path: Path,
) -> None:
    """``--no-profile-feedback`` turns the store reader off, which makes
    ``--profile-memory-feedback`` a no-op. Doing nothing quietly is the worst answer available:
    the caller asked for a learned cap by name and would run with the authored one believing
    otherwise. Say it, and keep exit 0 — the combination is legal, just empty.
    """
    machine, container = "synthhost", "affinity4_cpu-max-max"
    store = tmp_path / "store"
    store.mkdir()
    _write_store(store, machine, container, [_row("g.learned", peak=str(2 * GIB))
                                             for _ in range(6)])
    dag = tmp_path / "dag.json"
    dag.write_text(
        '{"steps": [{"group": "g", "job": "learned", "cmd": "true", "deps": []}]}',
        encoding="utf-8",
    )
    env = {
        "DAGRUN_MACHINE_ID": machine,
        "DAGRUN_CONTAINER_CLASS": container,
    }
    args = ["run", "--dag", str(dag), "--perf-dir", str(store), "--no-profile",
            "--unsafe-no-cgroups", "-q", "--profile-memory-feedback", "--no-profile-feedback"]

    rc, _out, err = _run_cli(args, env)

    assert rc == 0, err
    assert "--no-profile-feedback" in err
    assert "no estimate is derived" in err
    # And it really did not derive one.
    assert "rss_baseline_bytes=" not in err
