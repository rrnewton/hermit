"""Tests for the pure CI classifier: base state + the five red classifications."""

from __future__ import annotations

from pr_landing_planner.classify import (
    ClassifyConfig,
    FlakySignature,
    classify_pr,
    classify_state,
    flaky_signatures_from_objs,
    parse_rollup,
)
from pr_landing_planner.model import CheckRun, CiState, RedClass


def _check(name: str, conclusion: str = "", status: str = "COMPLETED", text: str = "",
           duration: int | None = None) -> CheckRun:
    return CheckRun(name=name, status=status, conclusion=conclusion, text=text, duration_secs=duration)


CFG = ClassifyConfig(gate_check="merge-gate")


def test_classify_state_green_red_pending_none() -> None:
    assert classify_state([]) is CiState.NO_RESULT
    assert classify_state([_check("CI", "SUCCESS")]) is CiState.PASSED
    assert classify_state([_check("CI", "SUCCESS"), _check("x", "FAILURE")]) is CiState.FAILED
    assert classify_state([_check("CI", "", "IN_PROGRESS")]) is CiState.NO_RESULT
    # A single genuine failure dominates passes and no-results.
    assert classify_state([_check("a", "SUCCESS"), _check("b", "TIMED_OUT")]) is CiState.FAILED


def test_two_passes_four_failures_eleven_no_results() -> None:
    passes = (
        _check("check-run", "SUCCESS", "COMPLETED"),
        _check("status-context", "SUCCESS", ""),
    )
    assert all(classify_state([check]) is CiState.PASSED for check in passes)

    failures = ("FAILURE", "TIMED_OUT", "ERROR", "STARTUP_FAILURE")
    assert all(classify_state([_check("CI", value)]) is CiState.FAILED for value in failures)

    no_results = (
        _check("CI", "CANCELLED"),
        _check("CI", "SKIPPED"),
        _check("CI", "NEUTRAL"),
        _check("CI", "STALE"),
        _check("CI", "ACTION_REQUIRED"),
        _check("CI", "", "QUEUED"),
        _check("CI", "", "IN_PROGRESS"),
        _check("CI", "", "WAITING"),
        _check("CI", "", "REQUESTED"),
        _check("CI", "", "MISSING"),
        _check("CI", "FUTURE_STATE"),
    )
    assert all(classify_state([check]) is CiState.NO_RESULT for check in no_results)


def test_cancelled_required_gate_neither_satisfies_nor_fails() -> None:
    verdict = classify_pr([_check("merge-gate", "CANCELLED")], CFG)
    assert verdict.raw_state is CiState.NO_RESULT
    assert not verdict.gate_ok
    assert verdict.red_class is None


def test_green_fresh_gate_ok() -> None:
    v = classify_pr([_check("CI", "SUCCESS"), _check("merge-gate", "SUCCESS")], CFG)
    assert v.raw_state is CiState.PASSED
    assert v.red_class is None
    assert v.gate_present and v.gate_ok


def test_stale_required_check() -> None:
    # Real CI green on the head, but the required gate froze red (ds-4171).
    v = classify_pr(
        [_check("CI", "SUCCESS"), _check("merge-gate", "FAILURE", text="stale")], CFG
    )
    assert v.raw_state is CiState.FAILED
    assert v.red_class is RedClass.STALE_REQUIRED_CHECK


def test_evaluate_once_race_is_benign() -> None:
    v = classify_pr(
        [
            _check("CI", "", "IN_PROGRESS"),
            _check("merge-gate", "FAILURE", text="Full CI still queued; rerun after CI completes"),
        ],
        CFG,
    )
    assert v.red_class is RedClass.EVALUATE_ONCE_RACE


def test_non_gate_failure_with_race_words_remains_real() -> None:
    verdict = classify_pr(
        [
            _check("unit", "FAILURE", text="dependency still queued"),
            _check("merge-gate", "SUCCESS"),
        ],
        CFG,
    )
    assert verdict.red_class is RedClass.REAL


def test_runner_outage_by_marker_and_duration() -> None:
    v1 = classify_pr(
        [_check("CI", "SUCCESS"), _check("merge-gate", "FAILURE", text="BlobNotFound", duration=3)],
        CFG,
    )
    assert v1.red_class is RedClass.RUNNER_OUTAGE
    assert v1.gate_missing_run
    v2 = classify_pr([_check("merge-gate", "STARTUP_FAILURE", text="no runner")], CFG)
    assert v2.red_class is RedClass.RUNNER_OUTAGE


def test_flaky_requires_a_signature() -> None:
    checks = [_check("wasm-core", "FAILURE", text="browser flake"), _check("merge-gate", "SUCCESS")]
    # Without a signature the wasm-core red is a REAL regression.
    assert classify_pr(checks, CFG).red_class is RedClass.REAL
    # With a matching signature it is reclassified flaky.
    cfg = ClassifyConfig(
        gate_check="merge-gate", flaky_signatures=(FlakySignature(name_regex="wasm-core"),)
    )
    assert classify_pr(checks, cfg).red_class is RedClass.FLAKY


def test_flaky_only_when_all_reds_match() -> None:
    checks = [
        _check("wasm-core", "FAILURE", text="flake"),
        _check("unit", "FAILURE", text="assertion failed"),
        _check("merge-gate", "SUCCESS"),
    ]
    cfg = ClassifyConfig(
        gate_check="merge-gate", flaky_signatures=(FlakySignature(name_regex="wasm-core"),)
    )
    # unit's red is not covered by any signature => the PR is a REAL red, not flaky.
    assert classify_pr(checks, cfg).red_class is RedClass.REAL


def test_real_red() -> None:
    v = classify_pr(
        [_check("unit", "FAILURE", text="assertion"), _check("merge-gate", "SUCCESS")], CFG
    )
    assert v.red_class is RedClass.REAL
    assert "unit" in v.detail


def test_outage_precedes_stale_and_flaky() -> None:
    # Even with a flaky signature present, a never-ran gate is an OUTAGE, not flaky.
    cfg = ClassifyConfig(
        gate_check="merge-gate", flaky_signatures=(FlakySignature(name_regex="merge-gate"),)
    )
    v = classify_pr([_check("merge-gate", "FAILURE", text="BlobNotFound", duration=1)], cfg)
    assert v.red_class is RedClass.RUNNER_OUTAGE


def test_parse_rollup_narrows_gh_json() -> None:
    raw: object = [
        {"name": "CI", "status": "COMPLETED", "conclusion": "SUCCESS"},
        {"context": "legacy-status", "state": "SUCCESS"},
        "garbage-entry-ignored",
    ]
    checks = parse_rollup(raw)
    assert len(checks) == 2
    assert checks[0].name == "CI" and checks[0].conclusion == "SUCCESS"
    assert checks[1].name == "legacy-status" and checks[1].conclusion == "SUCCESS"
    assert classify_state(checks) is CiState.PASSED


def test_parse_rollup_uses_latest_exact_head_attempt_in_both_orders() -> None:
    sha = "a" * 40
    older = {
        "name": "merge-gate",
        "headSha": sha,
        "status": "COMPLETED",
        "conclusion": "FAILURE",
        "startedAt": "2026-08-04T15:12:05Z",
        "detailsUrl": "https://github.com/o/r/actions/runs/30922888575/job/1",
    }
    newer = {
        "name": "merge-gate",
        "headSha": sha,
        "status": "COMPLETED",
        "conclusion": "SUCCESS",
        "startedAt": "2026-08-04T15:24:36Z",
        "detailsUrl": "https://github.com/o/r/actions/runs/30923975433/job/2",
    }
    wrong_head = {**newer, "headSha": "b" * 40, "conclusion": "FAILURE"}
    for raw in ([older, newer, wrong_head], [wrong_head, newer, older]):
        checks = parse_rollup(raw, head_sha=sha)
        assert len(checks) == 1
        assert checks[0].conclusion == "SUCCESS"
        assert classify_state(checks) is CiState.PASSED


def test_parse_rollup_tied_contrary_verdicts_are_no_result_in_both_orders() -> None:
    failure = {
        "name": "merge-gate",
        "status": "COMPLETED",
        "conclusion": "FAILURE",
        "startedAt": "2026-08-04T15:24:36Z",
        "detailsUrl": "https://github.com/o/r/actions/runs/11/job/100",
    }
    success = {
        **failure,
        "conclusion": "SUCCESS",
        "detailsUrl": "https://github.com/o/r/actions/runs/11/job/101",
    }
    for raw in ([failure, success], [success, failure]):
        checks = parse_rollup(raw)
        assert len(checks) == 1
        assert classify_state(checks) is CiState.NO_RESULT


def test_flaky_signatures_from_objs() -> None:
    doc: object = {"signatures": [{"name_regex": "wasm"}, {"text_regex": "flake"}, {"note": "no regex"}]}
    sigs = flaky_signatures_from_objs(doc)
    assert len(sigs) == 2  # the note-only entry (no regex) is dropped
    doc_list: object = [{"name_regex": "x"}]
    assert len(flaky_signatures_from_objs(doc_list)) == 1
