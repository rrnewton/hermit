"""Tests for the pure graph algorithms: edges, transitive reduction, stacks, held, partition."""

from __future__ import annotations

from dataclasses import replace

from pr_landing_planner.graph import (
    build_conflict_edges_file_overlap,
    build_ordering_edges_base_ref,
    build_overlap_edges,
    build_stacks,
    cluster_by_conflict,
    connected_components,
    dedupe_ordering,
    held_reasons,
    partition_parallel_safe,
    rebases_avoided,
    transitive_reduce,
)
from pr_landing_planner.model import Cluster, ConflictEdge, OrderingEdge, PrNode


def _node(
    number: int,
    *,
    head_ref: str = "",
    base_ref: str = "integration",
    files: frozenset[str] = frozenset(),
    draft: bool = False,
    base_conflict: tuple[str, ...] = (),
    mergeable: str = "",
    review_decision: str = "",
    priority: int = 0,
    size: int = 0,
    created_at: str = "",
) -> PrNode:
    return PrNode(
        number=number,
        head_ref=head_ref or f"feat-{number}",
        base_ref=base_ref,
        head_sha=f"sha-{number}",
        base_sha="base",
        files=files,
        is_draft=draft,
        base_conflict_paths=base_conflict,
        mergeable=mergeable,
        review_decision=review_decision,
        priority=priority,
        additions=size,
        created_at=created_at,
    )


def test_overlap_and_file_overlap_conflict_edges() -> None:
    a = _node(1, files=frozenset({"x.rs", "y.rs"}))
    b = _node(2, files=frozenset({"y.rs"}))
    c = _node(3, files=frozenset({"z.rs"}))
    overlaps = build_overlap_edges([a, b, c])
    assert overlaps == (type(overlaps[0])(1, 2, ("y.rs",)),)
    conflicts = build_conflict_edges_file_overlap([a, b, c])
    assert [(e.a, e.b) for e in conflicts] == [(1, 2)]


def test_base_ref_ordering_edges() -> None:
    root = _node(1, head_ref="feat-root")
    stacked = _node(2, head_ref="feat-child", base_ref="feat-root")
    edges = build_ordering_edges_base_ref([root, stacked])
    assert edges == (OrderingEdge(1, 2, "base-ref"),)


def test_transitive_reduce_drops_implied_edge() -> None:
    edges = [OrderingEdge(1, 2, "x"), OrderingEdge(2, 3, "x"), OrderingEdge(1, 3, "x")]
    reduced = transitive_reduce(edges)
    assert OrderingEdge(1, 3, "x") not in reduced
    assert OrderingEdge(1, 2, "x") in reduced and OrderingEdge(2, 3, "x") in reduced


def test_build_stacks() -> None:
    edges = [OrderingEdge(1, 2, "x"), OrderingEdge(2, 3, "x")]
    assert build_stacks(edges) == ((1, 2, 3),)


def test_dedupe_ordering_keeps_first_reason_sorted() -> None:
    edges = [OrderingEdge(2, 3, "ancestry"), OrderingEdge(1, 2, "base-ref"), OrderingEdge(2, 3, "base-ref")]
    deduped = dedupe_ordering(edges)
    assert [(e.before, e.after) for e in deduped] == [(1, 2), (2, 3)]
    assert deduped[1].reason == "ancestry"  # first seen wins


def test_held_reasons_base_draft_and_transitive() -> None:
    draft = _node(1, draft=True)
    conflicted = _node(2, base_conflict=("f.rs",))
    gh_conflict = _node(3, mergeable="CONFLICTING")
    dependent = _node(4, head_ref="feat-4", base_ref="feat-1")  # stacked on the draft
    ordering = build_ordering_edges_base_ref([draft, conflicted, gh_conflict, dependent])
    held = held_reasons([draft, conflicted, gh_conflict, dependent], ordering)
    by = {h.pr: h.reasons for h in held}
    assert "draft" in by[1]
    assert "local-base-conflict" in by[2]
    assert "github-base-conflicting" in by[3]
    assert by[4] == ("depends-on-held:#1",)


def test_review_decisions_and_ordering_cycles_fail_closed() -> None:
    review_required = _node(1, review_decision="REVIEW_REQUIRED")
    changes_requested = _node(2, review_decision="CHANGES_REQUESTED")
    approved = _node(3, review_decision="APPROVED")
    cycle_a = _node(4)
    cycle_b = _node(5)
    downstream = _node(6)
    ordering = [
        OrderingEdge(4, 5, "base-ref"),
        OrderingEdge(5, 4, "ancestry"),
        OrderingEdge(5, 6, "base-ref"),
    ]
    held = held_reasons(
        [review_required, changes_requested, approved, cycle_a, cycle_b, downstream],
        ordering,
    )
    by = {item.pr: item.reasons for item in held}
    assert by[1] == ("review-required",)
    assert by[2] == ("changes-requested",)
    assert 3 not in by
    assert "ordering-cycle" in by[4]
    assert "ordering-cycle" in by[5]
    assert by[6] == ("depends-on-held:#5",)

    unavailable = replace(
        _node(7, review_decision="APPROVED"), review_evidence_unavailable=True
    )
    assert held_reasons((unavailable,), ())[0].reasons == (
        "review-evidence-unavailable",
    )

def test_exact_head_objection_resolution_only_clears_changes_requested() -> None:
    resolved = _node(1, review_decision="CHANGES_REQUESTED")
    resolved = replace(resolved, review_objections_resolved=True)
    unresolved = _node(2, review_decision="CHANGES_REQUESTED")
    review_required = replace(
        _node(3, review_decision="REVIEW_REQUIRED"),
        review_objections_resolved=True,
    )
    held = {item.pr: item.reasons for item in held_reasons(
        [resolved, unresolved, review_required], []
    )}
    assert 1 not in held
    assert held[2] == ("changes-requested",)
    assert held[3] == ("review-required",)


def test_partition_respects_conflicts_and_ordering() -> None:
    a = _node(1)
    b = _node(2)
    c = _node(3)
    conflicts = [ConflictEdge(1, 2, ("x.rs",))]
    ordering = [OrderingEdge(1, 3, "ancestry")]  # 3 must follow 1
    groups = partition_parallel_safe([a, b, c], conflicts, ordering)
    # 1 and 2 conflict => different groups; 3 follows 1 => not in group 0.
    assert 1 in groups[0]
    assert 2 not in groups[0]
    g_of = {n: i for i, g in enumerate(groups) for n in g}
    assert g_of[3] > g_of[1]
    assert g_of[2] > 0 or g_of[2] != g_of[1]


def test_partition_ranks_by_priority_then_size() -> None:
    # Two non-conflicting PRs land in one group; order within is by (priority, size, age, number).
    hi = _node(5, priority=0, size=100)
    lo = _node(6, priority=1, size=1)
    groups = partition_parallel_safe([lo, hi], [], [])
    assert groups == ((5, 6),)  # priority 0 (#5) ranked before priority 1 (#6)


def test_partition_excludes_held() -> None:
    a = _node(1)
    b = _node(2)
    groups = partition_parallel_safe([a, b], [], [], exclude=frozenset({2}))
    assert groups == ((1,),)


# --------------------------------------------------------------------------- conflict clustering
def test_connected_components_transitive_and_singleton() -> None:
    # 1-2 and 2-3 conflict (one 3-PR component); 4-5 conflict; 6 is isolated.
    edges = [ConflictEdge(1, 2, ("a",)), ConflictEdge(2, 3, ("b",)), ConflictEdge(4, 5, ("c",))]
    comps = connected_components([1, 2, 3, 4, 5, 6], edges)
    # Largest-first, ties by least member; members sorted ascending.
    assert comps == ((1, 2, 3), (4, 5), (6,))


def test_connected_components_ignores_edges_outside_node_set() -> None:
    edges = [ConflictEdge(1, 99, ("a",))]  # 99 is not in the node set
    assert connected_components([1, 2], edges) == ((1,), (2,))


def test_cluster_by_conflict_stack_order_follows_ordering_then_rank() -> None:
    # A 3-PR conflict component. Ordering says #3 must land before #1 (base-ref stacking).
    n1 = _node(1, priority=0, size=5)
    n2 = _node(2, priority=0, size=1)
    n3 = _node(3, priority=0, size=9)
    conflicts = [ConflictEdge(1, 2, ("shared.toml",)), ConflictEdge(2, 3, ("shared.toml",))]
    ordering = [OrderingEdge(3, 1, "base-ref")]  # 3 is the base of 1
    clusters = cluster_by_conflict([n1, n2, n3], conflicts, ordering)
    assert len(clusters) == 1
    cl = clusters[0]
    # #3 must come before #1 (ordering); among rank-free choices the smallest rank leads. #3 has no
    # predecessor so it's a root; #2 (rank size=1) is also a root and ranks before #3 (size=9).
    assert cl.members[0] == 2  # lowest-rank root lands first
    assert cl.members.index(3) < cl.members.index(1)  # ordering constraint respected
    assert cl.conflict_paths == ("shared.toml",)
    assert cl.size == 3
    assert cl.rebases_avoided == 2


def test_cluster_uses_merge_tree_edges_not_file_overlap() -> None:
    # Two PRs edit the SAME file but produce NO real conflict edge (disjoint regions). File-overlap
    # would fuse them; conflict-clustering must keep them as separate singleton lanes.
    a = _node(1, files=frozenset({"big.rs"}))
    b = _node(2, files=frozenset({"big.rs"}))
    # No ConflictEdge between them => separate clusters.
    clusters = cluster_by_conflict([a, b], [], [])
    assert {cl.members for cl in clusters} == {(1,), (2,)}
    # Contrast: the file-overlap fallback WOULD have linked them into one conflict edge.
    assert [(e.a, e.b) for e in build_conflict_edges_file_overlap([a, b])] == [(1, 2)]


def test_rebases_avoided_sums_size_minus_one() -> None:
    clusters = (
        Cluster(members=(1, 2, 3, 4, 5), conflict_paths=("x",)),  # 4 avoided
        Cluster(members=(6, 7), conflict_paths=("y",)),  # 1 avoided
        Cluster(members=(8,), conflict_paths=()),  # 0
    )
    assert rebases_avoided(clusters) == 5
    assert [c.rebases_avoided for c in clusters] == [4, 1, 0]
