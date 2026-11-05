"""Landing-context evidence, policy, assignment, and mechanism-overlap tests."""

from __future__ import annotations

import json
from dataclasses import fields, replace

import pytest

from pr_landing_planner.emit import render_json
from pr_landing_planner.landing_context import (
    apply_landing_context,
    has_comment_changes_requested,
    parse_landing_context,
    retirement_record,
    review_evidence_digest,
)
from pr_landing_planner.graph import build_mechanism_edges, held_reasons, review_binding
from pr_landing_planner.model import (
    CiState,
    CiVerdict,
    CollectedGraph,
    PolicyClass,
    PrAction,
    PrNode,
    RedClass,
    ReviewEvidenceEvent,
    ReviewEvidenceSnapshot,
    ReviewBinding,
    ValidationAuthority,
    ValidationEvidence,
)
from pr_landing_planner.plan import assemble_result, compute_plan


def _node(
    number: int,
    *,
    labels: tuple[str, ...] = (),
    state: CiState = CiState.PENDING,
    red: RedClass | None = None,
) -> PrNode:
    return PrNode(
        number=number,
        head_ref=f"feature-{number}",
        base_ref="main",
        head_sha=f"sha-{number}",
        base_sha="base",
        labels=labels,
        ci=CiVerdict(
            raw_state=state,
            red_class=red,
            gate_present=True,
            gate_ok=state is CiState.GREEN,
            gate_missing_run=False,
        ),
    )


def test_raw_local_label_is_cache_only_but_dereferenced_record_is_evidence() -> None:
    nodes = apply_landing_context(
        [
            _node(
                1,
                labels=(
                    "locally-validated",
                    "agent:widget-a",
                    "landing-policy:ci-hygiene",
                    "mechanism:cancel-in-progress",
                ),
            ),
            _node(2, labels=("mechanism:cancel-in-progress",)),
        ],
        [],
    )
    # Negative bracket: one bare cache label produces zero validation evidence.
    assert nodes[0].validation_evidence is ValidationEvidence.NONE
    assert nodes[0].assigned_agent == "widget-a"
    assert nodes[0].policy_class is PolicyClass.CI_HYGIENE
    assert build_mechanism_edges(nodes)[0].mechanisms == ("cancel-in-progress",)
    plan, _ = compute_plan(nodes, [], [], [])
    actions = {decision.pr: decision.action for decision in plan.per_pr_actions}
    assert actions[1] is PrAction.REFIRE_CI
    assert 1 not in plan.land_now

    context = parse_landing_context(
        {
            "prs": [
                {
                    "pr": 1,
                    "head_sha": "sha-1",
                    "base_sha": "base",
                    "validation_evidence": "locally-validated",
                }
            ]
        }
    )
    # Positive bracket: one caller-dereferenced exact-identity record is accepted.
    dereferenced = apply_landing_context([_node(1)], context)[0]
    multiply_labeled = apply_landing_context(
        [_node(3, labels=("agent:departed", "agent:replacement"))], ()
    )[0]
    assert multiply_labeled.assigned_agent == ""
    assert multiply_labeled.number == 3

    assert dereferenced.validation_evidence is ValidationEvidence.LOCALLY_VALIDATED

    with pytest.raises(ValueError, match="locally-validated evidence requires"):
        parse_landing_context(
            {"prs": [{"pr": 1, "validation_evidence": "locally-validated"}]}
        )


def test_clean_record_keeps_exact_head_and_delegates_older_base_to_authority() -> None:
    with pytest.raises(ValueError, match="requires exact 'head_sha'.*'base_sha'"):
        parse_landing_context(
            {
                "prs": [
                    {"pr": 1, "validation_evidence": "clean-validate-record"}
                ]
            }
        )

    context = parse_landing_context(
        {
            "prs": [
                {
                    "pr": 1,
                    "head_sha": "stale-sha",
                    "base_sha": "base",
                    "validation_evidence": "clean-validate-record",
                    "validation_authority": "hard-green",
                }
            ]
        }
    )
    with pytest.raises(ValueError, match="landing context is stale"):
        apply_landing_context([_node(1)], context)

    with pytest.raises(ValueError, match="requires explicit validation_authority"):
        parse_landing_context(
            {
                "prs": [
                    {
                        "pr": 1,
                        "head_sha": "sha-1",
                        "base_sha": "base",
                        "validation_evidence": "clean-validate-record",
                    }
                ]
            }
        )

    hard_green = parse_landing_context(
        {
            "prs": [
                {
                    "pr": 1,
                    "head_sha": "sha-1",
                    "base_sha": "base",
                    "validation_evidence": "clean-validate-record",
                    "validation_authority": "hard-green",
                }
            ]
        }
    )
    hard_green_node = apply_landing_context([_node(1)], hard_green)[0]
    assert hard_green_node.validation_authority is ValidationAuthority.HARD_GREEN
    hard_green_plan, _ = compute_plan([hard_green_node], [], [], [])
    assert hard_green_plan.per_pr_actions[0].action is PrAction.LAND_NOW

    earlier_green = parse_landing_context(
        {
            "prs": [
                {
                    "pr": 1,
                    "head_sha": "sha-1",
                    "base_sha": "earlier-green-base",
                    "validation_evidence": "clean-validate-record",
                    "validation_authority": "soft-green",
                }
            ]
        }
    )
    authorized = apply_landing_context(
        [replace(_node(1), commits_behind=5)], earlier_green
    )[0]
    assert authorized.validation_authority is ValidationAuthority.SOFT_GREEN
    # Explicit freshness bound: this case is about the soft-green authority path,
    # and being behind is no longer a reroute reason by default.
    plan, _ = compute_plan([authorized], [], [], [], freshness_max_behind=0)
    decision = plan.per_pr_actions[0]
    assert decision.action is PrAction.REBASE_THEN_LAND
    assert "without pre-landing revalidation" in decision.why

    default_plan, _ = compute_plan([authorized], [], [], [])
    assert default_plan.per_pr_actions[0].action is PrAction.LAND_NOW

    hard_green_on_other_base = parse_landing_context(
        {
            "prs": [
                {
                    "pr": 1,
                    "head_sha": "sha-1",
                    "base_sha": "divergent-base",
                    "validation_evidence": "clean-validate-record",
                    "validation_authority": "hard-green",
                }
            ]
        }
    )
    with pytest.raises(ValueError, match="supplied no soft-green authority"):
        apply_landing_context([_node(1)], hard_green_on_other_base)


def test_validation_authority_requires_a_clean_record() -> None:
    with pytest.raises(ValueError, match="requires validation_evidence"):
        parse_landing_context(
            {
                "prs": [
                    {
                        "pr": 1,
                        "head_sha": "sha-1",
                        "base_sha": "base",
                        "validation_authority": "soft-green",
                    }
                ]
            }
        )


def test_local_evidence_bypasses_ci_wait_but_gate_policy_escalates() -> None:
    context = parse_landing_context(
        {
            "prs": [
                {
                    "pr": 1,
                    "head_sha": "sha-1",
                    "base_sha": "base",
                    "validation_evidence": "clean-validate-record",
                    "validation_authority": "hard-green",
                    "policy_class": "ci-hygiene",
                    "assigned_agent": "widget-ci",
                },
                {
                    "pr": 2,
                    "head_sha": "sha-2",
                    "base_sha": "base",
                    "validation_evidence": "clean-validate-record",
                    "validation_authority": "hard-green",
                    "policy_class": "gate-policy",
                },
            ]
        }
    )
    nodes = apply_landing_context(
        [
            _node(1, state=CiState.RED, red=RedClass.STALE_REQUIRED_CHECK),
            _node(2, state=CiState.GREEN),
        ],
        context,
    )
    plan, _ = compute_plan(nodes, [], [], [])
    actions = {decision.pr: decision for decision in plan.per_pr_actions}
    assert actions[1].action is PrAction.LAND_NOW
    assert "no merge-gate wait" in actions[1].why
    assert actions[2].action is PrAction.ESCALATE_GATE_POLICY
    assert 2 not in plan.land_now


def test_authoritative_ci_is_reported_as_evidence() -> None:
    node = apply_landing_context(
        [_node(1, labels=("locally-validated",), state=CiState.GREEN)], []
    )[0]
    assert node.validation_evidence is ValidationEvidence.AUTHORITATIVE_CI


def test_json_schema_exposes_context_and_mechanism_overlap() -> None:
    # Uses a RECOGNISED mechanism: under the enum redesign, clustering keys on the normalised
    # Mechanism enum value, not the raw slug, so an unknown slug ("shared") would be UNCLASSIFIED
    # rather than an edge. `cancel-in-progress` is a seeded enum member, so the pair still clusters.
    nodes = apply_landing_context(
        [
            _node(1, labels=("locally-validated", "mechanism:cancel-in-progress")),
            _node(2, labels=("mechanism:cancel-in-progress",)),
        ],
        [],
    )
    mechanism_edges = build_mechanism_edges(nodes)
    result = assemble_result(
        CollectedGraph(
            repository="owner/repo",
            base="main",
            nodes=nodes,
            conflict_edges=(),
            overlap_edges=(),
            ordering_edges=(),
            mechanism_edges=mechanism_edges,
        )
    )
    payload = json.loads(render_json(result))
    assert payload["nodes"][0]["validation_evidence"] == "none"
    assert payload["nodes"][0]["validation_authority"] == "none"
    assert payload["nodes"][0]["review_binding"] == "not-required"
    assert payload["nodes"][0]["review_pass_heads"] == {}
    assert payload["nodes"][0]["review_objections_resolved"] is False
    assert payload["mechanism_overlap_edges"] == [
        {"a": 1, "b": 2, "mechanisms": ["cancel-in-progress"]}
    ]


REVIEWED_HEAD = "92e1e0d0af65e50cd2991d4deaa25f726832fbf4"
REBASED_HEAD = "0fc9f61edc01d6425def2efb0ed82f01410c7fcc"
CHANGED_HEAD = "1111111111111111111111111111111111111111"
REVIEW_LABELS = (
    "post-facto-human-review",
    "passed-review-codex",
    "passed-review-claude",
)


def test_review_passes_bind_exact_head_and_head_moves_fail_closed() -> None:
    exact_context = parse_landing_context(
        {
            "prs": [
                {
                    "pr": 394,
                    "review_pass_heads": {
                        "codex": REBASED_HEAD,
                        "claude": REBASED_HEAD,
                    },
                }
            ]
        }
    )
    exact = apply_landing_context(
        [replace(_node(394, labels=REVIEW_LABELS), head_sha=REBASED_HEAD)],
        exact_context,
    )[0]
    assert review_binding(exact) == (ReviewBinding.EXACT_HEAD, ())
    assert not held_reasons((exact,), ())

    stale_context = parse_landing_context(
        {
            "prs": [
                {
                    "pr": 394,
                    "review_pass_heads": {
                        "codex": REBASED_HEAD,
                        "claude": REVIEWED_HEAD,
                    },
                }
            ]
        }
    )
    stale = apply_landing_context(
        [replace(_node(394, labels=REVIEW_LABELS), head_sha=REBASED_HEAD)],
        stale_context,
    )[0]
    reason = (
        f"review-pass-stale:claude:reviewed={REVIEWED_HEAD}:current={REBASED_HEAD}"
    )
    assert review_binding(stale) == (ReviewBinding.STALE, (reason,))
    assert held_reasons((stale,), ())[0].reasons == (reason,)

    changed = replace(stale, head_sha=CHANGED_HEAD)
    assert review_binding(changed)[0] is ReviewBinding.STALE


def test_review_objection_resolution_is_boolean_and_exact_head_bound() -> None:
    with pytest.raises(ValueError, match="must be a boolean"):
        parse_landing_context(
            {"prs": [{"pr": 394, "review_objections_resolved": "yes"}]}
        )
    with pytest.raises(ValueError, match="requires exact 'head_sha'"):
        parse_landing_context(
            {"prs": [{"pr": 394, "review_objections_resolved": True}]}
        )
    with pytest.raises(ValueError, match="review_evidence_digest"):
        parse_landing_context(
            {
                "prs": [
                    {
                        "pr": 394,
                        "head_sha": REBASED_HEAD,
                        "review_objections_resolved": True,
                    }
                ]
            }
        )

    observed_at = "2026-09-04T12:00:00Z"
    snapshot = ReviewEvidenceSnapshot(
        head_sha=REBASED_HEAD,
        review_decision="CHANGES_REQUESTED",
        events=(
            ReviewEvidenceEvent(
                kind="review", identity="review-1", state="CHANGES_REQUESTED",
                head_sha=REBASED_HEAD, created_at=observed_at,
                updated_at=observed_at, last_edited_at="",
                body="please address the race", author="reviewer",
            ),
            ReviewEvidenceEvent(
                kind="issue-comment", identity="comment-1", state="ACTIVE",
                head_sha="", created_at=observed_at, updated_at=observed_at,
                last_edited_at="", body="resolved by the latest patch",
                author="release-authority",
            ),
            ReviewEvidenceEvent(
                kind="review-comment", identity="thread-1", state="RESOLVED",
                head_sha="", created_at=observed_at, updated_at=observed_at,
                last_edited_at="", body="inline objection retired", author="reviewer",
            ),
        ),
    )
    digest = review_evidence_digest(snapshot)
    context = parse_landing_context(
        {
            "prs": [
                {
                    "pr": 394,
                    "head_sha": REBASED_HEAD,
                    "review_objections_resolved": True,
                    "review_evidence_digest": digest,
                }
            ]
        }
    )
    node = apply_landing_context(
        [
            replace(
                _node(394),
                head_sha=REBASED_HEAD,
                updated_at=observed_at,
                review_decision="CHANGES_REQUESTED",
                review_evidence_digest=digest,
            )
        ],
        context,
    )[0]
    assert node.review_objections_resolved
    assert not held_reasons((node,), ())

    with pytest.raises(ValueError, match="landing context is stale"):
        apply_landing_context([replace(node, head_sha=CHANGED_HEAD)], context)
    same_second_objection = ReviewEvidenceSnapshot(
        head_sha=snapshot.head_sha,
        review_decision=snapshot.review_decision,
        events=(*snapshot.events, ReviewEvidenceEvent(
            kind="review-comment", identity="thread-2", state="ACTIVE",
            head_sha="", created_at=observed_at, updated_at=observed_at,
            last_edited_at="", body="new same-second objection", author="reviewer",
        )),
    )
    with pytest.raises(
        ValueError, match="review objection resolution is stale"
    ) as stale_error:
        apply_landing_context(
            [
                replace(
                    node,
                    updated_at=observed_at,
                    review_evidence_digest=review_evidence_digest(
                        same_second_objection
                    ),
                )
            ],
            context,
        )
    assert "uncontexted exact-head plan" in str(stale_error.value)
    assert "nodes[].review_evidence_digest" in str(stale_error.value)

    with pytest.raises(ValueError, match="requires review_objections_resolved=true"):
        parse_landing_context(
            {"prs": [{"pr": 394, "review_evidence_digest": digest}]}
        )
    with pytest.raises(ValueError, match="stable kind or identity"):
        review_evidence_digest(
            ReviewEvidenceSnapshot(
                REBASED_HEAD,
                "CHANGES_REQUESTED",
                (
                    ReviewEvidenceEvent(
                        kind="review", identity="", state="APPROVED",
                        head_sha=REBASED_HEAD, created_at=observed_at,
                        updated_at=observed_at, last_edited_at="", author="reviewer",
                    ),
                ),
            )
        )
    missing_author = replace(
        snapshot,
        events=(replace(snapshot.events[0], author=""), *snapshot.events[1:]),
    )
    assert review_evidence_digest(missing_author) == digest
    with pytest.raises(ValueError, match="no aggregate decision"):
        review_evidence_digest(replace(snapshot, review_decision=""))
    with pytest.raises(ValueError, match="unknown aggregate decision"):
        review_evidence_digest(replace(snapshot, review_decision="UNKNOWN"))
    with pytest.raises(ValueError, match="no matching review"):
        review_evidence_digest(replace(snapshot, events=snapshot.events[1:]))
    author_changed = replace(
        snapshot,
        events=(
            replace(snapshot.events[0], author="different-reviewer"),
            *snapshot.events[1:],
        ),
    )
    assert review_evidence_digest(author_changed) == digest
    with pytest.raises(ValueError, match="duplicate stable identity"):
        review_evidence_digest(
            ReviewEvidenceSnapshot(
                REBASED_HEAD,
                "CHANGES_REQUESTED",
                (
                    ReviewEvidenceEvent(
                        kind="review", identity="same", state="APPROVED",
                        head_sha=REBASED_HEAD, created_at=observed_at,
                        updated_at=observed_at, last_edited_at="", author="reviewer",
                    ),
                    ReviewEvidenceEvent(
                        kind="review", identity="same", state="DISMISSED",
                        head_sha=REBASED_HEAD, created_at=observed_at,
                        updated_at=observed_at, last_edited_at="", author="reviewer",
                    ),
                ),
            )
        )


def test_review_evidence_digest_covers_every_normalized_authority_field() -> None:
    assert tuple(field.name for field in fields(ReviewEvidenceEvent)) == (
        "kind",
        "identity",
        "state",
        "head_sha",
        "created_at",
        "updated_at",
        "last_edited_at",
        "body",
        "author",
        "retirement_actor_permission",
    )
    assert tuple(field.name for field in fields(ReviewEvidenceSnapshot)) == (
        "head_sha",
        "review_decision",
        "events",
    )
    event = ReviewEvidenceEvent(
        kind="review",
        identity="review-1",
        author="reviewer",
        state="APPROVED",
        head_sha=REBASED_HEAD,
        created_at="2026-09-04T11:59:00Z",
        updated_at="2026-09-04T12:00:00Z",
        last_edited_at="",
        body="looks good",
    )
    snapshot = ReviewEvidenceSnapshot(REBASED_HEAD, "APPROVED", (event,))
    digest = review_evidence_digest(snapshot)
    mutations = (
        replace(event, kind="issue-comment"),
        replace(event, identity="review-2"),
        replace(event, state="DISMISSED"),
        replace(event, head_sha=CHANGED_HEAD),
        replace(event, created_at="2026-09-04T11:58:00Z"),
        replace(event, updated_at="2026-09-04T12:00:01Z"),
        replace(event, last_edited_at="2026-09-04T12:00:01Z"),
        replace(event, body="new objection"),
    )
    for mutation in mutations:
        assert review_evidence_digest(replace(snapshot, events=(mutation,))) != digest
    assert review_evidence_digest(
        replace(snapshot, events=(replace(event, author="different-reviewer"),))
    ) == digest
    assert review_evidence_digest(replace(snapshot, head_sha=CHANGED_HEAD)) != digest
    assert review_evidence_digest(
        replace(snapshot, review_decision="REVIEW_REQUIRED")
    ) != digest


def test_retirement_uses_event_author_permission_and_ignores_claimed_identity() -> None:
    body = (
        "[team, release-authority, session, model, role=observer]\n"
        f"CHANGES-REQUESTED-WITHDRAWN-AT: codex {REBASED_HEAD} "
        "BY release-authority\n"
        "RETIRES 123456"
    )
    event = ReviewEvidenceEvent(
        kind="issue-comment",
        identity="comment-1",
        author="release-authority",
        state="ACTIVE",
        head_sha="",
        created_at="2026-09-04T12:00:00Z",
        updated_at="2026-09-04T12:00:01Z",
        body=body,
        retirement_actor_permission="write",
    )
    snapshot = ReviewEvidenceSnapshot(REBASED_HEAD, "APPROVED", (event,))
    record = retirement_record(body)
    assert record is not None
    assert (record.target_comment_id, record.lane, record.head_sha) == (
        "123456", "codex", REBASED_HEAD
    )
    write_digest = review_evidence_digest(snapshot)
    assert write_digest == "83f83abe9e2e319f0a65a4f100d1b6c91f8a6c0ca29e6797d120166e6cca3b58"
    appended_objection = replace(
        snapshot,
        events=(
            replace(event, body=f"{body}\nDo not land: the race remains."),
        ),
    )
    assert review_evidence_digest(appended_objection) != write_digest
    assert review_evidence_digest(
        replace(
            snapshot,
            events=(replace(event, retirement_actor_permission="maintain"),),
        )
    ) != write_digest

    unverified = replace(
        snapshot, events=(replace(event, author="", retirement_actor_permission=""),)
    )
    unverified_digest = review_evidence_digest(unverified)
    assert unverified_digest != write_digest
    assert review_evidence_digest(
        replace(unverified, events=(replace(unverified.events[0], author="departed"),))
    ) == unverified_digest

    with pytest.raises(ValueError, match="triage-or-higher"):
        review_evidence_digest(
            replace(
                snapshot,
                events=(replace(event, retirement_actor_permission="read"),),
            )
        )
    with pytest.raises(ValueError, match="lacks a GitHub event author"):
        review_evidence_digest(
            replace(snapshot, events=(replace(event, author=""),))
        )
    hostname_author = replace(
        snapshot, events=(replace(event, author="devbig014"),)
    )
    assert review_evidence_digest(hostname_author) != write_digest
    for claimed_identity in (
        "other-agent",
        "other_agent",
        "other.agent",
        "K",
    ):
        false_by = replace(
            snapshot,
            events=(
                replace(
                    event,
                    body=body.replace("BY release-authority", f"BY {claimed_identity}"),
                ),
            ),
        )
        assert retirement_record(false_by.events[0].body) == record
        assert review_evidence_digest(false_by) == write_digest
    malformed_by = replace(
        snapshot,
        events=(
            replace(event, body=body.replace("BY release-authority", "BY other/agent")),
        ),
    )
    assert retirement_record(malformed_by.events[0].body) == record
    malformed_digest = review_evidence_digest(malformed_by)
    assert malformed_digest != write_digest
    assert review_evidence_digest(
        replace(
            malformed_by,
            events=(replace(malformed_by.events[0], body=malformed_by.events[0].body.replace("other/agent", "other agent")),),
        )
    ) != malformed_digest
    for malformed_suffix in ("BY: other.agent", "BY/other.agent"):
        alternate_malformed = replace(
            snapshot,
            events=(
                replace(
                    event,
                    body=body.replace("BY release-authority", malformed_suffix),
                ),
            ),
        )
        assert retirement_record(alternate_malformed.events[0].body) == record
        assert review_evidence_digest(alternate_malformed) != write_digest
    for non_ascii_identity in ("K", "İ", "ı", "ſ"):
        non_ascii = replace(
            snapshot,
            events=(
                replace(
                    event,
                    body=body.replace(
                        "BY release-authority", f"BY {non_ascii_identity}"
                    ),
                ),
            ),
        )
        assert retirement_record(non_ascii.events[0].body) == record
        assert review_evidence_digest(non_ascii) != write_digest
    different_disclosure = replace(
        snapshot,
        events=(
            replace(
                event,
                body=body.replace("[team, release-authority, session, model, role=observer]", "[team, departed, old-session, model, role=observer]"),
            ),
        ),
    )
    assert review_evidence_digest(different_disclosure) == write_digest
    with pytest.raises(ValueError, match="inactive or stale retirement"):
        review_evidence_digest(
            replace(snapshot, events=(replace(event, state="MINIMIZED:OUTDATED"),))
        )
    with pytest.raises(ValueError, match="inactive or stale retirement"):
        review_evidence_digest(
            replace(
                snapshot,
                events=(replace(event, body=body.replace(REBASED_HEAD, CHANGED_HEAD)),),
            )
        )
    with pytest.raises(ValueError, match="non-retirement"):
        review_evidence_digest(
            replace(
                snapshot,
                events=(replace(event, body="ordinary comment"),),
            )
        )


def test_review_marker_attribution_is_metadata_for_every_marker() -> None:
    body = (
        "[team, current-reviewer, session, model, role=reviewer]\n"
        f"CHANGES-REQUESTED-AT: codex {REBASED_HEAD} BY current-reviewer\n"
        f"CHANGES-REQUESTED-WITHDRAWN-AT: codex {REBASED_HEAD} "
        "BY current-reviewer\n"
        f"APPROVED-AT: codex {REBASED_HEAD} BY current-reviewer\n"
        "The objection is resolved by the changed bounds check."
    )
    event = ReviewEvidenceEvent(
        kind="issue-comment",
        identity="comment-markers",
        author="current-reviewer",
        state="ACTIVE",
        head_sha="",
        created_at="2026-09-05T15:03:48Z",
        updated_at="2026-09-05T15:03:48Z",
        body=body,
    )
    snapshot = ReviewEvidenceSnapshot(REBASED_HEAD, "", (event,))
    digest = review_evidence_digest(snapshot)
    for disclosure_agent in (
        "departed-reviewer",
        "departed_reviewer",
        "departed.reviewer",
        "K",
    ):
        metadata_changed = body.replace(
            "[team, current-reviewer, session, model, role=reviewer]",
            f"[team, {disclosure_agent}, old-session, other-model, role=reviewer]",
        )
        assert review_evidence_digest(
            replace(snapshot, events=(replace(event, body=metadata_changed),))
        ) == digest
    for claimed_identity in (
        "departed-reviewer",
        "departed_reviewer",
        "departed.reviewer",
    ):
        metadata_changed = body.replace("BY current-reviewer", f"BY {claimed_identity}")
        assert review_evidence_digest(
            replace(snapshot, events=(replace(event, body=metadata_changed),))
        ) == digest
    for non_ascii_identity in ("K", "İ", "ı", "ſ"):
        metadata_changed = body.replace(
            "[team, current-reviewer, session, model, role=reviewer]",
            f"[team, {non_ascii_identity}, session, model, role=reviewer]",
        )
        assert review_evidence_digest(
            replace(snapshot, events=(replace(event, body=metadata_changed),))
        ) != digest
    metadata_removed = "\n".join(
        line.replace(" BY current-reviewer", "")
        for line in body.split("\n")[1:]
    )
    assert review_evidence_digest(
        replace(snapshot, events=(replace(event, body=metadata_removed),))
    ) == digest
    malformed_body = body.replace("BY current-reviewer", "BY current/reviewer")
    malformed = replace(snapshot, events=(replace(event, body=malformed_body),))
    malformed_digest = review_evidence_digest(malformed)
    assert malformed_digest != digest
    assert has_comment_changes_requested(malformed)
    mutated_malformed = replace(
        malformed,
        events=(
            replace(
                malformed.events[0],
                body=malformed_body.replace("current/reviewer", "current reviewer"),
            ),
        ),
    )
    assert review_evidence_digest(mutated_malformed) != malformed_digest
    assert has_comment_changes_requested(mutated_malformed)
    fenced = f"{body}\n```text\nAPPROVED-AT: codex {REBASED_HEAD} BY quoted\n```"
    changed_fenced = fenced.replace("BY quoted", "BY other-quoted")
    assert review_evidence_digest(
        replace(snapshot, events=(replace(event, body=fenced),))
    ) != review_evidence_digest(
        replace(snapshot, events=(replace(event, body=changed_fenced),))
    )


@pytest.mark.parametrize(
    ("kind", "state", "head_sha"),
    (
        ("review", "APPROVED", REBASED_HEAD),
        ("issue-comment", "ACTIVE", ""),
        ("review-comment", "ACTIVE", ""),
    ),
)
def test_unverified_who_metadata_is_optional_only_as_an_exact_prose_line(
    kind: str, state: str, head_sha: str
) -> None:
    base_body = "The bounds check resolves the objection."
    event = ReviewEvidenceEvent(
        kind=kind,
        identity=f"{kind}-metadata",
        author="reviewer",
        state=state,
        head_sha=head_sha,
        created_at="2026-09-05T15:03:48Z",
        updated_at="2026-09-05T15:03:48Z",
        body=base_body,
    )
    snapshot = ReviewEvidenceSnapshot(REBASED_HEAD, "APPROVED", (event,))
    digest = review_evidence_digest(snapshot)
    for agent in ("review-agent", "review_agent", "review.agent", "K"):
        with_metadata = replace(
            snapshot,
            events=(
                replace(
                    event,
                    body=f"{base_body}\nUnverified --who metadata: {agent}",
                ),
            ),
        )
        assert review_evidence_digest(with_metadata) == digest
    for non_ascii_identity in ("K", "İ", "ı", "ſ"):
        with_metadata = replace(
            snapshot,
            events=(
                replace(
                    event,
                    body=(
                        f"{base_body}\nUnverified --who metadata: {non_ascii_identity}"
                    ),
                ),
            ),
        )
        assert review_evidence_digest(with_metadata) != digest

    preserved_forms = (
        f"{base_body}\nUnverified --who metadata: review.agent extra",
        f"{base_body}\n> Unverified --who metadata: review.agent",
        f"{base_body}\n`Unverified --who metadata: review.agent`",
        f"{base_body}\n```text\nUnverified --who metadata: review.agent\n```",
        f"{base_body}\n\n    Unverified --who metadata: review.agent",
    )
    for preserved in preserved_forms:
        first = replace(snapshot, events=(replace(event, body=preserved),))
        second = replace(
            snapshot,
            events=(
                replace(event, body=preserved.replace("review.agent", "other_agent")),
            ),
        )
        assert review_evidence_digest(first) != digest
        assert review_evidence_digest(first) != review_evidence_digest(second)


@pytest.mark.parametrize("kind", ("issue-comment", "review-comment"))
def test_comment_refusal_survives_valid_and_malformed_by_metadata(kind: str) -> None:
    marker = f"CHANGES-REQUESTED-AT: codex {REBASED_HEAD}"
    event = ReviewEvidenceEvent(
        kind=kind,
        identity=f"{kind}-refusal",
        author="reviewer",
        state="ACTIVE",
        head_sha="",
        created_at="2026-09-05T15:03:48Z",
        updated_at="2026-09-05T15:03:48Z",
        body=marker,
    )
    for suffix in (
        "",
        " BY review-agent",
        " BY review_agent",
        " BY review.agent",
        " BY review/agent",
        " BY review agent",
        " BY",
        " BY: review.agent",
        " BY/review.agent",
        " BY K",
        " BY İ",
        " BY ı",
        " BY ſ",
    ):
        snapshot = ReviewEvidenceSnapshot(
            REBASED_HEAD,
            "",
            (replace(event, body=f"{marker}{suffix}"),),
        )
        assert has_comment_changes_requested(snapshot)

@pytest.mark.parametrize("space", ("\u00a0", "\u2003"))
def test_non_ascii_whitespace_is_not_review_evidence_syntax(space: str) -> None:
    marker = f"CHANGES-REQUESTED-AT: codex {REBASED_HEAD}"
    event = ReviewEvidenceEvent(
        kind="issue-comment",
        identity="comment-whitespace",
        author="reviewer",
        state="ACTIVE",
        head_sha="",
        created_at="2026-09-05T15:03:48Z",
        updated_at="2026-09-05T15:03:48Z",
        body=marker,
    )
    canonical = ReviewEvidenceSnapshot(REBASED_HEAD, "", (event,))
    canonical_digest = review_evidence_digest(canonical)
    assert has_comment_changes_requested(canonical)

    malformed_markers = (
        f"{space}{marker}",
        marker.replace(": ", f":{space}"),
        marker.replace("codex ", f"codex{space}"),
        f"#{space}{marker}",
    )
    for malformed_marker in malformed_markers:
        malformed = replace(
            canonical, events=(replace(event, body=malformed_marker),)
        )
        assert not has_comment_changes_requested(malformed)
        assert review_evidence_digest(malformed) != canonical_digest

    canonical_by = replace(
        canonical, events=(replace(event, body=f"{marker} BY K"),)
    )
    malformed_by = replace(
        canonical, events=(replace(event, body=f"{marker} BY{space}K"),)
    )
    assert review_evidence_digest(canonical_by) == canonical_digest
    assert has_comment_changes_requested(malformed_by)
    assert review_evidence_digest(malformed_by) != canonical_digest

    summary_event = replace(event, body="substantive result")
    summary = replace(canonical, events=(summary_event,))
    summary_digest = review_evidence_digest(summary)
    canonical_disclosure = replace(
        summary,
        events=(
            replace(
                summary_event,
                body="[team, reviewer, session, model, role=reviewer]\nsubstantive result",
            ),
        ),
    )
    malformed_disclosure = replace(
        summary,
        events=(
            replace(
                summary_event,
                body=f"[team,{space}reviewer, session, model, role=reviewer]\nsubstantive result",
            ),
        ),
    )
    assert review_evidence_digest(canonical_disclosure) == summary_digest
    assert review_evidence_digest(malformed_disclosure) != summary_digest

    canonical_metadata = replace(
        summary,
        events=(replace(summary_event, body="substantive result\nUnverified --who metadata: K"),),
    )
    malformed_metadata = replace(
        summary,
        events=(
            replace(
                summary_event,
                body=f"substantive result\nUnverified --who metadata:{space}K",
            ),
        ),
    )
    assert review_evidence_digest(canonical_metadata) == summary_digest
    assert review_evidence_digest(malformed_metadata) != summary_digest

    withdrawal = f"CHANGES-REQUESTED-WITHDRAWN-AT: codex {REBASED_HEAD}"
    assert retirement_record(f"{withdrawal}\nRETIRES 123456") is not None
    with pytest.raises(ValueError, match="canonical withdrawal"):
        retirement_record(f"{withdrawal.replace(': ', f':{space}')}\nRETIRES 123456")
    assert retirement_record(f"{withdrawal}\nRETIRES{space}123456") is None



def test_unverified_retirement_stays_visible_and_does_not_clear_objection() -> None:
    observed_at = "2026-09-04T12:00:00Z"
    retirement = ReviewEvidenceEvent(
        kind="issue-comment",
        identity="comment-2",
        author="",
        state="ACTIVE",
        head_sha="",
        created_at=observed_at,
        updated_at=observed_at,
        body=(
            f"CHANGES-REQUESTED-WITHDRAWN-AT: codex {REBASED_HEAD} "
            "BY departed\nRETIRES 123456"
        ),
    )
    snapshot = ReviewEvidenceSnapshot(
        REBASED_HEAD,
        "CHANGES_REQUESTED",
        (
            ReviewEvidenceEvent(
                kind="review",
                identity="review-1",
                author="departed-reviewer",
                state="CHANGES_REQUESTED",
                head_sha=REBASED_HEAD,
                created_at=observed_at,
                updated_at=observed_at,
                body="still unresolved",
            ),
            retirement,
        ),
    )
    digest = review_evidence_digest(snapshot)
    node = replace(
        _node(394),
        head_sha=REBASED_HEAD,
        review_decision="CHANGES_REQUESTED",
        review_evidence_digest=digest,
    )
    applied = apply_landing_context([node], ())[0]
    assert not applied.review_objections_resolved
    assert held_reasons((applied,), ())[0].reasons == ("changes-requested",)


def test_review_pass_labels_without_receipts_are_unbound_and_bad_receipts_refuse() -> None:
    unbound = apply_landing_context(
        [replace(_node(394, labels=REVIEW_LABELS), head_sha=REBASED_HEAD)], ()
    )[0]
    assert review_binding(unbound)[0] is ReviewBinding.UNBOUND

    with pytest.raises(ValueError, match="exact 40-character lowercase hex SHA"):
        parse_landing_context(
            {"prs": [{"pr": 394, "review_pass_heads": {"claude": "0fc9f61e"}}]}
        )
    with pytest.raises(ValueError, match="unknown review lane"):
        parse_landing_context(
            {"prs": [{"pr": 394, "review_pass_heads": {"other": REBASED_HEAD}}]}
        )
