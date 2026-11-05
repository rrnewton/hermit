"""Pure parsing and application of caller-supplied landing context.

GitHub exposes checks and labels, but an external coordinator owns facts the generic
planner cannot infer: an exact-head/base local validation record, exact-head review
receipts, the assigned landing agent, and whether a change alters gate policy. ``--landing-context``
supplies those facts without baking one project's ledger or task system into this
package.
"""

from __future__ import annotations

import hashlib
import re
from collections.abc import Mapping, Sequence
from dataclasses import dataclass, replace

from pr_landing_planner.model import (
    CiState,
    PolicyClass,
    PrNode,
    ReviewEvidenceSnapshot,
    ValidationAuthority,
    ValidationEvidence,
)

AGENT_PREFIX = "agent:"
POLICY_PREFIX = "landing-policy:"
REQUIRED_REVIEW_LANES = ("codex", "claude")
ALLOWED_RETIREMENT_PERMISSIONS = frozenset(("triage", "write", "maintain", "admin"))
_AGENT_TOKEN = r"[A-Za-z0-9_.-]+"
_HORIZONTAL_ASCII_WHITESPACE = r"[ \t]"
_ASCII_WHITESPACE = " \t\r\n\v\f"
_ASCII_CASE = re.ASCII | re.IGNORECASE

_RETIREMENT_TARGET = re.compile(
    rf"^{_HORIZONTAL_ASCII_WHITESPACE}*RETIRES{_HORIZONTAL_ASCII_WHITESPACE}+"
    rf"#?([0-9]{{6,}}){_HORIZONTAL_ASCII_WHITESPACE}*\r?$",
    _ASCII_CASE,
)
_WITHDRAWAL = re.compile(
    rf"^CHANGES-REQUESTED-WITHDRAWN-AT:{_HORIZONTAL_ASCII_WHITESPACE}*"
    rf"(?P<lane>claude|codex){_HORIZONTAL_ASCII_WHITESPACE}+"
    r"(?P<head>[0-9a-f]{40})",
    _ASCII_CASE,
)
_BLOCK_PREFIX = re.compile(r"^(?:#{1,6}[ \t]+|[-+*][ \t]+)")
_FENCE = re.compile(r"^ {0,3}(?P<f>`{3,}|~{3,})[ \t]*(?P<info>.*)$")
_DISCLOSURE = re.compile(
    rf"^\[[A-Za-z0-9_.-]+,{_HORIZONTAL_ASCII_WHITESPACE}*"
    rf"[A-Za-z0-9_.-]+,{_HORIZONTAL_ASCII_WHITESPACE}*[^,\[\]\r\n]+,"
    rf"{_HORIZONTAL_ASCII_WHITESPACE}*[A-Za-z0-9_.-]+,"
    rf"{_HORIZONTAL_ASCII_WHITESPACE}*role=[A-Za-z0-9_.-]+\]\r?$",
    _ASCII_CASE,
)
_REVIEW_MARKER = re.compile(
    r"^(?:CHANGES-REQUESTED-WITHDRAWN-AT|CHANGES-REQUESTED-AT|APPROVED-AT):"
    rf"{_HORIZONTAL_ASCII_WHITESPACE}*(?:claude|codex)"
    rf"{_HORIZONTAL_ASCII_WHITESPACE}+[0-9a-f]{{40}}",
    _ASCII_CASE,
)
_BY_IDENTITY = re.compile(rf"[ \t]+BY[ \t]+{_AGENT_TOKEN}$", _ASCII_CASE)
_WHO_METADATA = re.compile(rf"^Unverified --who metadata: {_AGENT_TOKEN}\r?$", re.ASCII)
_COMMENT_OBJECTION = re.compile(
    rf"^CHANGES-REQUESTED-AT:{_HORIZONTAL_ASCII_WHITESPACE}*"
    rf"(?:claude|codex){_HORIZONTAL_ASCII_WHITESPACE}+[0-9a-f]{{40}}",
    _ASCII_CASE,
)


@dataclass(frozen=True)
class LandingContext:
    """Caller-owned landing facts for one PR, guarded by fetched commit identities."""

    pr: int
    head_sha: str = ""
    base_sha: str = ""
    assigned_agent: str = ""
    validation_evidence: ValidationEvidence | None = None
    validation_authority: ValidationAuthority | None = None
    review_pass_heads: tuple[tuple[str, str], ...] = ()
    review_objections_resolved: bool = False
    review_evidence_digest: str = ""
    policy_class: PolicyClass | None = None


@dataclass(frozen=True)
class RetirementRecord:
    """Exact objection-retirement linkage, without optional attribution text."""

    target_comment_id: str
    lane: str
    head_sha: str


def _str_field(obj: Mapping[str, object], key: str) -> str:
    value = obj.get(key)
    return value if isinstance(value, str) else ""


def _exact_sha(value: str) -> bool:
    return len(value) == 40 and all(char in "0123456789abcdef" for char in value)


def _exact_sha256(value: str) -> bool:
    return len(value) == 64 and all(char in "0123456789abcdef" for char in value)


def _ascii_strip(value: str) -> str:
    return value.strip(_ASCII_WHITESPACE)


def _prose_line_indexes(body: str) -> tuple[int, ...]:
    """Return indexes of comment lines outside fenced and indented code blocks."""

    indexes: list[int] = []
    fence = ""
    indented = False
    previous_blank = True
    for index, raw in enumerate(body.split("\n")):
        blank = not _ascii_strip(raw)
        if fence:
            match = _FENCE.match(raw)
            if (
                match is not None
                and match.group("f")[0] == fence[0]
                and len(match.group("f")) >= len(fence)
                and not _ascii_strip(match.group("info"))
            ):
                fence = ""
            previous_blank = blank
            continue
        match = _FENCE.match(raw)
        if match is not None:
            fence = match.group("f")
            indented = False
            previous_blank = False
            continue
        if indented:
            if blank:
                previous_blank = True
                continue
            if raw.startswith(("    ", "\t")):
                continue
            indented = False
        elif previous_blank and raw.startswith(("    ", "\t")) and not blank:
            indented = True
            previous_blank = False
            continue
        indexes.append(index)
        previous_blank = blank
    return tuple(indexes)


def _prose_lines(body: str) -> tuple[str, ...]:
    """Return comment lines outside fenced and indented code blocks."""

    lines = body.split("\n")
    return tuple(lines[index] for index in _prose_line_indexes(body))


def _undecorate(line: str) -> str:
    normalized = _ascii_strip(_BLOCK_PREFIX.sub("", _ascii_strip(line)))
    while True:
        for wrapper in ("`", "**", "__", "*", "_"):
            if (
                normalized.startswith(wrapper)
                and normalized.endswith(wrapper)
                and len(normalized) > 2 * len(wrapper)
            ):
                normalized = _ascii_strip(
                    normalized[len(wrapper) : -len(wrapper)]
                )
                break
        else:
            return normalized


def _marker_match(pattern: re.Pattern[str], line: str) -> re.Match[str] | None:
    """Match the marker core without interpreting trailing optional metadata."""

    normalized = _undecorate(line)
    match = pattern.match(normalized)
    if match is None:
        return None
    end = match.end()
    return match if end == len(normalized) or normalized[end] in " \t" else None


def retirement_record(body: str) -> RetirementRecord | None:
    """Return one exact retirement linkage, ignoring optional attribution text."""

    lines = _prose_lines(body)
    targets = [match for line in lines if (match := _RETIREMENT_TARGET.match(line))]
    if not targets:
        return None
    if len(targets) != 1:
        raise ValueError("review evidence retirement must name exactly one target")
    withdrawals = [
        match for line in lines if (match := _marker_match(_WITHDRAWAL, line))
    ]
    if len(withdrawals) != 1:
        raise ValueError("review evidence retirement needs one canonical withdrawal")
    withdrawal = withdrawals[0]
    return RetirementRecord(
        target_comment_id=targets[0].group(1),
        lane=withdrawal.group("lane").lower(),
        head_sha=withdrawal.group("head").lower(),
    )


def _normalized_review_body(body: str) -> str:
    """Remove only optional attribution metadata from review evidence."""

    lines = body.split("\n")
    prose_indexes = frozenset(_prose_line_indexes(body))
    first_nonblank = next(
        (index for index, line in enumerate(lines) if _ascii_strip(line)), None
    )
    normalized: list[str] = []
    for index, raw in enumerate(lines):
        if (
            index == first_nonblank
            and index in prose_indexes
            and _DISCLOSURE.fullmatch(raw) is not None
        ):
            continue
        if index in prose_indexes and _WHO_METADATA.fullmatch(raw) is not None:
            continue
        line = raw
        if index in prose_indexes and _marker_match(_REVIEW_MARKER, raw) is not None:
            normalized_line = _undecorate(raw)
            claimed_identity = _BY_IDENTITY.search(normalized_line)
            if claimed_identity is not None:
                claimed_text = claimed_identity.group(0)
                claimed_start = raw.rfind(claimed_text)
                if claimed_start >= 0:
                    line = (
                        raw[:claimed_start]
                        + raw[claimed_start + len(claimed_text) :]
                    )
        normalized.append(line)
    return "\n".join(normalized)


def has_comment_changes_requested(snapshot: ReviewEvidenceSnapshot) -> bool:
    """Return whether the complete snapshot contains a canonical comment refusal."""

    return any(
        event.kind in ("issue-comment", "review-comment")
        and any(
            _marker_match(_COMMENT_OBJECTION, line) is not None
            for line in _prose_lines(event.body)
        )
        for event in snapshot.events
    )


def review_evidence_digest(snapshot: ReviewEvidenceSnapshot) -> str:
    """Digest one complete exact-head review/comment event set canonically."""

    if not _exact_sha(snapshot.head_sha):
        raise ValueError("review evidence snapshot head must be an exact lowercase SHA")
    if snapshot.review_decision not in (
        "",
        "APPROVED",
        "CHANGES_REQUESTED",
        "REVIEW_REQUIRED",
    ):
        raise ValueError("review evidence snapshot has an unknown aggregate decision")
    normalized = tuple(
        (event, retirement_record(event.body), _normalized_review_body(event.body))
        for event in snapshot.events
    )
    ordered = sorted(
        normalized,
        key=lambda item: (
            item[0].kind,
            item[0].identity,
            item[0].author if item[0].retirement_actor_permission else "",
            item[0].state,
            item[0].head_sha,
            item[0].created_at,
            item[0].updated_at,
            item[0].last_edited_at,
            item[2],
            item[0].retirement_actor_permission,
        ),
    )
    seen: set[tuple[str, str]] = set()
    for event, retirement, _body in ordered:
        if not event.kind or not event.identity:
            raise ValueError("review evidence event lacks a stable kind or identity")
        if event.kind not in ("review", "issue-comment", "review-comment"):
            raise ValueError(f"review evidence event has unknown kind {event.kind!r}")
        permission = event.retirement_actor_permission
        if retirement is None:
            if permission:
                raise ValueError(
                    "non-retirement review evidence carries repository permission"
                )
        elif permission:
            if retirement.head_sha != snapshot.head_sha or event.state != "ACTIVE":
                raise ValueError(
                    "repository permission is bound to an inactive or stale retirement"
                )
            if not event.author:
                raise ValueError(
                    "review evidence retirement permission lacks a GitHub event author"
                )
            if permission not in ALLOWED_RETIREMENT_PERMISSIONS:
                raise ValueError(
                    "review evidence retirement lacks current triage-or-higher permission"
                )
        if not event.state:
            raise ValueError("review evidence event lacks a state")
        if event.kind == "review" and not _exact_sha(event.head_sha):
            raise ValueError("native review evidence requires an exact lowercase head SHA")
        if event.head_sha and not _exact_sha(event.head_sha):
            raise ValueError(
                "review evidence event head must be empty or an exact lowercase SHA"
            )
        if not event.created_at or not event.updated_at:
            raise ValueError(
                "review evidence event lacks a creation or version timestamp"
            )
        key = (event.kind, event.identity)
        if key in seen:
            raise ValueError(
                f"review evidence contains duplicate stable identity {event.kind}:{event.identity}"
            )
        seen.add(key)
    has_changes_requested = any(
        event.kind == "review" and event.state == "CHANGES_REQUESTED"
        for event, _retirement, _body in ordered
    )
    if not snapshot.review_decision and has_changes_requested:
        raise ValueError(
            "review evidence has a changes-requested review but no aggregate decision"
        )
    if snapshot.review_decision == "CHANGES_REQUESTED" and not has_changes_requested:
        raise ValueError(
            "review evidence has a changes-requested aggregate but no matching review"
        )

    digest = hashlib.sha256(b"pr-landing-planner-review-evidence-v3")

    def feed(value: str) -> None:
        encoded = value.encode("utf-8")
        digest.update(len(encoded).to_bytes(8, "big"))
        digest.update(encoded)

    feed(snapshot.head_sha)
    feed(snapshot.review_decision)
    digest.update(len(ordered).to_bytes(8, "big"))
    for event, _retirement, body in ordered:
        for value in (
            event.kind,
            event.identity,
            event.author if event.retirement_actor_permission else "",
            event.state,
            event.head_sha,
            event.created_at,
            event.updated_at,
            event.last_edited_at,
            body,
            event.retirement_actor_permission,
        ):
            feed(value)
    return digest.hexdigest()


def parse_landing_context(raw: object) -> tuple[LandingContext, ...]:
    """Parse ``{"prs": [...]}``, rejecting ambiguous or unsafe evidence."""
    if not isinstance(raw, dict):
        raise ValueError("landing context must be an object with a 'prs' array")
    prs = raw.get("prs")
    if not isinstance(prs, list):
        raise ValueError("landing context field 'prs' must be an array")

    contexts: list[LandingContext] = []
    seen: set[int] = set()
    for item in prs:
        if not isinstance(item, dict):
            raise ValueError("each landing context PR entry must be an object")
        obj: dict[str, object] = {str(key): value for key, value in item.items()}
        pr = obj.get("pr")
        if not isinstance(pr, int) or isinstance(pr, bool) or pr <= 0:
            raise ValueError("landing context PR entry needs a positive integer 'pr'")
        if pr in seen:
            raise ValueError(f"landing context contains duplicate PR #{pr}")
        seen.add(pr)

        evidence_raw = _str_field(obj, "validation_evidence")
        authority_raw = _str_field(obj, "validation_authority")
        policy_raw = _str_field(obj, "policy_class")
        try:
            evidence = ValidationEvidence(evidence_raw) if evidence_raw else None
        except ValueError as exc:
            raise ValueError(
                f"PR #{pr} has unknown validation_evidence {evidence_raw!r}"
            ) from exc
        try:
            authority = ValidationAuthority(authority_raw) if authority_raw else None
        except ValueError as exc:
            raise ValueError(
                f"PR #{pr} has unknown validation_authority {authority_raw!r}"
            ) from exc
        try:
            policy = PolicyClass(policy_raw) if policy_raw else None
        except ValueError as exc:
            raise ValueError(f"PR #{pr} has unknown policy_class {policy_raw!r}") from exc

        head_sha = _str_field(obj, "head_sha")
        base_sha = _str_field(obj, "base_sha")
        review_objections_resolved = obj.get("review_objections_resolved", False)
        review_digest = _str_field(obj, "review_evidence_digest")
        if not isinstance(review_objections_resolved, bool):
            raise ValueError(
                f"PR #{pr} review_objections_resolved must be a boolean"
            )
        if review_objections_resolved and not _exact_sha(head_sha):
            raise ValueError(
                f"PR #{pr} review_objections_resolved requires exact 'head_sha'"
            )
        if review_objections_resolved and not _exact_sha256(review_digest):
            raise ValueError(
                f"PR #{pr} review_objections_resolved requires "
                "an exact lowercase 'review_evidence_digest'; run an uncontexted "
                "exact-head plan, have the review authority assess that snapshot, and "
                "copy nodes[].review_evidence_digest into the generated context"
            )
        if review_digest and not review_objections_resolved:
            raise ValueError(
                f"PR #{pr} review_evidence_digest requires "
                "review_objections_resolved=true"
            )
        if evidence in (
            ValidationEvidence.LOCALLY_VALIDATED,
            ValidationEvidence.CLEAN_VALIDATE_RECORD,
        ) and (not head_sha or not base_sha):
            raise ValueError(
                f"PR #{pr} {evidence.value} evidence requires exact 'head_sha' "
                "and 'base_sha'; revalidate and record both fetched identities"
            )
        if (
            authority is not None
            and authority is not ValidationAuthority.NONE
            and evidence is not ValidationEvidence.CLEAN_VALIDATE_RECORD
        ):
            raise ValueError(
                f"PR #{pr} {authority.value} validation_authority requires "
                "validation_evidence 'clean-validate-record'"
            )
        if evidence is ValidationEvidence.CLEAN_VALIDATE_RECORD and authority not in (
            ValidationAuthority.HARD_GREEN,
            ValidationAuthority.SOFT_GREEN,
        ):
            raise ValueError(
                f"PR #{pr} clean-validate-record requires explicit "
                "validation_authority 'hard-green' or 'soft-green'"
            )
        raw_review_pass_heads = obj.get("review_pass_heads", {})
        if not isinstance(raw_review_pass_heads, dict):
            raise ValueError(
                f"PR #{pr} review_pass_heads must be an object mapping lane to exact SHA"
            )
        review_pass_heads: list[tuple[str, str]] = []
        for raw_lane, raw_sha in raw_review_pass_heads.items():
            lane = raw_lane if isinstance(raw_lane, str) else ""
            if lane not in REQUIRED_REVIEW_LANES:
                raise ValueError(f"PR #{pr} has unknown review lane {lane!r}")
            sha = raw_sha if isinstance(raw_sha, str) else ""
            if not _exact_sha(sha):
                raise ValueError(
                    f"PR #{pr} review_pass_heads[{lane!r}] must be an exact "
                    "40-character lowercase hex SHA"
                )
            review_pass_heads.append((lane, sha))
        contexts.append(
            LandingContext(
                pr=pr,
                head_sha=head_sha,
                base_sha=base_sha,
                assigned_agent=_str_field(obj, "assigned_agent"),
                validation_evidence=evidence,
                validation_authority=authority,
                review_pass_heads=tuple(sorted(review_pass_heads)),
                review_objections_resolved=review_objections_resolved,
                review_evidence_digest=review_digest,
                policy_class=policy,
            )
        )
    return tuple(contexts)


def _one_label_value(labels: Sequence[str], prefix: str, field: str, pr: int) -> str:
    values = sorted(
        {
            label[len(prefix) :]
            for label in labels
            if label.startswith(prefix) and label != prefix
        }
    )
    if len(values) > 1:
        raise ValueError(f"PR #{pr} has multiple {field} labels: {', '.join(values)}")
    return values[0] if values else ""


def _optional_agent_label(labels: Sequence[str]) -> str:
    values = sorted(
        {
            label[len(AGENT_PREFIX) :]
            for label in labels
            if label.startswith(AGENT_PREFIX) and label != AGENT_PREFIX
        }
    )
    return values[0] if len(values) == 1 else ""


def _label_context(node: PrNode) -> PrNode:
    assigned_agent = _optional_agent_label(node.labels)
    policy_raw = _one_label_value(node.labels, POLICY_PREFIX, "landing-policy", node.number)
    try:
        policy = PolicyClass(policy_raw) if policy_raw else PolicyClass.UNCLASSIFIED
    except ValueError as exc:
        raise ValueError(
            f"PR #{node.number} has unknown landing-policy label {policy_raw!r}"
        ) from exc

    if node.ci.raw_state is CiState.GREEN and node.ci.gate_ok:
        evidence = ValidationEvidence.AUTHORITATIVE_CI
    else:
        # A locally-validated label is only a cache hint. It deliberately maps to
        # NONE; only caller-supplied evidence bound to the fetched head and base may
        # produce LOCALLY_VALIDATED.
        evidence = ValidationEvidence.NONE
    return replace(
        node,
        assigned_agent=assigned_agent,
        validation_evidence=evidence,
        policy_class=policy,
    )


def apply_landing_context(
    nodes: Sequence[PrNode], contexts: Sequence[LandingContext]
) -> tuple[PrNode, ...]:
    """Apply labels and caller authority; keep head binding exact and fail closed on drift."""
    by_context = {context.pr: context for context in contexts}
    node_numbers = {node.number for node in nodes}
    unknown = sorted(set(by_context) - node_numbers)
    if unknown:
        rendered = ", ".join(f"#{number}" for number in unknown)
        raise ValueError(f"landing context names PRs absent from this plan: {rendered}")

    out: list[PrNode] = []
    for raw_node in nodes:
        node = _label_context(raw_node)
        context = by_context.get(node.number)
        if context is None:
            out.append(node)
            continue
        if context.head_sha and context.head_sha != node.head_sha:
            raise ValueError(
                f"PR #{node.number} landing context is stale: "
                f"context={context.head_sha}, current={node.head_sha}"
            )
        if (
            context.review_objections_resolved
            and node.review_evidence_digest != context.review_evidence_digest
        ):
            raise ValueError(
                f"PR #{node.number} review objection resolution is stale: "
                f"context digest {context.review_evidence_digest!r}, "
                f"host digest {node.review_evidence_digest!r}; rerun an uncontexted "
                "exact-head plan, have the authority reassess that snapshot, "
                "and copy nodes[].review_evidence_digest into fresh context"
            )
        if (
            context.base_sha
            and context.base_sha != node.base_sha
            and context.validation_authority is not ValidationAuthority.SOFT_GREEN
        ):
            raise ValueError(
                f"PR #{node.number} landing context base differs: "
                f"context={context.base_sha}, current={node.base_sha}; "
                "the consuming workspace supplied no soft-green authority"
            )
        out.append(
            replace(
                node,
                assigned_agent=context.assigned_agent or node.assigned_agent,
                validation_evidence=(
                    context.validation_evidence
                    if context.validation_evidence is not None
                    else node.validation_evidence
                ),
                validation_authority=(
                    context.validation_authority
                    if context.validation_authority is not None
                    else node.validation_authority
                ),
                review_pass_heads=context.review_pass_heads,
                review_objections_resolved=context.review_objections_resolved,
                policy_class=(
                    context.policy_class
                    if context.policy_class is not None
                    else node.policy_class
                ),
            )
        )
    return tuple(out)
