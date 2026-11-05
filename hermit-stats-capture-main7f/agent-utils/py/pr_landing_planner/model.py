"""Core vocabulary for pr-landing-planner: pure data + enums, no I/O.

A landing planner answers one question over a set of open pull requests (PRs): *given the conflict
graph and live workflow-check health, which PRs can be landed now, which must be rebased first, which reds are
real vs. benign, and in what order should the coordinator act?*

Everything here is a frozen dataclass or an :class:`enum.Enum`. The interesting work
(:mod:`pr_landing_planner.graph`, :mod:`pr_landing_planner.classify`, :mod:`pr_landing_planner.plan`,
:mod:`pr_landing_planner.emit`) is PURE and operates on these values, so it is unit-testable with a
:class:`pr_landing_planner.fakehost.FakeHost` and stable across implementations. The two
side-effecting boundaries (talking to a VCS host, running git) live behind the
:class:`pr_landing_planner.host.VcsHost` protocol.
"""

from __future__ import annotations

from collections.abc import Sequence
from dataclasses import dataclass, field
from enum import Enum

DEFAULT_REPO = ""
DEFAULT_BASE = "main"
#: The legacy default workflow-check name; consuming repository policy decides its role.
DEFAULT_GATE_CHECK = "merge-gate"

NATIVE_REVIEW_STATES = frozenset(
    {"APPROVED", "CHANGES_REQUESTED", "COMMENTED", "DISMISSED", "PENDING"}
)


class CiState(Enum):
    """The trinary workflow-check verdict for a PR's whole check rollup.

    ``PENDING`` and ``NONE`` remain aliases for callers compiled against the
    older vocabulary. Neither is a fourth outcome: active, absent, cancelled,
    skipped, neutral, stale, and unknown checks all have ``NO_RESULT``.
    """

    PASSED = "passed"
    FAILED = "failed"
    NO_RESULT = "no-result"
    GREEN = "passed"
    RED = "failed"
    PENDING = "no-result"
    NONE = "no-result"


class RedClass(Enum):
    """Why a PR's CI is (or looks) red.

    Grounded in common CI failure modes:

    * :attr:`REAL` — a genuine regression; hold and dispatch a fix.
    * :attr:`FLAKY` — a red whose check name / message matches a caller-supplied flaky signature; a
      re-fire of CI is expected to clear it.
    * :attr:`STALE_REQUIRED_CHECK` — the underlying CI is green on the head commit but the required
      gate check froze on a stale result; re-fire the gate, do not treat as a failure.
    * :attr:`EVALUATE_ONCE_RACE` — the gate fired once while full CI was still queued and exited with
      a benign "still queued" message; this is noise — treat as pending.
    * :attr:`RUNNER_OUTAGE` — the gate job itself never executed (blank runner / BlobNotFound /
      near-zero duration), typically across many branches; escalate the CI outage.
    """

    REAL = "real"
    FLAKY = "flaky"
    STALE_REQUIRED_CHECK = "stale-required-check"
    EVALUATE_ONCE_RACE = "evaluate-once-race"
    RUNNER_OUTAGE = "runner-outage"


class PrAction(Enum):
    """The recommended next action for one PR. Advisory only — the planner never mutates anything."""

    LAND_NOW = "land-now"
    REBASE_THEN_LAND = "rebase-then-land"
    REFIRE_STALE_GATE = "refire-stale-gate"
    ESCALATE_RUNNER_OUTAGE = "escalate-runner-outage"
    ESCALATE_GATE_POLICY = "escalate-gate-policy"
    REFIRE_CI = "refire-ci"
    HOLD_FIX = "hold-fix"
    WAIT = "wait"


class ValidationEvidence(Enum):
    """Evidence attached to an exact tested head and recorded base identity."""

    NONE = "none"
    AUTHORITATIVE_CI = "authoritative-ci"
    # Caller-supplied dereferenced record bound to the fetched head and base.
    # A bare label never produces this variant.
    LOCALLY_VALIDATED = "locally-validated"
    CLEAN_VALIDATE_RECORD = "clean-validate-record"


class ValidationAuthority(Enum):
    """Consuming-workspace decision about whether recorded validation authorizes landing."""

    NONE = "none"
    HARD_GREEN = "hard-green"
    SOFT_GREEN = "soft-green"


class ReviewBinding(Enum):
    """Whether required adversarial-review PASS receipts bind to the current head."""

    NOT_REQUIRED = "not-required"
    MISSING = "missing"
    UNBOUND = "unbound"
    STALE = "stale"
    EXACT_HEAD = "exact-head"


class PolicyClass(Enum):
    """Whether a change is routine CI hygiene or changes the gate policy itself."""

    UNCLASSIFIED = "unclassified"
    CI_HYGIENE = "ci-hygiene"
    GATE_POLICY = "gate-policy"


@dataclass(frozen=True)
class CheckRun:
    """One entry in a PR's status-check rollup, narrowed to the fields the classifier needs.

    ``status`` and ``conclusion`` are upper-cased GitHub tokens (e.g. ``COMPLETED`` / ``SUCCESS``);
    ``text`` carries an optional human message (a check title / description) used only for flaky /
    evaluate-once / outage signature matching; ``workflow`` is the owning workflow name when known;
    ``duration_secs`` is the check's run time when known (a near-zero gate run is an outage signal).
    """

    name: str
    status: str = ""
    conclusion: str = ""
    text: str = ""
    workflow: str = ""
    duration_secs: int | None = None


@dataclass(frozen=True)
class ReviewEvidenceEvent:
    """One stable event with separate creation and current-version timestamps."""

    kind: str
    identity: str
    state: str
    head_sha: str
    created_at: str = ""
    updated_at: str = ""
    last_edited_at: str = ""
    body: str = ""
    #: Immutable repository-host login when available. It is authority only for an
    #: exact current retirement whose permission was verified.
    author: str = ""
    #: Current repository permission for an exact current retirement's immutable
    #: repository-host author. Empty for all other events and failed permission checks.
    retirement_actor_permission: str = ""


@dataclass(frozen=True)
class ReviewEvidenceSnapshot:
    """Exact review/comment evidence observed together at one PR head."""

    head_sha: str
    review_decision: str
    events: tuple[ReviewEvidenceEvent, ...]


@dataclass(frozen=True)
class RawPr:
    """A single open PR as produced by a :class:`pr_landing_planner.host.VcsHost`.

    This is the host boundary's output: everything the pure core needs about a PR *before* git
    fetches / merge-tree probes. ``api_head_sha`` is the head commit the host API reported; the
    collector re-fetches the head and aborts if it drifted (the content-identity guard). ``checks``
    is the already-narrowed rollup, and ``labels`` the PR's labels (both used by the pure classifier
    and priority logic).
    """

    number: int
    head_ref: str
    base_ref: str
    api_head_sha: str
    title: str = ""
    author: str = ""
    is_draft: bool = False
    mergeable: str = ""
    review_decision: str = ""
    #: True when the host could not produce the complete exact-head review snapshot.
    review_evidence_unavailable: bool = False
    created_at: str = ""
    updated_at: str = ""
    additions: int = 0
    deletions: int = 0
    labels: tuple[str, ...] = ()
    checks: tuple[CheckRun, ...] = ()
    review_snapshot: ReviewEvidenceSnapshot | None = None
    #: DERIVE-stage output: mechanism-candidate strings pulled mechanically from the diff (const/env
    #: names, YAML keys). Classified into the stable mechanism enum downstream; defaults empty.
    mechanism_symbols: tuple[str, ...] = ()


@dataclass(frozen=True)
class CiVerdict:
    """The classifier's refined verdict for one PR (see :mod:`pr_landing_planner.classify`)."""

    raw_state: CiState
    #: Set whenever the PR presents a red-ish anomaly (real red, flaky, stale gate, race, outage).
    red_class: RedClass | None
    #: True when the required gate check is present in the rollup.
    gate_present: bool
    #: True when the required gate check concluded successfully.
    gate_ok: bool
    #: True when the gate check shows the "never actually ran" outage signature.
    gate_missing_run: bool
    detail: str = ""


@dataclass(frozen=True)
class PrNode:
    """A fully-collected PR: its metadata, changed files, freshness, CI verdict, and hold state.

    This is what the pure graph / plan layers consume. ``files`` is the changed-file set (base..head);
    ``base_conflict_paths`` are paths that conflict with the base itself (a real merge-tree probe, not
    just GitHub's ``mergeable`` flag); ``commits_behind`` is how far the head trails the base (the
    freshness signal); ``priority`` is the resolved ordering priority (lower = more urgent).
    """

    number: int
    head_ref: str
    base_ref: str
    head_sha: str
    base_sha: str
    title: str = ""
    author: str = ""
    is_draft: bool = False
    mergeable: str = ""
    review_decision: str = ""
    #: True when review evidence collection was incomplete or indeterminate.
    review_evidence_unavailable: bool = False
    created_at: str = ""
    updated_at: str = ""
    additions: int = 0
    deletions: int = 0
    labels: tuple[str, ...] = ()
    #: DERIVE-stage output carried through from :class:`RawPr` (see there); classified into the
    #: mechanism enum by the graph layer. Defaults empty for back-compat with predating constructors.
    mechanism_symbols: tuple[str, ...] = ()
    files: frozenset[str] = field(default_factory=frozenset)
    base_conflict_paths: tuple[str, ...] = ()
    commits_behind: int = 0
    ci: CiVerdict = field(
        default_factory=lambda: CiVerdict(CiState.NONE, None, False, False, False, "")
    )
    priority: int = 0
    assigned_agent: str = ""
    validation_evidence: ValidationEvidence = ValidationEvidence.NONE
    validation_authority: ValidationAuthority = ValidationAuthority.NONE
    review_pass_heads: tuple[tuple[str, str], ...] = ()
    #: Digest of the exact review/comment event snapshot collected at ``head_sha``.
    review_evidence_digest: str = ""
    #: Caller authority, bound to ``head_sha`` and ``review_evidence_digest``, that every
    #: recorded review objection has an exact resolution record. GitHub's
    #: aggregate review decision may remain CHANGES_REQUESTED after that record.
    review_objections_resolved: bool = False
    policy_class: PolicyClass = PolicyClass.UNCLASSIFIED

    @property
    def size(self) -> int:
        """Return the total added and deleted line count."""

        return self.additions + self.deletions

    @property
    def base_conflicting(self) -> bool:
        """Return whether the pull request conflicts with its target base."""

        return bool(self.base_conflict_paths) or self.mergeable == "CONFLICTING"


@dataclass(frozen=True)
class ConflictEdge:
    """Two PRs Git confirms cannot merge cleanly with each other (a real merge-tree conflict)."""

    a: int
    b: int
    paths: tuple[str, ...]


@dataclass(frozen=True)
class OverlapEdge:
    """Two PRs that touch the same file(s) but that Git *can* auto-merge — a semantic-review risk."""

    a: int
    b: int
    paths: tuple[str, ...]


@dataclass(frozen=True)
class MechanismEdge:
    """Two PRs that change the same mechanism — a SEMANTIC overlap keyed on a normalised enum value.

    Unlike a :class:`ConflictEdge` (git can't merge them) or an :class:`OverlapEdge` (they share a
    file), a mechanism edge links two PRs that change the SAME mechanism — a config key, flag, label,
    or concurrency group — possibly in DIFFERENT files, under DIFFERENT SPELLINGS, and possibly with
    OPPOSITE intent, so it SURVIVES a clean merge and git has no opinion on it (the #1567-vs-#1575
    ``cancel-in-progress`` near-miss). ``mechanisms`` are canonical :class:`~pr_landing_planner.
    mechanism.Mechanism` values, NOT raw derived strings — that normalisation is what unifies
    ``concurrency.cancel-in-progress`` and ``CANCEL_IN_PROGRESS`` into one bucket. The planner only
    SURFACES the pair for coordinator review; it never judges intent.
    """

    a: int
    b: int
    mechanisms: tuple[str, ...]


@dataclass(frozen=True)
class Cluster:
    """A connected component of the REAL-conflict graph: PRs that must land as ONE rebase stack.

    Two PRs are in the same cluster when Git confirms they conflict (a ``ConflictEdge``), transitively.
    Because distinct clusters share no real conflict, they define independent landing LANES that a
    coordinator can drive concurrently. ``members`` is the intra-cluster STACK ORDER (base -> tip): a
    deterministic topological order over the cluster's ordering edges, tie-broken by land rank
    (priority, size, age, number), so one bad PR mid-stack can be dropped and the rest re-chained in
    the same relative order. ``conflict_paths`` is the union of the real conflicting paths among the
    cluster's members. A cluster of N PRs lands as one rebase chain, avoiding N-1 serial rebases."""

    members: tuple[int, ...]
    conflict_paths: tuple[str, ...] = ()

    @property
    def size(self) -> int:
        """Return the number of pull requests in this conflict cluster."""

        return len(self.members)

    @property
    def rebases_avoided(self) -> int:
        """Return the serial rebase count avoided by landing the cluster as a stack."""

        return max(0, self.size - 1)


@dataclass(frozen=True)
class UnclassifiedMechanism:
    """DERIVE-stage candidate strings on one PR that CLASSIFY could not map to a known mechanism.

    UNCLASSIFIED is a valid, load-bearing output: it is the signal that the mechanism enum may need a
    new member (a classifier that always returns a known value would silently merge distinct
    mechanisms or force-fit a new one). Surfaced loudly so an agent can recognise each candidate as an
    existing member (add an alias) or genuinely new (add an enum member).
    """

    pr: int
    candidates: tuple[str, ...]


@dataclass(frozen=True)
class OrderingEdge:
    """A directed "``before`` must land before ``after``" constraint (base-stacking or ancestry)."""

    before: int
    after: int
    reason: str  # "base-ref" | "ancestry"


@dataclass(frozen=True)
class HeldPr:
    """A PR that cannot land yet, with the reason(s) why (draft / base-conflict / depends-on-held)."""

    pr: int
    reasons: tuple[str, ...]


@dataclass(frozen=True)
class CollectedGraph:
    """The output of the collection layer: nodes + all raw edges, ready for the pure graph/plan."""

    repository: str
    base: str
    nodes: tuple[PrNode, ...]
    conflict_edges: tuple[ConflictEdge, ...]
    overlap_edges: tuple[OverlapEdge, ...]
    ordering_edges: tuple[OrderingEdge, ...]
    #: Semantic same-mechanism overlaps, clustered on the normalised mechanism enum value; defaults
    #: empty for back-compat with callers that predate the mechanism dimension.
    mechanism_edges: tuple[MechanismEdge, ...] = ()
    #: Per-PR derived strings that CLASSIFY could not map — the "enum may need a new member" signal.
    unclassified_mechanisms: tuple[UnclassifiedMechanism, ...] = ()


@dataclass(frozen=True)
class PrActionDecision:
    """One PR's recommended action, the plain-language reason, and its parallel-safe group index."""

    pr: int
    action: PrAction
    why: str
    group: int | None = None


@dataclass(frozen=True)
class Plan:
    """The fused landing plan: parallel-safe groups, the land-now set, an order, and per-PR actions."""

    parallel_safe_groups: tuple[tuple[int, ...], ...]
    land_now: tuple[int, ...]
    order: tuple[int, ...]
    per_pr_actions: tuple[PrActionDecision, ...]
    batch: tuple[int, ...] = ()


@dataclass(frozen=True)
class Diagnostics:
    """The loud, first-class CI diagnostics (No Silent Failure): which reds are which, is CI down."""

    stale_gates: tuple[int, ...]
    flaky_reds: tuple[int, ...]
    real_reds: tuple[int, ...]
    evaluate_once_race: tuple[int, ...]
    outage_prs: tuple[int, ...]
    outage_suspected: bool


@dataclass(frozen=True)
class PlanResult:
    """The whole planner output: the collected graph, stacks, held PRs, the plan, and diagnostics."""

    graph: CollectedGraph
    stacks: tuple[tuple[int, ...], ...]
    held: tuple[HeldPr, ...]
    plan: Plan
    diagnostics: Diagnostics


def edge_key(a: int, b: int) -> tuple[int, int]:
    """Canonical unordered-pair key (min, max) for pairwise edges."""
    return (a, b) if a <= b else (b, a)


def dedupe_priority(labels: Sequence[str]) -> tuple[str, ...]:
    """Deterministic, order-preserving de-duplication of a label sequence."""
    seen: set[str] = set()
    out: list[str] = []
    for label in labels:
        if label not in seen:
            seen.add(label)
            out.append(label)
    return tuple(out)
