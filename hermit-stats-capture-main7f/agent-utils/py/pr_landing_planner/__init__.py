"""pr-landing-planner: a conflict-graph + CI-aware, advisory PR-landing planner.

Collect the open pull requests targeting a base branch, build the real merge-conflict graph (via
``git merge-tree``, with a file-overlap fallback), classify each red CI into one of five failure
modes (real / flaky / stale-required-check / evaluate-once-race / runner-outage), apply exact-identity
validation evidence and policy disposition, surface mechanism overlaps, compute freshness and holds,
partition into parallel-safe groups, and assign each PR a recommended action. It is ADVISORY ONLY —
it recommends actions and never mutates a pull request.

The pure core (:mod:`pr_landing_planner.graph` / ``classify`` / ``plan`` / ``emit``) operates on
already-collected data and is unit-testable with :class:`pr_landing_planner.fakehost.FakeHost`; the
only side-effecting boundary is the :class:`pr_landing_planner.host.VcsHost` protocol, implemented by
:class:`pr_landing_planner.githubhost.GitHubHost` (``gh`` + ``git``).

    from pr_landing_planner.fakehost import FakeHost, load_fixture_text
    from pr_landing_planner.collect import collect_graph
    from pr_landing_planner.plan import assemble_result
    from pr_landing_planner.emit import render_human
    host, repo, base = FakeHost.from_fixture(load_fixture_text(text, as_yaml=True))
    result = assemble_result(collect_graph(host, repo=repo, base=base))
    print(render_human(result))
"""

from __future__ import annotations

from pr_landing_planner.classify import (
    ClassifyConfig,
    FlakySignature,
    classify_pr,
    classify_state,
    flaky_signatures_from_objs,
    parse_rollup,
)
from pr_landing_planner.collect import (
    CONFLICT_DETECTOR_FILE_OVERLAP,
    CONFLICT_DETECTOR_MERGE_TREE,
    CollectionError,
    collect_graph,
    select_prs,
)
from pr_landing_planner.emit import render_actions, render_human, render_json
from pr_landing_planner.fakehost import FakeHost, FixtureError, load_fixture_text
from pr_landing_planner.githubhost import GitHubHost
from pr_landing_planner.graph import (
    build_stacks,
    held_reasons,
    partition_parallel_safe,
    review_binding,
    transitive_reduce,
)
from pr_landing_planner.host import VcsHost
from pr_landing_planner.model import (
    CheckRun,
    CiState,
    CiVerdict,
    CollectedGraph,
    ConflictEdge,
    Diagnostics,
    HeldPr,
    MechanismEdge,
    OrderingEdge,
    OverlapEdge,
    Plan,
    PlanResult,
    PrAction,
    PrActionDecision,
    PrNode,
    PolicyClass,
    RawPr,
    RedClass,
    ReviewBinding,
    ValidationAuthority,
    ValidationEvidence,
)
from pr_landing_planner.plan import assemble_result, compute_plan
from pr_landing_planner.priority import (
    LabelPriority,
    NonePriority,
    PriorityProvider,
    make_priority_provider,
)

__version__: str = "0.1.0"

__all__ = [
    "__version__",
    # model
    "CheckRun",
    "CiState",
    "CiVerdict",
    "CollectedGraph",
    "ConflictEdge",
    "Diagnostics",
    "HeldPr",
    "MechanismEdge",
    "OrderingEdge",
    "OverlapEdge",
    "Plan",
    "PlanResult",
    "PrAction",
    "PrActionDecision",
    "PrNode",
    "PolicyClass",
    "RawPr",
    "RedClass",
    "ReviewBinding",
    "ValidationAuthority",
    "ValidationEvidence",
    # classify
    "ClassifyConfig",
    "FlakySignature",
    "classify_pr",
    "classify_state",
    "parse_rollup",
    "flaky_signatures_from_objs",
    # graph
    "build_stacks",
    "held_reasons",
    "partition_parallel_safe",
    "review_binding",
    "transitive_reduce",
    # collect
    "collect_graph",
    "select_prs",
    "CollectionError",
    "CONFLICT_DETECTOR_MERGE_TREE",
    "CONFLICT_DETECTOR_FILE_OVERLAP",
    # plan
    "assemble_result",
    "compute_plan",
    # emit
    "render_human",
    "render_json",
    "render_actions",
    # hosts + priority
    "VcsHost",
    "GitHubHost",
    "FakeHost",
    "FixtureError",
    "load_fixture_text",
    "PriorityProvider",
    "NonePriority",
    "LabelPriority",
    "make_priority_provider",
]
