"""Hermetic three-state interpretation and selection of CI checks.

A missing answer is neither a passing nor a failing answer. Unknown values are
therefore kept as ``NO_RESULT`` so callers fail closed without inventing a
verdict. The module is deliberately self-contained: importing the planner never
loads code or data from the network or from a neighboring checkout.
"""

from __future__ import annotations

import re
from collections.abc import Mapping

PASS_CONCLUSIONS: frozenset[str] = frozenset(("success",))
FAIL_CONCLUSIONS: frozenset[str] = frozenset(
    ("failure", "timed_out", "error", "startup_failure")
)

_RUN_URL = re.compile(r"/actions/runs/(\d+)(?:/|$)")


def _text(value: object) -> str:
    return str(value or "").strip()


def _check_context(check: Mapping[str, object]) -> str:
    return _text(check.get("name") or check.get("context"))


def _check_head(check: Mapping[str, object]) -> str:
    return _text(
        check.get("headSha")
        or check.get("head_sha")
        or check.get("headRefOid")
    )


def _run_id(check: Mapping[str, object]) -> int:
    for key in ("runId", "run_id"):
        value = check.get(key)
        if not value:
            continue
        if not isinstance(value, (str, int, float)):
            continue
        try:
            return int(value)
        except (TypeError, ValueError):
            pass
    url = _text(
        check.get("detailsUrl")
        or check.get("details_url")
        or check.get("url")
        or check.get("html_url")
    )
    match = _RUN_URL.search(url)
    return int(match.group(1)) if match is not None else 0


def _timestamp(check: Mapping[str, object]) -> str:
    # A queued check can expose startedAt=0001-01-01. Treat that sentinel as
    # absent so it cannot make a newer queued run look older than a completed
    # predecessor. A run ID remains the primary ordering key when present.
    for key in ("createdAt", "created_at", "startedAt", "started_at", "completedAt"):
        value = _text(check.get(key))
        if value and not value.startswith("0001-01-01"):
            return value
    return ""


def _ambiguous_check(context: str) -> dict[str, object]:
    return {
        "name": context,
        "status": "AMBIGUOUS",
        "conclusion": "",
        "_selectionError": (
            "duplicate check context has equal ordering identity and contrary verdicts"
        ),
    }


def select_latest_checks(value: object, *, head_sha: str = "") -> list[dict[str, object]]:
    """Return one deterministically newest check per context.

    Head-bearing entries are restricted to ``head_sha``. Newness is ordered by
    workflow run ID and timestamp. Contrary duplicates with the same identity
    become an explicit ambiguity instead of depending on input order.
    """
    if isinstance(value, Mapping):
        value = value.get("statusCheckRollup", value.get("check_runs", []))
    if not isinstance(value, list):
        return []

    latest: dict[str, tuple[tuple[int, str], int, dict[str, object]]] = {}
    order: list[str] = []
    for index, raw in enumerate(value):
        if not isinstance(raw, Mapping):
            continue
        check = {str(key): item for key, item in raw.items()}
        observed_head = _check_head(check)
        if head_sha and observed_head and observed_head != head_sha:
            continue
        context = _check_context(check)
        key = context or f"\0unnamed-{index}"
        candidate_key = (_run_id(check), _timestamp(check))
        previous_record = latest.get(key)
        if previous_record is None:
            order.append(key)
            latest[key] = (candidate_key, index, check)
            continue

        previous_key, previous_index, previous = previous_record
        if candidate_key > previous_key:
            latest[key] = (candidate_key, index, check)
        elif candidate_key == previous_key:
            same_verdict = (
                _text(previous.get("status")) == _text(check.get("status"))
                and _text(previous.get("conclusion") or previous.get("state"))
                == _text(check.get("conclusion") or check.get("state"))
            )
            if not same_verdict:
                latest[key] = (
                    candidate_key,
                    max(previous_index, index),
                    _ambiguous_check(context),
                )
            elif index > previous_index:
                latest[key] = (candidate_key, index, check)
    return [latest[key][2] for key in order]


def classify_check(status: object, conclusion: object) -> str:
    """Return ``PASSED``, ``FAILED``, or ``NO_RESULT`` for one check."""
    normalized_status = _text(status).lower()
    normalized_conclusion = _text(conclusion).lower()
    if normalized_status and normalized_status != "completed":
        return "NO_RESULT"
    if normalized_conclusion in PASS_CONCLUSIONS:
        return "PASSED"
    if normalized_conclusion in FAIL_CONCLUSIONS:
        return "FAILED"
    return "NO_RESULT"
