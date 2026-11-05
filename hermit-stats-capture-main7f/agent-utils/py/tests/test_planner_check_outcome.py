"""Hermeticity and deterministic-selection tests for the packaged CI authority."""

from __future__ import annotations

from pr_landing_planner.check_outcome import classify_check, select_latest_checks


def test_three_state_classification_fails_closed() -> None:
    assert classify_check("COMPLETED", "SUCCESS") == "PASSED"
    assert classify_check("COMPLETED", "FAILURE") == "FAILED"
    assert classify_check("COMPLETED", "TIMED_OUT") == "FAILED"
    assert classify_check("IN_PROGRESS", "") == "NO_RESULT"
    assert classify_check("COMPLETED", "NEUTRAL") == "NO_RESULT"
    assert classify_check("COMPLETED", "FUTURE_VALUE") == "NO_RESULT"


def test_latest_check_is_exact_head_and_run_id_selected() -> None:
    selected = select_latest_checks(
        [
            {
                "name": "CI",
                "headSha": "old",
                "runId": 99,
                "status": "COMPLETED",
                "conclusion": "FAILURE",
            },
            {
                "name": "CI",
                "headSha": "head",
                "runId": 1,
                "status": "COMPLETED",
                "conclusion": "FAILURE",
            },
            {
                "name": "CI",
                "headSha": "head",
                "runId": 2,
                "status": "COMPLETED",
                "conclusion": "SUCCESS",
            },
        ],
        head_sha="head",
    )
    assert len(selected) == 1
    assert selected[0]["runId"] == 2
    assert selected[0]["conclusion"] == "SUCCESS"


def test_equal_identity_contrary_checks_become_ambiguous() -> None:
    selected = select_latest_checks(
        [
            {"name": "CI", "runId": 7, "status": "COMPLETED", "conclusion": "FAILURE"},
            {"name": "CI", "runId": 7, "status": "COMPLETED", "conclusion": "SUCCESS"},
        ]
    )
    assert selected == [
        {
            "name": "CI",
            "status": "AMBIGUOUS",
            "conclusion": "",
            "_selectionError": (
                "duplicate check context has equal ordering identity and contrary verdicts"
            ),
        }
    ]
