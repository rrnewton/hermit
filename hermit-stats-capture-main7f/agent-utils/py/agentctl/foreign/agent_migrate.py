#!/usr/bin/env python3
"""Move an idle live agent between visible backends without resetting history.

Usage:
  agent_migrate.py NAME [--to tmux|herdr] [--mode headless|tui]
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

if __package__ in (None, ""):
    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from agentctl.foreign import lib


def main() -> int:
    """Move an idle worker to a different backend or execution mode."""
    ap = argparse.ArgumentParser(description="Migrate an idle subagent presentation backend.")
    ap.add_argument("name")
    ap.add_argument("--to", choices=list(lib.SUPPORTED_BACKENDS), default="herdr")
    ap.add_argument(
        "--mode",
        choices=list(lib.SUPPORTED_MODES),
        default=None,
        help="destination mode for supported non-Codex runners; Codex is TUI-only",
    )
    args = ap.parse_args()
    try:
        result = lib.migrate_agent(args.name, to_backend=args.to, to_mode=args.mode)
    except lib.AgentOperationError as exc:
        lib.die(exc.message)
    print(
        f"agent {result.name!r} migrated {result.from_backend}/{result.from_mode} -> "
        f"{result.to_backend}/{result.to_mode}; "
        f"presentation {result.tmux_target}; session {'preserved' if result.session_id else 'not yet created'}"
    )
    print(f"  transcript: {result.transcript}")
    if result.to_backend == "herdr":
        print(
            "WARNING: restart any pre-Herdr MCP server before using its subagent tools on this worker; "
            "a temporary tmux guard prevents its old garbage collector from archiving the Herdr runner."
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
