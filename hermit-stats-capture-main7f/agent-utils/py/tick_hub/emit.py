"""The machine-readable output contract: pure HEALTH / ACTION / NOTE / ERROR line formatters.

Every tick emits one of four line types on stdout; a caller parses by the leading token:

    HEALTH: <check> <ok|stale|missing> age_secs=<N|NA> threshold_secs=<N> detail="..."
    ACTION: <skill> [key=value ...] title="..."
    NOTE:   <free text>
    ERROR:  <text>

Lines are independent; parse by the leading token. These formatters are pure (no I/O) so the exact
bytes are unit-testable and stable. Field VALUES are bare when safe and double-quoted (with ``\\``
and ``"`` escaped) when they contain whitespace, a quote, or are empty; the ``title`` is always
quoted. This mirrors the well-worn contract that this tool generalizes.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence

HEALTH_STATUS_OK = "ok"
HEALTH_STATUS_STALE = "stale"
HEALTH_STATUS_MISSING = "missing"


def _quote(value: str) -> str:
    """Double-quote a value, escaping backslashes and quotes."""
    escaped = value.replace("\\", "\\\\").replace('"', '\\"')
    return f'"{escaped}"'


def _needs_quote(value: str) -> bool:
    return value == "" or any(ch.isspace() for ch in value) or '"' in value or "\\" in value


def _fmt_value(value: str) -> str:
    return _quote(value) if _needs_quote(value) else value


def format_action(skill: str, fields: Mapping[str, str], title: str) -> str:
    """``ACTION: <skill> [k=v ...] [title="..."]`` (title appended only when non-empty)."""
    parts = [f"ACTION: {skill}"]
    for key, value in fields.items():
        parts.append(f"{key}={_fmt_value(value)}")
    if title:
        parts.append(f"title={_quote(title)}")
    return " ".join(parts)


def format_note(text: str) -> str:
    """Format an informational ``NOTE`` record."""
    return f"NOTE: {text}"


def format_no_result(name: str, detail: str = "") -> str:
    """Format a gate that ran but could not determine its condition.

    Built without the configured emit template and never through
    :func:`render_emit`. A captured summary may be appended as optional detail,
    but the name alone is sufficient, so a gate that printed nothing can still
    report its real state instead of an unresolved-placeholder fault.
    """
    detail = detail.strip()
    tail = f" ({detail})" if detail else ""
    return f"NO_RESULT: {name} could not determine its condition; this is not a pass{tail}"


def format_unevaluable(name: str, dependencies: Sequence[str]) -> str:
    """Format a quiet reminder whose declared dependency has no result."""
    joined = ",".join(dependencies)
    return (
        f"NO_RESULT: {name} is unevaluable because dependency "
        f"{joined} could not determine its condition; this is not a pass"
    )


def format_clean(name: str) -> str:
    """Format a gate that RAN and found nothing wrong.

    ⚠️ THIS EXISTS BECAUSE SILENCE MEANT TWO THINGS. In an ordinary tick a clean
    gate emits nothing, and so does a gate that was never reached — the reader
    cannot tell "checked, fine" from "not checked". Every other outcome already
    has a line; this was the one that did not, and its absence is the ambiguity a
    pending report exists to remove.
    """
    return f"CLEAN: {name} ran and found nothing to report"


def format_suppressed(name: str, missing_flags: Sequence[str]) -> str:
    """Format a reminder NOT evaluated because a required flag is off.

    Distinct from CLEAN and from every NO-SIGNAL: nothing was measured, and the
    reason is configuration rather than failure. Naming the flags lets the reader
    act instead of wondering why the gate is missing from the report.
    """
    flags = ",".join(missing_flags) or "(none recorded)"
    return f"SUPPRESSED: {name} did not run; required flag(s) not set: {flags}"


def format_error(text: str) -> str:
    """Format an operational ``ERROR`` record."""
    return f"ERROR: {text}"


def format_health(
    name: str, status: str, age_secs: int | None, threshold_secs: int, detail: str
) -> str:
    """``HEALTH: <name> <status> age_secs=<N|NA> threshold_secs=<N> detail="..."``."""
    age = "NA" if age_secs is None else str(age_secs)
    escaped_detail = detail.replace("\\", "\\\\").replace('"', '\\"')
    return (
        f"HEALTH: {name} {status} age_secs={age} "
        f'threshold_secs={threshold_secs} detail="{escaped_detail}"'
    )
