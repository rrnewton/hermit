#!/usr/bin/env python3
"""Clear a live subagent's harness context while retaining its warm slot.

Usage:
  agent_reset.py NAME
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

if __package__ in (None, ""):
    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from agentctl.foreign import lib


def main() -> int:
    """Reset a named worker conversation and report the preserved worker identity."""
    ap = argparse.ArgumentParser(description="Reset a live subagent's harness conversation context.")
    ap.add_argument("name", help="live agent name")
    args = ap.parse_args()

    try:
        result = lib.reset_agent_context(args.name)
    except lib.AgentOperationError as exc:
        lib.die(exc.message)
    previous = "present" if result.previous_session_id else "already empty"
    print(f"agent {result.name!r} context reset ({previous}); transcript: {result.transcript}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
