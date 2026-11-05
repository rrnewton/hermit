#!/usr/bin/env python3
"""Stop a subagent: end its runner, kill its presentation, archive its state.

Graceful: writes a STOP marker and gives the runner a short grace to finish the
in-flight turn, then kills the window regardless. State (including transcripts)
is moved under state/_archive/ rather than deleted, and the registry row is
removed.

Usage:
  agent_down.py NAME [--grace SECONDS]
  agent_down.py NAME --force       # retire without probing an unavailable backend
  agent_down.py --all-dead          # just gc: reap every agent whose window/runner is gone
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

if __package__ in (None, ""):
    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from agentctl.foreign import lib


def _down_one(name: str, grace: float, force: bool) -> int:
    try:
        res = lib.bring_down_agent(name, grace=grace, archive=True, force=force)
    except lib.AgentOperationError as exc:
        lib.die(exc.message)
    if not res.was_registered:
        print(
            f"agent {name!r} was not in the registry; "
            f"killed_window={res.killed_window}; archived {res.archived_to}"
        )
        return 0
    if res.forced:
        print(
            f"WARNING: forced retirement of {name!r}; {res.unverified_presentation} was not "
            f"probed or closed. State archived to {res.archived_to}; registry entry removed."
        )
    else:
        print(f"agent {name!r} down; window killed; state archived to {res.archived_to}")
    return 0


def main() -> int:
    """Retire a named worker or collect dead workers, reporting preserved state."""
    ap = argparse.ArgumentParser(description="Stop a subagent or reap dead ones.")
    ap.add_argument("name", nargs="?", default=None)
    ap.add_argument("--grace", type=float, default=10.0, help="seconds to let a turn finish")
    ap.add_argument("--all-dead", action="store_true", help="gc every dead agent and exit")
    ap.add_argument(
        "--force",
        action="store_true",
        help="retire NAME without probing or closing its presentation backend; archives state loudly",
    )
    args = ap.parse_args()

    if args.all_dead:
        if args.force:
            lib.die("--force requires an agent NAME; --all-dead remains conservative")
        notes = lib.gc()
        if notes:
            for note in notes:
                print(f"[gc] {note}")
        else:
            print("no dead agents to reap")
        return 0

    if args.name is None:
        lib.die("give an agent NAME, or use --all-dead")
    lib.validate_name(args.name)
    return _down_one(args.name, args.grace, args.force)


if __name__ == "__main__":
    raise SystemExit(main())
