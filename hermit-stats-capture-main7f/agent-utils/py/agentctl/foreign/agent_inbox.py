#!/usr/bin/env python3
"""Inspect or recover the bounded-retry inbox of an interactive TUI subagent.

Usage:
  agent_inbox.py NAME --inspect
  agent_inbox.py NAME --drain [--clear-composer]

``--clear-composer`` deliberately discards a visible unsent draft before the
drain. Use it for a known poison message, not while a human is typing.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

if __package__ in (None, ""):
    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from agentctl.foreign import lib


def _print_snapshot(result: lib.TuiInboxResult) -> None:
    print(f"agent: {result.name}")
    print(f"pending: {result.pending or 'none'}")
    print(f"quarantined: {result.quarantined or 'none'}")
    print(f"failed messages: {result.failed_dir}")


def main() -> int:
    """Inspect pending TUI delivery artifacts or attempt a bounded drain."""
    parser = argparse.ArgumentParser(description="Inspect or drain a TUI subagent inbox.")
    parser.add_argument("name")
    action = parser.add_mutually_exclusive_group()
    action.add_argument("--inspect", action="store_true", help="show pending and quarantined sequence numbers")
    action.add_argument("--drain", action="store_true", help="retry pending messages and skip poison entries")
    parser.add_argument(
        "--clear-composer",
        action="store_true",
        help="discard the visible TUI draft before draining; use only for known stuck delivery",
    )
    args = parser.parse_args()
    if args.clear_composer and not args.drain:
        parser.error("--clear-composer requires --drain")
    try:
        result = (
            lib.drain_tui_inbox(args.name, clear_composer=args.clear_composer)
            if args.drain
            else lib.tui_inbox_snapshot(args.name)
        )
    except lib.AgentOperationError as exc:
        lib.die(exc.message)
    _print_snapshot(result)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
