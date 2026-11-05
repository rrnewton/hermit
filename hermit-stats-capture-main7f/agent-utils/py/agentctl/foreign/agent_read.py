#!/usr/bin/env python3
"""Print a subagent's output.

Modes:
  --last            just the last completed turn's final answer (last-message.txt)
  --since-turn N    the transcript from the START of turn N onward
  --tail K          the last K lines of the transcript
  (default)         the whole transcript

Usage:
  agent_read.py NAME [--last | --since-turn N | --tail K]
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

if __package__ in (None, ""):
    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from agentctl.foreign import lib


def main() -> int:
    """Print the requested worker transcript or scrollback view."""
    ap = argparse.ArgumentParser(description="Read a subagent's transcript.")
    ap.add_argument("name")
    group = ap.add_mutually_exclusive_group()
    group.add_argument("--last", action="store_true", help="last turn's final answer")
    group.add_argument("--since-turn", type=int, default=None, help="from turn N onward")
    group.add_argument("--tail", type=int, default=None, help="last K transcript lines")
    args = ap.parse_args()

    mode = "all"
    if args.last:
        mode = "last"
    elif args.since_turn is not None:
        mode = "since_turn"
    elif args.tail is not None:
        mode = "tail"
    try:
        res = lib.read_agent_output(
            args.name,
            mode=mode,
            since_turn=args.since_turn,
            tail=args.tail,
        )
    except lib.AgentOperationError as exc:
        lib.die(exc.message)
    sys.stdout.write(res.text)
    if not res.text.endswith("\n"):
        sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
