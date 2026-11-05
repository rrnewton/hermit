"""Source-checkout test configuration for the wrkslots distribution."""

from __future__ import annotations

import sys
from pathlib import Path


PY_ROOT = Path(__file__).resolve().parents[2]
if str(PY_ROOT) not in sys.path:
    sys.path.insert(0, str(PY_ROOT))

# The suite predates disk-image slots and exercises the plain-worktree
# representation; image slots have their own tests (test_slot_images.py and
# e2e_images.sh). New projects created by these tests therefore default to
# worktrees. Subprocesses inherit this.
import os  # noqa: E402

os.environ.setdefault("WRKSLOTS_INIT_REPRESENTATION", "worktree")

import pytest  # noqa: E402


@pytest.fixture(autouse=True)
def idle_validation_run_host(
    request: pytest.FixtureRequest, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Give each in-process test a host on which no validation run exists.

    A validation row's liveness is answered from this host's live processes,
    user-systemd units and control groups, after proving that this process
    sees the whole host.  Lifecycle shards run in their own user and PID
    namespaces and on hosts with no user bus, where that evidence is
    unreadable or the proof fails, and on a busy host the real unit
    population changes under the enumeration.  Neither is what those tests
    examine.  So this replaces the authority's host evidence, the retained
    unit control-group read, and the host-view proof that recovery also
    makes.  Recovery still reads its own process table and units through
    the functions those tests stub.  Tests that examine the evidence itself
    carry the ``validation_run_evidence`` marker and supply it themselves.
    Child processes get the same host from ``idle_validation_host``.
    """

    if request.node.get_closest_marker("validation_run_evidence") is not None:
        return
    from wrkslots import cli
    from wrkslots.tests.idle_validation_host import (
        no_retained_cgroup_members,
        no_validation_runs,
        whole_host_process_view,
    )

    monkeypatch.setattr(cli, "_validation_run_host_evidence", no_validation_runs)
    monkeypatch.setattr(cli, "_retained_unit_cgroup_members", no_retained_cgroup_members)
    monkeypatch.setattr(cli, "_assert_host_process_view", whole_host_process_view)
