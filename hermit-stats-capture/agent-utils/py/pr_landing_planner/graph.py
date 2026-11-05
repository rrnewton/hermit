"""PURE graph algorithms over pre-collected PRs and edges — no network, no git, no I/O.

The collection layer (:mod:`pr_landing_planner.collect`, which drives a
:class:`pr_landing_planner.host.VcsHost`) produces the nodes and the raw conflict / overlap /
ordering edges. Everything in THIS module is a deterministic function of that data, so it is directly
unit-testable and provides a stable contract for every implementation:

* file-overlap edges + the file-overlap conflict FALLBACK (used when merge-tree is disabled),
* base-ref (explicit stacking) ordering edges,
* transitive reduction of the ordering DAG + stack extraction,
* held-PR reasoning (draft / review / base-conflict / dependency-cycle, propagated transitively),
* the parallel-safe partition: a greedy independent-set layering over the real conflict graph that
  respects ordering edges.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence

from pr_landing_planner.mechanism import Mechanism, classify
from pr_landing_planner.model import (
    Cluster,
    ConflictEdge,
    HeldPr,
    MechanismEdge,
    OrderingEdge,
    OverlapEdge,
    PrNode,
    ReviewBinding,
    UnclassifiedMechanism,
)
from pr_landing_planner.landing_context import REQUIRED_REVIEW_LANES

#: PRs/tasks may declare the mechanism they change with a ``mechanism:<slug>`` label (owner
#: convention). The slug is one derive source, fed through the same classifier
#: as diff-derived symbols so a label and a raw identifier for the same mechanism normalise together.
MECHANISM_LABEL_PREFIX = "mechanism:"


# --------------------------------------------------------------------------- mechanism (semantic) edges
def mechanism_slugs(labels: Sequence[str]) -> tuple[str, ...]:
    """The distinct ``mechanism:<slug>`` slugs declared by ``labels``, sorted and de-duplicated."""
    slugs = [
        label[len(MECHANISM_LABEL_PREFIX) :]
        for label in labels
        if label.startswith(MECHANISM_LABEL_PREFIX) and len(label) > len(MECHANISM_LABEL_PREFIX)
    ]
    return tuple(sorted(dict.fromkeys(slugs)))


def mechanism_candidates(node: PrNode) -> tuple[str, ...]:
    """DERIVE stage: all mechanism-candidate strings for a PR — label slugs + diff-derived symbols.

    Deliberately does NOT feed arbitrary labels (``p1``, ``draft``): only declared ``mechanism:``
    slugs and mechanically-derived diff symbols are candidates, so an UNCLASSIFIED result stays
    meaningful instead of drowning in ordinary label noise.
    """
    return tuple(sorted(dict.fromkeys((*mechanism_slugs(node.labels), *node.mechanism_symbols))))


def classify_node_mechanisms(node: PrNode) -> tuple[frozenset[Mechanism], tuple[str, ...]]:
    """CLASSIFY a PR's candidates: ``(recognised mechanisms, UNCLASSIFIED candidate strings)``."""
    recognised: set[Mechanism] = set()
    unclassified: list[str] = []
    for candidate in mechanism_candidates(node):
        mechanism = classify(candidate)
        if mechanism is None:
            unclassified.append(candidate)
        else:
            recognised.add(mechanism)
    return frozenset(recognised), tuple(unclassified)


def build_mechanism_edges(nodes: Sequence[PrNode]) -> tuple[MechanismEdge, ...]:
    """CLUSTER stage: undirected edges between PRs that share a mechanism ENUM VALUE (not raw string).

    Clustering on the normalised enum is what catches the collision raw derivation misses: two PRs
    that touch the same mechanism under different spellings in different files land in one bucket. It
    only surfaces the pair; it never judges whether the intents agree.
    """
    recognised = [classify_node_mechanisms(node)[0] for node in nodes]
    edges: list[MechanismEdge] = []
    for i, a in enumerate(nodes):
        if not recognised[i]:
            continue
        for j in range(i + 1, len(nodes)):
            shared = recognised[i] & recognised[j]
            if shared:
                mechanisms = tuple(sorted(m.value for m in shared))
                edges.append(MechanismEdge(a.number, nodes[j].number, mechanisms))
    return tuple(edges)


def build_unclassified_mechanisms(nodes: Sequence[PrNode]) -> tuple[UnclassifiedMechanism, ...]:
    """Per-PR derived candidates the classifier could not map — the "enum needs a new member" signal."""
    out: list[UnclassifiedMechanism] = []
    for node in nodes:
        _, unclassified = classify_node_mechanisms(node)
        if unclassified:
            out.append(UnclassifiedMechanism(node.number, unclassified))
    return tuple(out)


# --------------------------------------------------------------------------- file-based edges
def build_overlap_edges(nodes: Sequence[PrNode]) -> tuple[OverlapEdge, ...]:
    """Undirected file-overlap edges: two PRs whose changed-file sets intersect (semantic risk)."""
    edges: list[OverlapEdge] = []
    for i, a in enumerate(nodes):
        for b in nodes[i + 1 :]:
            shared = tuple(sorted(a.files & b.files))
            if shared:
                edges.append(OverlapEdge(a.number, b.number, shared))
    return tuple(edges)


def build_conflict_edges_file_overlap(nodes: Sequence[PrNode]) -> tuple[ConflictEdge, ...]:
    """Fast, conservative conflict FALLBACK: treat any shared-file pair as a conflict.

    Used when ``--conflict-detector file-overlap`` is selected (or merge-tree is unavailable). This
    over-serializes PRs that share a file but do not truly collide — which is exactly why the default
    detector is the real ``git merge-tree`` probe in :mod:`pr_landing_planner.collect`."""
    edges: list[ConflictEdge] = []
    for i, a in enumerate(nodes):
        for b in nodes[i + 1 :]:
            shared = tuple(sorted(a.files & b.files))
            if shared:
                edges.append(ConflictEdge(a.number, b.number, shared))
    return tuple(edges)


def build_ordering_edges_base_ref(nodes: Sequence[PrNode]) -> tuple[OrderingEdge, ...]:
    """Explicit-stacking ordering edges: PR B's base branch IS PR A's head branch => A before B."""
    by_head_ref = {n.head_ref: n for n in nodes}
    edges: list[OrderingEdge] = []
    for node in nodes:
        predecessor = by_head_ref.get(node.base_ref)
        if predecessor is not None and predecessor.number != node.number:
            edges.append(OrderingEdge(predecessor.number, node.number, "base-ref"))
    return tuple(edges)


def dedupe_ordering(edges: Sequence[OrderingEdge]) -> tuple[OrderingEdge, ...]:
    """De-duplicate ordering edges by (before, after), keeping the first reason seen, sorted."""
    seen: dict[tuple[int, int], OrderingEdge] = {}
    for edge in edges:
        seen.setdefault((edge.before, edge.after), edge)
    return tuple(sorted(seen.values(), key=lambda e: (e.before, e.after)))


# --------------------------------------------------------------------------- transitive reduction
def _has_path(
    adjacency: dict[int, set[int]], start: int, target: int, skip: tuple[int, int]
) -> bool:
    pending = [start]
    seen: set[int] = set()
    while pending:
        current = pending.pop()
        if current in seen:
            continue
        seen.add(current)
        for child in adjacency.get(current, set()):
            if (current, child) == skip:
                continue
            if child == target:
                return True
            pending.append(child)
    return False


def transitive_reduce(edges: Sequence[OrderingEdge]) -> tuple[OrderingEdge, ...]:
    """Drop any ordering edge that is implied by a longer path (transitive reduction of the DAG)."""
    adjacency: dict[int, set[int]] = {}
    for edge in edges:
        adjacency.setdefault(edge.before, set()).add(edge.after)
    return tuple(
        edge
        for edge in edges
        if not _has_path(adjacency, edge.before, edge.after, (edge.before, edge.after))
    )


def build_stacks(edges: Sequence[OrderingEdge]) -> tuple[tuple[int, ...], ...]:
    """Root-to-leaf dependency chains (length >= 2) over the transitively-reduced ordering DAG."""
    reduced = transitive_reduce(edges)
    children: dict[int, set[int]] = {}
    parents: dict[int, set[int]] = {}
    involved: set[int] = set()
    for edge in reduced:
        children.setdefault(edge.before, set()).add(edge.after)
        parents.setdefault(edge.after, set()).add(edge.before)
        involved.update((edge.before, edge.after))
    roots = sorted(node for node in involved if not parents.get(node))
    stacks: list[tuple[int, ...]] = []

    def visit(node: int, path: tuple[int, ...]) -> None:
        next_nodes = sorted(children.get(node, set()))
        if not next_nodes:
            if len(path) > 1:
                stacks.append(path)
            return
        for child in next_nodes:
            if child in path:
                continue
            visit(child, (*path, child))

    for root in roots:
        visit(root, (root,))
    return tuple(stacks)


# --------------------------------------------------------------------------- held reasoning
def held_reasons(
    nodes: Sequence[PrNode], ordering_edges: Sequence[OrderingEdge]
) -> tuple[HeldPr, ...]:
    """Compute per-PR hold reasons, propagating ``depends-on-held`` through ordering edges.

    Base reasons include draft state, unresolved review, conflicts with the base, and membership in
    an ordering cycle. Any PR ordered after a held PR inherits ``depends-on-held:#N`` until
    fixpoint. Unknown non-empty review decisions fail closed rather than being treated as approval.
    """
    adjacency: dict[int, set[int]] = {}
    for edge in ordering_edges:
        adjacency.setdefault(edge.before, set()).add(edge.after)

    cycle_nodes: set[int] = set()
    for start in adjacency:
        pending = list(adjacency.get(start, ()))
        seen: set[int] = set()
        while pending:
            current = pending.pop()
            if current == start:
                cycle_nodes.add(start)
                break
            if current in seen:
                continue
            seen.add(current)
            pending.extend(adjacency.get(current, ()))

    reasons: dict[int, list[str]] = {}
    for node in nodes:
        node_reasons: list[str] = []
        if node.is_draft:
            node_reasons.append("draft")
        if node.review_evidence_unavailable:
            node_reasons.append("review-evidence-unavailable")
        review = node.review_decision.strip().upper()
        if review == "REVIEW_REQUIRED":
            node_reasons.append("review-required")
        elif review == "CHANGES_REQUESTED":
            if not node.review_objections_resolved:
                node_reasons.append("changes-requested")
        elif review and review != "APPROVED":
            node_reasons.append(f"review-decision-unknown:{review}")
        node_reasons.extend(review_binding(node)[1])
        if node.base_conflict_paths:
            node_reasons.append("local-base-conflict")
        if node.mergeable == "CONFLICTING":
            node_reasons.append("github-base-conflicting")
        if node.number in cycle_nodes:
            node_reasons.append("ordering-cycle")
        if node_reasons:
            reasons[node.number] = node_reasons

    changed = True
    while changed:
        changed = False
        for edge in ordering_edges:
            if edge.before in reasons and edge.after not in reasons:
                reasons[edge.after] = [f"depends-on-held:#{edge.before}"]
                changed = True

    return tuple(
        HeldPr(pr=number, reasons=tuple(reasons[number])) for number in sorted(reasons)
    )


def review_binding(node: PrNode) -> tuple[ReviewBinding, tuple[str, ...]]:
    """Dereference review PASS labels through exact-head caller receipts.

    A head move always makes the receipt stale, even for a patch-identical rebase,
    until the reviewer re-attests the bounded delta at the new head.
    """
    protocol_active = bool(node.review_pass_heads) or any(
        label == "post-facto-human-review"
        or label.startswith("passed-review-")
        or label.startswith("adversarial-review-")
        for label in node.labels
    )
    if not protocol_active:
        return ReviewBinding.NOT_REQUIRED, ()

    receipts = dict(node.review_pass_heads)
    reasons: list[str] = []
    for lane in REQUIRED_REVIEW_LANES:
        if f"passed-review-{lane}" not in node.labels:
            reasons.append(f"review-pass-missing:{lane}")
            continue
        reviewed = receipts.get(lane)
        if reviewed is None:
            reasons.append(f"review-pass-unbound:{lane}")
        elif reviewed != node.head_sha:
            reasons.append(
                f"review-pass-stale:{lane}:reviewed={reviewed}:current={node.head_sha}"
            )

    if not reasons:
        binding = ReviewBinding.EXACT_HEAD
    elif any(reason.startswith("review-pass-stale:") for reason in reasons):
        binding = ReviewBinding.STALE
    elif any(reason.startswith("review-pass-unbound:") for reason in reasons):
        binding = ReviewBinding.UNBOUND
    else:
        binding = ReviewBinding.MISSING
    return binding, tuple(reasons)


# --------------------------------------------------------------------------- parallel-safe partition
def partition_parallel_safe(
    nodes: Sequence[PrNode],
    conflict_edges: Sequence[ConflictEdge],
    ordering_edges: Sequence[OrderingEdge],
    exclude: frozenset[int] = frozenset(),
) -> tuple[tuple[int, ...], ...]:
    """Greedy layering into parallel-safe groups (independent sets in the conflict graph).

    A PR joins the earliest group where (a) every ordering-predecessor is already placed in a
    strictly-earlier group and (b) it does not conflict with any PR already placed in that group.
    Within a layer, PRs are considered in the deterministic land priority: priority (lower first),
    then diff size, then age (``created_at``), then PR number — so the emitted order is stable and
    matches the fusion table's "priority -> size -> age" ranking. ``exclude`` drops held PRs.
    """
    active = [n for n in nodes if n.number not in exclude]
    numbers = {n.number for n in active}
    by_number = {n.number: n for n in active}

    conflicts: dict[int, set[int]] = {n: set() for n in numbers}
    for edge in conflict_edges:
        if edge.a in numbers and edge.b in numbers:
            conflicts[edge.a].add(edge.b)
            conflicts[edge.b].add(edge.a)

    predecessors: dict[int, set[int]] = {n: set() for n in numbers}
    for oedge in ordering_edges:
        if oedge.before in numbers and oedge.after in numbers:
            predecessors[oedge.after].add(oedge.before)

    def rank(number: int) -> tuple[int, int, str, int]:
        node = by_number[number]
        return (node.priority, node.size, node.created_at, node.number)

    remaining = set(numbers)
    placed: set[int] = set()
    groups: list[tuple[int, ...]] = []
    while remaining:
        ready = sorted(
            (n for n in remaining if predecessors[n].issubset(placed)), key=rank
        )
        if not ready:
            # An ordering cycle among the rest: emit each remaining PR as its own singleton group
            # (deterministically), so we never loop forever.
            for number in sorted(remaining, key=rank):
                groups.append((number,))
            break
        group: list[int] = []
        for number in ready:
            if all(peer not in conflicts[number] for peer in group):
                group.append(number)
        groups.append(tuple(group))
        remaining.difference_update(group)
        placed.update(group)
    return tuple(groups)


# --------------------------------------------------------------------------- conflict clustering
def connected_components(
    numbers: Sequence[int], conflict_edges: Sequence[ConflictEdge]
) -> tuple[tuple[int, ...], ...]:
    """Connected components of the UNDIRECTED real-conflict graph (union-find).

    Every number in ``numbers`` appears in exactly one component; a PR with no conflict edge is its
    own singleton. Components are returned largest-first, ties broken by least member; each component's
    members are sorted ascending. Edges naming a number absent from ``numbers`` are ignored. This is
    the clustering step the parallel-safe partition is the DUAL of: partitioning splits a component
    across layers to run in parallel, whereas clustering keeps a component together to land as one
    rebase stack."""
    parent: dict[int, int] = {n: n for n in numbers}

    def find(x: int) -> int:
        root = x
        while parent[root] != root:
            root = parent[root]
        while parent[x] != root:
            parent[x], x = root, parent[x]
        return root

    def union(a: int, b: int) -> None:
        ra, rb = find(a), find(b)
        if ra != rb:
            lo, hi = (ra, rb) if ra < rb else (rb, ra)
            parent[hi] = lo

    for edge in conflict_edges:
        if edge.a in parent and edge.b in parent:
            union(edge.a, edge.b)

    groups: dict[int, list[int]] = {}
    for n in parent:
        groups.setdefault(find(n), []).append(n)
    components = [tuple(sorted(members)) for members in groups.values()]
    components.sort(key=lambda comp: (-len(comp), comp[0]))
    return tuple(components)


def _land_rank(node: PrNode) -> tuple[int, int, str, int]:
    """The deterministic land priority shared by partitioning and stack ordering."""
    return (node.priority, node.size, node.created_at, node.number)


def _stack_order(
    members: Sequence[int],
    by_number: Mapping[int, PrNode],
    ordering_edges: Sequence[OrderingEdge],
) -> tuple[int, ...]:
    """Deterministic topological order (base -> tip) over the intra-member ordering edges.

    Kahn's algorithm with a land-rank tie-break: among the currently-buildable (in-degree-zero)
    members, the lowest land rank lands first. Members with no ordering constraint therefore fall back
    to pure rank order. A residual cycle degrades to rank order over whatever remains, so the function
    always returns a total order of every member and never loops. Because the result is a total order,
    dropping any one member leaves the rest in the same relative order — the stack is not
    all-or-nothing."""
    member_set = set(members)
    succ: dict[int, set[int]] = {m: set() for m in member_set}
    indeg: dict[int, int] = {m: 0 for m in member_set}
    for edge in ordering_edges:
        if (
            edge.before in member_set
            and edge.after in member_set
            and edge.after not in succ[edge.before]
        ):
            succ[edge.before].add(edge.after)
            indeg[edge.after] += 1

    def rank(number: int) -> tuple[int, int, str, int]:
        return _land_rank(by_number[number])

    ready = sorted((m for m in member_set if indeg[m] == 0), key=rank)
    ordered: list[int] = []
    placed: set[int] = set()
    while ready:
        node = ready.pop(0)
        ordered.append(node)
        placed.add(node)
        for child in succ[node]:
            indeg[child] -= 1
            if indeg[child] == 0:
                ready.append(child)
        ready.sort(key=rank)
    if len(ordered) < len(member_set):  # residual ordering cycle: append leftovers deterministically
        ordered.extend(sorted(member_set - placed, key=rank))
    return tuple(ordered)


def cluster_by_conflict(
    nodes: Sequence[PrNode],
    conflict_edges: Sequence[ConflictEdge],
    ordering_edges: Sequence[OrderingEdge] = (),
) -> tuple[Cluster, ...]:
    """Group PRs into stack-landable clusters: connected components of the REAL-conflict graph.

    Each :class:`~pr_landing_planner.model.Cluster` is one connected component (PRs that transitively
    conflict and so cannot land in parallel), with its members in STACK ORDER (base -> tip) and the
    union of the component's real conflicting paths. Distinct clusters share no real conflict by
    construction, so they are the parallel landing LANES. Clustering runs over the merge-tree
    ``conflict_edges``, NOT shared-file overlap: disjoint-region edits of one file produce no conflict
    edge and so are NOT falsely fused into one stack (contrast
    :func:`build_conflict_edges_file_overlap`)."""
    by_number = {n.number: n for n in nodes}
    components = connected_components([n.number for n in nodes], conflict_edges)
    edge_pairs = [(frozenset((e.a, e.b)), e.paths) for e in conflict_edges]
    clusters: list[Cluster] = []
    for comp in components:
        comp_set = set(comp)
        paths: set[str] = set()
        for pair, ps in edge_pairs:
            if pair <= comp_set:
                paths.update(ps)
        members = _stack_order(comp, by_number, ordering_edges)
        clusters.append(Cluster(members=members, conflict_paths=tuple(sorted(paths))))
    return tuple(clusters)


def rebases_avoided(clusters: Sequence[Cluster]) -> int:
    """Total serial rebases avoided by landing each cluster as one stack: sum of (size - 1).

    Serial landing rebases every one of a cluster's N PRs onto the freshly-moved base (N rebases);
    landing the cluster as a single rebase chain costs one, so the cluster saves N-1. Singletons
    contribute 0."""
    return sum(c.rebases_avoided for c in clusters)


__all__ = [
    "MECHANISM_LABEL_PREFIX",
    "mechanism_slugs",
    "mechanism_candidates",
    "classify_node_mechanisms",
    "build_mechanism_edges",
    "build_unclassified_mechanisms",
    "build_overlap_edges",
    "build_conflict_edges_file_overlap",
    "build_ordering_edges_base_ref",
    "dedupe_ordering",
    "transitive_reduce",
    "build_stacks",
    "held_reasons",
    "partition_parallel_safe",
    "connected_components",
    "cluster_by_conflict",
    "rebases_avoided",
]
