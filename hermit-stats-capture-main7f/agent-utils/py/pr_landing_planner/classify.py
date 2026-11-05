"""PURE CI classification: a status-check rollup -> a refined :class:`CiVerdict`.

This is the headline value of the planner. A naive lander treats every red check as a failure and is
"wildly wrong" (a CI-health analysis found 53% of a 24h window's failures were benign gate noise).
This module instead classifies WHY a PR is red, into the five real failure modes captured in
:class:`pr_landing_planner.model.RedClass`:

* ``STALE_REQUIRED_CHECK`` — underlying CI green on the head, but the required gate froze on a stale
  result -> refire the gate.
* ``EVALUATE_ONCE_RACE`` — the gate fired once while full CI was still queued and exited "still
  queued" -> benign; treat as pending.
* ``RUNNER_OUTAGE`` — the gate job never actually ran (blank runner / BlobNotFound / near-zero
  duration), usually across many branches -> escalate.
* ``FLAKY`` — a red whose check name / message matches a caller-supplied signature -> refire CI.
* ``REAL`` — anything else -> hold and dispatch a fix.

Everything here is pure and config-driven: the gate-check name, flaky signatures, and the race /
outage markers are all :class:`ClassifyConfig` supplied by the caller.
"""

from __future__ import annotations

import re
from collections.abc import Mapping, Sequence
from dataclasses import dataclass

from pr_landing_planner.check_outcome import (
    FAIL_CONCLUSIONS,
    classify_check,
    select_latest_checks,
)
from pr_landing_planner.model import CheckRun, CiState, CiVerdict, RedClass

#: Derived from the canonical ci-hub authority; retained as an exported name
#: for callers that use the failure set in outage signatures.
FAILED_CONCLUSIONS: frozenset[str] = frozenset(value.upper() for value in FAIL_CONCLUSIONS)

#: Default substrings that identify the benign "evaluate-once race" gate message.
DEFAULT_EVALUATE_ONCE_MARKERS: tuple[str, ...] = (
    "full ci still queued",
    "rerun after ci completes",
    "still queued",
)
#: Default substrings / regexes that identify a gate job that never actually ran.
DEFAULT_OUTAGE_MARKERS: tuple[str, ...] = ("blobnotfound", "no runner", "runner not found")
#: A gate check that concluded in under this many seconds is treated as "never really ran".
DEFAULT_OUTAGE_MAX_DURATION_SECS = 5


@dataclass(frozen=True)
class FlakySignature:
    """A caller-supplied signature that marks a red check as flaky rather than a real regression.

    A check matches when its (case-insensitive) NAME matches :attr:`name_regex` (if given) AND its
    message TEXT matches :attr:`text_regex` (if given). At least one of the two must be set. Both set
    means both must match. The maintenance story matters — a stale flaky list silently masks real
    reds — so signatures are explicit caller config loaded from ``--flaky-signatures FILE``.
    """

    name_regex: str = ""
    text_regex: str = ""
    note: str = ""

    def validate(self) -> None:
        """Reject malformed regular expressions before a planning snapshot is collected."""
        for field, pattern in (
            ("name_regex", self.name_regex),
            ("text_regex", self.text_regex),
        ):
            if pattern:
                try:
                    re.compile(pattern, re.IGNORECASE)
                except re.error as exc:
                    raise ValueError(
                        f"invalid flaky signature {field} {pattern!r}: {exc}"
                    ) from exc

    def matches(self, check: CheckRun) -> bool:
        """Return whether *check* satisfies every configured regular expression."""

        name_ok = True
        text_ok = True
        if self.name_regex:
            name_ok = re.search(self.name_regex, check.name, re.IGNORECASE) is not None
        if self.text_regex:
            text_ok = re.search(self.text_regex, check.text, re.IGNORECASE) is not None
        if not self.name_regex and not self.text_regex:
            return False
        return name_ok and text_ok


@dataclass(frozen=True)
class ClassifyConfig:
    """Everything the pure classifier needs, all caller-supplied (nothing project-specific baked in)."""

    gate_check: str = "merge-gate"
    flaky_signatures: tuple[FlakySignature, ...] = ()
    evaluate_once_markers: tuple[str, ...] = DEFAULT_EVALUATE_ONCE_MARKERS
    outage_markers: tuple[str, ...] = DEFAULT_OUTAGE_MARKERS
    outage_max_duration_secs: int = DEFAULT_OUTAGE_MAX_DURATION_SECS
    #: Minimum number of PRs showing the gate-never-ran signature to declare a systemic outage.
    outage_min_prs: int = 2

    def validate(self) -> None:
        """Validate caller-controlled names, regexes, and numeric domains."""
        if not self.gate_check.strip():
            raise ValueError("gate check name must be non-empty")
        if self.outage_max_duration_secs < 0 or self.outage_min_prs < 0:
            raise ValueError("outage thresholds must be nonnegative")
        for signature in self.flaky_signatures:
            signature.validate()


# --------------------------------------------------------------------------- rollup parsing
def _as_list(value: object) -> list[object]:
    return list(value) if isinstance(value, list) else []


def _get_str(m: Mapping[str, object], *keys: str) -> str:
    """Return the first present, string-coercible value among ``keys`` (nested dicts flattened)."""
    for key in keys:
        if key in m and m[key] is not None:
            val = m[key]
            if isinstance(val, str):
                return val
            if isinstance(val, (int, float, bool)):
                return str(val)
    return ""


def _get_int(m: Mapping[str, object], *keys: str) -> int | None:
    for key in keys:
        val = m.get(key)
        if isinstance(val, bool):
            continue
        if isinstance(val, int):
            return val
    return None


def parse_rollup(raw: object, *, head_sha: str = "") -> tuple[CheckRun, ...]:
    """Narrow a host's raw ``statusCheckRollup`` (a list of dicts) into :class:`CheckRun` values.

    This is the single Any-narrowing boundary for a rollup: ``raw`` is ``object`` and every field is
    re-typed here, so nothing untyped leaks into the pure classifier. Unknown shapes are skipped
    rather than crashing (a rollup entry that is not a dict is ignored)."""
    checks: list[CheckRun] = []
    for entry in select_latest_checks(raw, head_sha=head_sha):
        item: dict[str, object] = {str(k): v for k, v in entry.items()}
        name = _get_str(item, "name", "context")
        if not name:
            # A generic StatusContext uses `context`; a CheckRun uses `name`. Skip truly nameless.
            name = _get_str(item, "workflowName")
        status = _get_str(item, "status").upper()
        conclusion = _get_str(item, "conclusion").upper()
        if not conclusion:
            # A legacy StatusContext uses terminal `state` in place of the
            # CheckRun status/conclusion pair.
            conclusion = _get_str(item, "state").upper()
        checks.append(
            CheckRun(
                name=name,
                status=status,
                conclusion=conclusion,
                text=_get_str(item, "text", "title", "description", "summary"),
                workflow=_get_str(item, "workflowName", "workflow"),
                duration_secs=_get_int(item, "duration_secs", "durationSecs"),
            )
        )
    return tuple(checks)


# --------------------------------------------------------------------------- base state
def _check_state(check: CheckRun) -> CiState:
    outcome = classify_check(check.status, check.conclusion)
    return {
        "PASSED": CiState.PASSED,
        "FAILED": CiState.FAILED,
        "NO_RESULT": CiState.NO_RESULT,
    }[outcome]


def classify_state(checks: Sequence[CheckRun]) -> CiState:
    """Return PASSED/FAILED/NO_RESULT over the whole rollup."""
    if not checks:
        return CiState.NO_RESULT
    saw_failed = False
    saw_no_result = False
    for check in checks:
        state = _check_state(check)
        if state is CiState.FAILED:
            saw_failed = True
        elif state is CiState.NO_RESULT:
            saw_no_result = True
    if saw_failed:
        return CiState.FAILED
    if saw_no_result:
        return CiState.NO_RESULT
    return CiState.PASSED


# --------------------------------------------------------------------------- signatures
def _is_evaluate_once(check: CheckRun, markers: Sequence[str]) -> bool:
    text = check.text.lower()
    return any(marker.lower() in text for marker in markers)


def _looks_like_missing_run(check: CheckRun, cfg: ClassifyConfig) -> bool:
    text = check.text.lower()
    if any(marker.lower() in text for marker in cfg.outage_markers):
        return True
    if check.conclusion == "STARTUP_FAILURE":
        return True
    if (
        check.duration_secs is not None
        and check.duration_secs <= cfg.outage_max_duration_secs
        and check.conclusion in FAILED_CONCLUSIONS
    ):
        return True
    return False


def _is_flaky(check: CheckRun, signatures: Sequence[FlakySignature]) -> bool:
    return any(sig.matches(check) for sig in signatures)


# --------------------------------------------------------------------------- the classifier
def classify_pr(checks: Sequence[CheckRun], cfg: ClassifyConfig) -> CiVerdict:
    """Refine a PR's rollup into a :class:`CiVerdict` (raw state + a :class:`RedClass` when red-ish).

    Precedence for a red PR:

    1. the gate check shows the *never-ran* signature -> ``RUNNER_OUTAGE``;
    2. the ONLY reds are benign "still queued" gate messages -> ``EVALUATE_ONCE_RACE`` (as pending);
    3. non-gate CI is green and only the gate is red (stale result) -> ``STALE_REQUIRED_CHECK``;
    4. every red check matches a flaky signature -> ``FLAKY``;
    5. otherwise -> ``REAL``.
    """
    raw_state = classify_state(checks)
    gate = next((c for c in checks if c.name == cfg.gate_check), None)
    gate_present = gate is not None
    gate_ok = gate is not None and _check_state(gate) is CiState.PASSED
    gate_missing_run = gate is not None and _looks_like_missing_run(gate, cfg)

    if raw_state is not CiState.FAILED:
        return CiVerdict(
            raw_state=raw_state,
            red_class=None,
            gate_present=gate_present,
            gate_ok=gate_ok,
            gate_missing_run=gate_missing_run,
            detail=f"{raw_state.value}",
        )

    red_checks = [c for c in checks if _check_state(c) is CiState.FAILED]
    non_gate_checks = [c for c in checks if c.name != cfg.gate_check]
    non_gate_state = classify_state(non_gate_checks) if non_gate_checks else CiState.NO_RESULT

    # (1) Systemic outage: the gate job never ran.
    if gate_missing_run:
        return CiVerdict(
            raw_state, RedClass.RUNNER_OUTAGE, gate_present, gate_ok, True,
            detail=f"gate check {cfg.gate_check!r} never ran (runner outage signature)",
        )

    # (2) Evaluate-once race: the only reds are benign "still queued" gate messages.
    if red_checks and all(
        c.name == cfg.gate_check and _is_evaluate_once(c, cfg.evaluate_once_markers)
        for c in red_checks
    ):
        return CiVerdict(
            raw_state, RedClass.EVALUATE_ONCE_RACE, gate_present, gate_ok, False,
            detail="gate evaluated once while full CI was still queued (benign; treat as pending)",
        )

    # (3) Stale required check: real CI green, only the gate is stale-red.
    gate_is_red = gate is not None and _check_state(gate) is CiState.FAILED
    non_gate_reds = [c for c in red_checks if c.name != cfg.gate_check]
    if gate_is_red and not non_gate_reds and non_gate_state is CiState.PASSED:
        return CiVerdict(
            raw_state, RedClass.STALE_REQUIRED_CHECK, gate_present, gate_ok, False,
            detail=f"CI green on head; required gate {cfg.gate_check!r} is stale",
        )

    # (4) Flaky: every red check matches a caller signature.
    if red_checks and all(_is_flaky(c, cfg.flaky_signatures) for c in red_checks):
        return CiVerdict(
            raw_state, RedClass.FLAKY, gate_present, gate_ok, False,
            detail="all red checks match a known flaky signature; refire CI",
        )

    # (5) Real regression.
    names = ", ".join(sorted({c.name for c in red_checks if not _is_flaky(c, cfg.flaky_signatures)}))
    return CiVerdict(
        raw_state, RedClass.REAL, gate_present, gate_ok, False,
        detail=f"real red on: {names}" if names else "real red",
    )


def flaky_signatures_from_objs(raw: object) -> tuple[FlakySignature, ...]:
    """Narrow a loaded ``--flaky-signatures`` document (a list of objects) into signatures.

    Accepts a top-level list, or an object with a ``signatures`` list. Each entry is an object with
    optional ``name_regex`` / ``text_regex`` / ``note`` string fields (at least one regex required).
    """
    entries: list[object]
    if isinstance(raw, list):
        entries = list(raw)
    elif isinstance(raw, dict):
        sig_list = raw.get("signatures")
        entries = list(sig_list) if isinstance(sig_list, list) else []
    else:
        entries = []
    out: list[FlakySignature] = []
    for entry in entries:
        if not isinstance(entry, dict):
            continue
        obj: dict[str, object] = {str(k): v for k, v in entry.items()}
        name_re = obj.get("name_regex")
        text_re = obj.get("text_regex")
        note = obj.get("note")
        sig = FlakySignature(
            name_regex=name_re if isinstance(name_re, str) else "",
            text_regex=text_re if isinstance(text_re, str) else "",
            note=note if isinstance(note, str) else "",
        )
        if sig.name_regex or sig.text_regex:
            out.append(sig)
    return tuple(out)


__all__ = [
    "FAILED_CONCLUSIONS",
    "FlakySignature",
    "ClassifyConfig",
    "parse_rollup",
    "classify_state",
    "classify_pr",
    "flaky_signatures_from_objs",
    "DEFAULT_EVALUATE_ONCE_MARKERS",
    "DEFAULT_OUTAGE_MARKERS",
]
