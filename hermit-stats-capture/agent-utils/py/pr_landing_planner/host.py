"""The VCS-host seam: everything the planner touches the outside world through.

The pure core (:mod:`pr_landing_planner.graph` / ``classify`` / ``plan`` / ``emit``) never talks to a
host; the collector (:mod:`pr_landing_planner.collect`) drives one :class:`VcsHost`. Only three
operations are truly host-specific — listing open PRs, and (carried ON the returned
:class:`~pr_landing_planner.model.RawPr`) each PR's check rollup + labels. The conflict / ancestry /
freshness operations are plain Git and work against any clone, so they live on the same protocol but
have identical semantics for every host.

We ship one real implementation, :class:`pr_landing_planner.githubhost.GitHubHost` (``gh`` + ``git``,
honoring a ``--net-wrapper`` and ``--gh-cmd``), plus :class:`pr_landing_planner.fakehost.FakeHost`
for deterministic, network-free tests and the runnable demo. A GitLab / Gerrit host would implement
this same protocol.
"""

from __future__ import annotations

from collections.abc import Sequence
from typing import Protocol

from pr_landing_planner.model import RawPr


class VcsHost(Protocol):
    """The pluggable boundary between the pure planner and a real VCS host + local git clone."""

    def list_open_prs(self, repo: str, base: str | None) -> tuple[RawPr, ...]:
        """Return every open PR (host-specific); each carries its rollup + labels + api head sha."""
        ...

    def prefetch_refs(self, refspecs: Sequence[tuple[str, str]]) -> dict[str, str]:
        """Fetch every ``(source, dest)`` pair in ONE network round-trip; return ``{dest: sha}``.

        The planner needs the base ref plus every PR head in the local object graph before it can run
        any ``merge-tree`` / ancestry probe. Doing that as N per-PR ``git fetch`` calls is the real
        "expensive fan-out": each fetch is a separate process + remote round-trip, so the cost is N ×
        (spawn + round-trip), not N × (download). Batching all refspecs into a single ``git fetch``
        collapses that to one round-trip; the per-PR analysis afterwards is a handful of local git
        commands (~37 ms each). See :func:`pr_landing_planner.collect.collect_graph` for the measured
        derivation. Every requested ``dest`` MUST appear in the returned mapping.
        """
        ...

    def merge_tree(self, left: str, right: str) -> tuple[str, ...]:
        """Return the conflicting paths of a trial merge of ``left`` and ``right`` (empty = clean)."""
        ...

    def is_ancestor(self, ancestor: str, descendant: str) -> bool:
        """True iff commit ``ancestor`` is an ancestor of commit ``descendant``."""
        ...

    def changed_files(self, base_sha: str, head_sha: str) -> frozenset[str]:
        """The set of files changed on ``base_sha..head_sha`` (merge-base three-dot diff)."""
        ...

    def commits_behind(self, head_sha: str, base_sha: str) -> int:
        """How many commits ``base_sha`` has that ``head_sha`` lacks (the freshness distance)."""
        ...


__all__ = ["VcsHost"]
