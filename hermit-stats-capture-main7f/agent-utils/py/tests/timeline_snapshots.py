"""One place for the suite to ask where a team's vendor snapshots are.

Roughly twenty assertions across six files used to spell the location out as
``archive / "teams" / slug / "source_snapshots"``. That was fine while there was exactly one
answer and wrong the moment there were two, and re-spelling the new default in twenty places
would have reproduced the original mistake with a different constant: a suite that pins a *path*
cannot tell the difference between "the code resolved the location correctly" and "the code and
the test agree on a typo".

So the suite asks the resolver the production code asks. Tests that are specifically *about* the
location -- the default, the legacy layout, the refusals, the migration -- name paths literally,
because there the path is the subject rather than an incidental.
"""

from __future__ import annotations

from pathlib import Path

from wrkviz.snapshot_store import resolve_snapshot_root


def snapshot_root(archive: Path, team_slug: str) -> Path:
    """Return where *team_slug*'s snapshots live under *archive*'s current layout."""

    return resolve_snapshot_root(archive, team_slug).root
