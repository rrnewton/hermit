"""Tests for the collection layer against a FakeHost: guard, selection, detectors, freshness."""

from __future__ import annotations

import pytest

from pr_landing_planner.collect import (
    CONFLICT_DETECTOR_FILE_OVERLAP,
    CONFLICT_DETECTOR_MERGE_TREE,
    CollectionError,
    collect_graph,
)
from pr_landing_planner.fakehost import FakeHost, FixtureError

_FIXTURE: dict[str, object] = {
    "repo": "OWNER/NAME",
    "base": "integration",
    "prs": [
        {
            "number": 1,
            "head_ref": "feat-1",
            "changed_files": ["src/shared.rs", "src/a.rs"],
            "commits_behind": 4,
            "checks": [{"name": "merge-gate", "conclusion": "SUCCESS"}],
        },
        {
            "number": 2,
            "head_ref": "feat-2",
            "changed_files": ["src/shared.rs", "src/b.rs"],
            "checks": [{"name": "merge-gate", "conclusion": "SUCCESS"}],
        },
        {
            "number": 3,
            "head_ref": "feat-3",
            "changed_files": ["src/c.rs"],
            "checks": [{"name": "merge-gate", "conclusion": "SUCCESS"}],
        },
    ],
    # #1 and #2 share src/shared.rs, but Git only truly CONFLICTS them here:
    "conflicts": [{"a": 1, "b": 2, "paths": ["src/shared.rs"]}],
}


def _host() -> FakeHost:
    host, _, _ = FakeHost.from_fixture(_FIXTURE)
    return host


def test_merge_tree_detector_finds_only_real_conflicts() -> None:
    graph = collect_graph(_host(), repo="OWNER/NAME", base="integration")
    assert [(e.a, e.b) for e in graph.conflict_edges] == [(1, 2)]
    # #1 and #2 also file-overlap; #3 overlaps nobody.
    assert [(e.a, e.b) for e in graph.overlap_edges] == [(1, 2)]
    behind = {n.number: n.commits_behind for n in graph.nodes}
    assert behind[1] == 4 and behind[2] == 0


def test_file_overlap_detector_is_more_conservative() -> None:
    graph = collect_graph(
        _host(), repo="OWNER/NAME", base="integration",
        conflict_detector=CONFLICT_DETECTOR_FILE_OVERLAP,
    )
    # Same set here because the only shared-file pair is also the only real conflict.
    assert [(e.a, e.b) for e in graph.conflict_edges] == [(1, 2)]


def test_conflict_detector_diverges_when_overlap_is_automergeable() -> None:
    fixture: dict[str, object] = {
        "repo": "R",
        "base": "integration",
        "prs": [
            {"number": 1, "head_ref": "a", "changed_files": ["shared.rs"]},
            {"number": 2, "head_ref": "b", "changed_files": ["shared.rs"]},
        ],
        # No `conflicts` entry => git merge-tree reports them auto-mergeable.
    }
    host, _, _ = FakeHost.from_fixture(fixture)
    mt = collect_graph(host, repo="R", base="integration", conflict_detector=CONFLICT_DETECTOR_MERGE_TREE)
    fo_host, _, _ = FakeHost.from_fixture(fixture)
    fo = collect_graph(fo_host, repo="R", base="integration", conflict_detector=CONFLICT_DETECTOR_FILE_OVERLAP)
    assert mt.conflict_edges == ()  # merge-tree: no real conflict
    assert [(e.a, e.b) for e in fo.conflict_edges] == [(1, 2)]  # file-overlap over-serializes


def test_content_identity_guard_aborts_on_drift() -> None:
    fixture: dict[str, object] = {
        "repo": "R",
        "base": "integration",
        "prs": [
            {
                "number": 1,
                "head_ref": "a",
                "api_head_sha": "AAA",
                "fetched_head_sha": "BBB",  # simulate the PR moving mid-collection
                "changed_files": ["a.rs"],
            }
        ],
    }
    host, _, _ = FakeHost.from_fixture(fixture)
    with pytest.raises(CollectionError, match="changed during collection"):
        collect_graph(host, repo="R", base="integration")


def test_review_snapshot_head_must_match_the_exact_fetched_head() -> None:
    head = "a" * 40
    fixture: dict[str, object] = {
        "repo": "R",
        "base": "integration",
        "prs": [
            {
                "number": 1,
                "head_ref": "a",
                "head_sha": head,
                "api_head_sha": head,
                "fetched_head_sha": head,
                "review_snapshot_head_sha": "b" * 40,
                "review_events": [],
            }
        ],
    }
    host, _, _ = FakeHost.from_fixture(fixture)
    with pytest.raises(CollectionError, match="review evidence changed during collection"):
        collect_graph(host, repo="R", base="integration")


@pytest.mark.parametrize("snapshot_decision", [None, "", "APPROVED"])
def test_review_snapshot_cannot_erase_or_contradict_changes_requested(
    snapshot_decision: object,
) -> None:
    head = "a" * 40
    fixture: dict[str, object] = {
        "repo": "R",
        "base": "integration",
        "prs": [
            {
                "number": 1,
                "head_sha": head,
                "review_decision": "CHANGES_REQUESTED",
                "review_snapshot_review_decision": snapshot_decision,
                "review_events": [
                    {
                        "kind": "review",
                        "identity": "review-1",
                        "author": "reviewer",
                        "state": "CHANGES_REQUESTED",
                        "head_sha": head,
                        "created_at": "2026-09-04T12:00:00Z",
                        "updated_at": "2026-09-04T12:00:00Z",
                        "last_edited_at": "",
                        "body": "please fix",
                    }
                ],
            }
        ],
    }
    host, _, _ = FakeHost.from_fixture(fixture)
    with pytest.raises(CollectionError, match="aggregate review decision changed"):
        collect_graph(host, repo="R", base="integration")


def test_matching_review_decisions_are_accepted() -> None:
    head = "a" * 40
    fixture: dict[str, object] = {
        "repo": "R",
        "base": "integration",
        "prs": [
            {
                "number": 1,
                "head_sha": head,
                "review_decision": "CHANGES_REQUESTED",
                "review_snapshot_review_decision": "CHANGES_REQUESTED",
                "review_events": [
                    {
                        "kind": "review",
                        "identity": "review-1",
                        "author": "reviewer",
                        "state": "CHANGES_REQUESTED",
                        "head_sha": head,
                        "created_at": "2026-09-04T12:00:00Z",
                        "updated_at": "2026-09-04T12:00:00Z",
                        "last_edited_at": "",
                        "body": "please fix",
                    }
                ],
            }
        ],
    }
    host, _, _ = FakeHost.from_fixture(fixture)
    graph = collect_graph(host, repo="R", base="integration")
    assert graph.nodes[0].review_decision == "CHANGES_REQUESTED"


def test_canonical_comment_refusal_holds_without_native_review_decision() -> None:
    head = "a" * 40
    fixture: dict[str, object] = {
        "repo": "R",
        "base": "integration",
        "prs": [
            {
                "number": 1,
                "head_sha": head,
                "review_decision": "",
                "review_snapshot_review_decision": "",
                "review_events": [
                    {
                        "kind": "issue-comment",
                        "identity": "comment-123456",
                        "author": "reviewer",
                        "state": "ACTIVE",
                        "head_sha": "",
                        "created_at": "2026-09-04T12:00:00Z",
                        "updated_at": "2026-09-04T12:00:00Z",
                        "last_edited_at": "",
                        "body": (
                            "[team, reviewer, unresolved, build-host, role=reviewer]\n"
                            f"CHANGES-REQUESTED-AT: codex {head}\n"
                            "The race remains."
                        ),
                    }
                ],
            }
        ],
    }
    host, _, _ = FakeHost.from_fixture(fixture)
    graph = collect_graph(host, repo="R", base="integration")
    assert graph.nodes[0].review_decision == "CHANGES_REQUESTED"


def test_review_evidence_unavailable_survives_collection() -> None:
    fixture: dict[str, object] = {
        "repo": "R",
        "base": "integration",
        "prs": [
            {
                "number": 1,
                "head_sha": "a" * 40,
                "review_decision": "APPROVED",
                "review_evidence_unavailable": True,
                "checks": [{"name": "merge-gate", "conclusion": "SUCCESS"}],
            }
        ],
    }
    host, _, _ = FakeHost.from_fixture(fixture)
    graph = collect_graph(host, repo="R", base="integration")
    node = graph.nodes[0]
    assert node.review_decision == "APPROVED"
    assert node.review_evidence_unavailable
    assert node.review_evidence_digest == ""

@pytest.mark.parametrize("malformed", (None, 0, "true", [], {}))
def test_review_evidence_unavailable_fixture_field_is_strict_boolean(
    malformed: object,
) -> None:
    fixture: dict[str, object] = {
        "repo": "R",
        "base": "integration",
        "prs": [
            {
                "number": 1,
                "head_sha": "a" * 40,
                "review_evidence_unavailable": malformed,
            }
        ],
    }
    with pytest.raises(
        FixtureError, match="field 'review_evidence_unavailable' must be a boolean"
    ):
        FakeHost.from_fixture(fixture)



def test_only_numbers_restricts_selection() -> None:
    graph = collect_graph(
        _host(), repo="OWNER/NAME", base="integration", only=frozenset({1, 3})
    )
    assert {n.number for n in graph.nodes} == {1, 3}


def test_transitive_stack_selection_across_base() -> None:
    fixture: dict[str, object] = {
        "repo": "R",
        "base": "integration",
        "prs": [
            {"number": 10, "head_ref": "root", "base_ref": "integration"},
            {"number": 11, "head_ref": "child", "base_ref": "root"},  # stacked, targets root not base
        ],
    }
    host, _, _ = FakeHost.from_fixture(fixture)
    graph = collect_graph(host, repo="R", base="integration")
    # #11 is pulled in as part of #10's transitive stack even though it targets `root`.
    assert {n.number for n in graph.nodes} == {10, 11}
    assert any(e.before == 10 and e.after == 11 for e in graph.ordering_edges)


def test_fixture_prs_inherit_declared_base() -> None:
    fixture: dict[str, object] = {
        "repo": "R",
        "base": "release",
        "prs": [{"number": 10, "head_ref": "candidate"}],
    }
    host, repo, base = FakeHost.from_fixture(fixture)
    graph = collect_graph(host, repo=repo, base=base)
    assert [node.number for node in graph.nodes] == [10]
    assert graph.nodes[0].base_ref == "release"


def test_unknown_detector_raises() -> None:
    with pytest.raises(ValueError, match="unknown conflict detector"):
        collect_graph(_host(), repo="R", base="integration", conflict_detector="bogus")


class _RecordingHost:
    """Wrap a FakeHost and record how many times refs are fetched, and with which refspecs."""

    def __init__(self, inner: FakeHost) -> None:
        self._inner = inner
        self.prefetch_calls: list[tuple[tuple[str, str], ...]] = []

    def prefetch_refs(self, refspecs):  # type: ignore[no-untyped-def]
        self.prefetch_calls.append(tuple(refspecs))
        return self._inner.prefetch_refs(refspecs)

    def __getattr__(self, name: str):  # type: ignore[no-untyped-def]
        return getattr(self._inner, name)


def test_collection_fetches_in_one_batched_call_not_per_pr() -> None:
    # The whole cost argument for defaulting to merge-tree is that the fetch is ONE round-trip, not a
    # per-PR fan-out. Guard that here: three PRs -> exactly one prefetch call carrying the base ref
    # plus all three PR heads (four refspecs), never one fetch per PR.
    host = _RecordingHost(_host())
    graph = collect_graph(host, repo="OWNER/NAME", base="integration")
    assert {n.number for n in graph.nodes} == {1, 2, 3}
    assert len(host.prefetch_calls) == 1
    sources = {source for source, _dest in host.prefetch_calls[0]}
    assert sources == {
        "refs/heads/integration",
        "refs/pull/1/head",
        "refs/pull/2/head",
        "refs/pull/3/head",
    }
