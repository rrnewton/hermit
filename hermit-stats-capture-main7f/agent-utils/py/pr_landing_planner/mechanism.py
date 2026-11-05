"""Normalize derived identifiers into stable mechanism categories for conflict analysis.

The same mechanism can appear under several spellings, such as
``concurrency.cancel-in-progress`` and ``CANCEL_IN_PROGRESS``. Classification maps those concrete
identifiers to :class:`Mechanism` values before the graph groups related pull requests. Unknown
identifiers remain unclassified so callers can review them without false matches.
"""

from __future__ import annotations

import re
from enum import Enum

#: The stable mechanism vocabulary. Seeded from what actually collided or duplicated on 2026-08-03,
#: not invented — four of these six caused a real problem that night (cancel-in-progress, the PR
#: auto-trigger, the locally-validated label, and the merge-gate required-check set). Extend it only
#: by adding a member here (plus its aliases below) after an UNCLASSIFIED candidate is recognised as
#: genuinely new. The ``value`` is the canonical string emitted in every output.
class Mechanism(Enum):
    """Canonical mechanism categories used to group semantic overlaps."""

    CANCEL_IN_PROGRESS = "cancel-in-progress"
    PR_AUTO_TRIGGER = "pr-auto-trigger"
    DAG_SCHEDULER_WIDTH = "dag-scheduler-width"
    LOCALLY_VALIDATED_LABEL = "locally-validated-label"
    VALIDATE_LEDGER_PATH = "validate-ledger-path"
    MERGE_GATE_REQUIRED_CHECKS = "merge-gate-required-checks"


#: Normalised alias cores per mechanism — the spellings CLASSIFY recognises. Each alias is already in
#: normalised form (see :func:`_normalize`): lowercase, non-alphanumeric runs collapsed to ``-``.
#: Aliases must be distinctive multi-token cores so hyphen-boundary substring matching cannot false-
#: positive on an unrelated identifier. When a real UNCLASSIFIED string turns out to be a known
#: mechanism under a new spelling, add that spelling's normalised form here — that is the whole
#: "extend the classifier" step.
_ALIASES: dict[Mechanism, tuple[str, ...]] = {
    Mechanism.CANCEL_IN_PROGRESS: ("cancel-in-progress",),
    Mechanism.PR_AUTO_TRIGGER: (
        "pr-auto-trigger",
        "on-pull-request",
        "pull-request-trigger",
        "pr-trigger",
        "pr-triggers",
    ),
    Mechanism.DAG_SCHEDULER_WIDTH: (
        "dag-scheduler-width",
        "ci-dag-jobs",
        "dag-jobs",
        "dag-width",
    ),
    Mechanism.LOCALLY_VALIDATED_LABEL: (
        "locally-validated-label",
        "locally-validated",
        "validated-locally",
    ),
    Mechanism.VALIDATE_LEDGER_PATH: (
        "validate-ledger-path",
        "ledger-path",
        "validate-ledger",
    ),
    Mechanism.MERGE_GATE_REQUIRED_CHECKS: (
        "merge-gate-required-checks",
        "merge-gate",
        "required-checks",
    ),
}

_NON_ALNUM = re.compile(r"[^a-z0-9]+")


def _normalize(raw: str) -> str:
    """Fold a derived string to a canonical spelling: lowercase, non-alphanumeric runs -> single ``-``.

    ``concurrency.cancel-in-progress`` / ``CANCEL_IN_PROGRESS`` / ``cancel_in_progress`` all fold to
    the same normalised core, which is exactly what lets recognition see one mechanism across spellings.
    """
    return _NON_ALNUM.sub("-", raw.lower()).strip("-")


def _alias_matches(normalized: str, alias: str) -> bool:
    """True when ``alias`` appears in ``normalized`` on hyphen boundaries (not mid-token).

    Boundary-aware so ``cancel-in-progress`` matches ``concurrency-cancel-in-progress`` but a bare
    token can never match a longer word it is merely a prefix/suffix substring of.
    """
    return (
        normalized == alias
        or normalized.startswith(f"{alias}-")
        or normalized.endswith(f"-{alias}")
        or f"-{alias}-" in normalized
    )


def classify(raw: str) -> Mechanism | None:
    """CLASSIFY one derived string into a :class:`Mechanism`, or ``None`` (UNCLASSIFIED).

    Deterministic recognition against :data:`_ALIASES`. Returns ``None`` when nothing matches — the
    valid, load-bearing "this may be a NEW mechanism" output. Mechanisms are checked in declaration
    order so a (table-bug) overlapping alias resolves deterministically rather than by dict chance.
    """
    normalized = _normalize(raw)
    if not normalized:
        return None
    for mechanism in Mechanism:
        if any(_alias_matches(normalized, alias) for alias in _ALIASES[mechanism]):
            return mechanism
    return None


# --------------------------------------------------------------------------- DERIVE (from a raw diff)
# SCREAMING_SNAKE env-var / const identifiers (>= 3 chars so short caps like "CI" or "ID" are ignored).
_SCREAMING_SNAKE = re.compile(r"\b[A-Z][A-Z0-9]*(?:_[A-Z0-9]+)+\b")
# A YAML-ish key at the start of an added line (``  cancel-in-progress: true``).
_YAML_KEY = re.compile(r"^\s*([A-Za-z][A-Za-z0-9_.-]*)\s*:")


def derive_symbols_from_diff(diff_text: str) -> tuple[str, ...]:
    """DERIVE mechanism-candidate strings from a unified diff — mechanical, no agent, added lines only.

    Pulls SCREAMING_SNAKE consts/env vars and YAML-ish keys from ``+`` lines. This is the EXACT-but-
    unnormalised stage; :func:`classify` is what folds these varied spellings onto one enum value.
    The result is sorted and de-duplicated for determinism. (Live-host wiring — feeding a real
    ``git diff`` here — is the remaining mechanical plumbing; the fixture path supplies these directly.)
    """
    symbols: set[str] = set()
    for line in diff_text.splitlines():
        if not line.startswith("+") or line.startswith("+++"):
            continue
        body = line[1:]
        symbols.update(_SCREAMING_SNAKE.findall(body))
        key = _YAML_KEY.match(body)
        if key is not None:
            symbols.add(key.group(1))
    return tuple(sorted(symbols))


__all__ = ["Mechanism", "classify", "derive_symbols_from_diff"]
