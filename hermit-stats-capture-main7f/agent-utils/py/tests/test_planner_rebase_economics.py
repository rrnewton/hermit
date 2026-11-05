"""The plan's rebase/validate economics: clustering avoids rebases and post-facto runs.

A branch behind the fetched base must rebase, while the consuming workspace may authorize its prior
green evidence to carry as soft-green. Landing a real-conflict cluster as ONE stack avoids repeated
rebases and their post-facto validation runs. These tests pin that the emitted plan JSON carries the
economics and that the two counts move together.
"""

from __future__ import annotations

import json

from pr_landing_planner import emit
from pr_landing_planner.emit import VALIDATE_ECONOMICS_RATIONALE, render_json
from pr_landing_planner.model import (
    CiState,
    CiVerdict,
    CollectedGraph,
    ConflictEdge,
    PrNode,
)
from pr_landing_planner.plan import assemble_result


def _node(number: int) -> PrNode:
    return PrNode(
        number=number,
        head_ref=f"feat-{number}",
        base_ref="main",
        head_sha=f"sha-{number}",
        base_sha="base",
        ci=CiVerdict(CiState.GREEN, None, gate_present=True, gate_ok=True, gate_missing_run=False),
    )


def _graph(*conflict_pairs: tuple[int, int], numbers: tuple[int, ...]) -> CollectedGraph:
    return CollectedGraph(
        repository="owner/repo",
        base="main",
        nodes=tuple(_node(n) for n in numbers),
        conflict_edges=tuple(ConflictEdge(a, b, ("f.rs",)) for a, b in conflict_pairs),
        overlap_edges=(),
        ordering_edges=(),
    )


def test_plan_json_carries_rebase_and_validate_economics() -> None:
    # Two conflicting PRs (a cluster of 2) + one independent PR (singleton). Stacking the cluster
    # avoids exactly one rebase — and therefore one post-facto validate run.
    result = assemble_result(_graph((1, 2), numbers=(1, 2, 3)))
    obj = json.loads(render_json(result))
    econ = obj["plan"]["rebase_economics"]
    assert econ["validate_record_keyed_to"] == "head_sha+base_sha"
    assert econ["rebases_avoided_by_clustering"] == 1
    # 1:1 with rebases: each avoided landing also avoids its post-facto validate run.
    assert econ["validate_runs_avoided_by_clustering"] == 1
    assert econ["rationale"] == VALIDATE_ECONOMICS_RATIONALE
    assert "must rebase before landing" in econ["rationale"]
    assert "without pre-landing revalidation" in econ["rationale"]
    assert "post-facto validation remains due" in econ["rationale"]


def test_economics_counts_move_together_and_scale() -> None:
    # A 3-cluster (avoids 2) plus a 2-cluster (avoids 1) => 3 total.
    result = assemble_result(
        _graph((1, 2), (2, 3), (4, 5), numbers=(1, 2, 3, 4, 5, 6))
    )
    econ = json.loads(render_json(result))["plan"]["rebase_economics"]
    assert econ["rebases_avoided_by_clustering"] == 3
    assert econ["validate_runs_avoided_by_clustering"] == econ["rebases_avoided_by_clustering"]


def test_no_conflicts_means_zero_economics() -> None:
    econ = json.loads(render_json(assemble_result(_graph(numbers=(1, 2, 3)))))["plan"][
        "rebase_economics"
    ]
    assert econ["rebases_avoided_by_clustering"] == 0
    assert econ["validate_runs_avoided_by_clustering"] == 0


def test_actions_summary_exposes_validate_runs_avoided() -> None:
    result = assemble_result(_graph((1, 2), numbers=(1, 2, 3)))
    lines = emit.render_actions(result).splitlines()
    summary = dict(
        line.split("=", 1) for line in lines if "=" in line and not line.startswith(("ACTION", "NOTE", "ERROR"))
    )
    assert summary["rebases_avoided"] == "1"
    assert summary["validate_runs_avoided"] == "1"
