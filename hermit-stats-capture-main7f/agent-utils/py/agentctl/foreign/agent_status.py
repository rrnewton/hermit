#!/usr/bin/env python3
"""Show live subagents (runs a gc pass first).

Prints a table of agents and, for each, its transcript path so the coordinator
can arm a Monitor on the ``TURN-DONE`` sentinel.

Usage:
  agent_status.py [NAME]
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

if __package__ in (None, ""):
    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from agentctl.foreign import lib


def _short(sid: str | None) -> str:
    if not sid:
        return "-"
    return sid[:8]


def main() -> int:
    """Display registered workers with process, presentation, queue, and error state."""
    ap = argparse.ArgumentParser(description="Status of live subagents.")
    ap.add_argument("name", nargs="?", default=None)
    args = ap.parse_args()

    try:
        status = lib.status_snapshot(args.name)
    except lib.AgentOperationError as exc:
        lib.die(exc.message)

    for note in status.gc_notes:
        print(f"[gc] {note}")

    if not status.agents:
        print("no live subagents")
        return 0

    header = (
        f"{'NAME':<16} {'BACKEND':<7} {'MODE':<9} {'HARNESS':<8} {'MODEL':<14} {'WIN':<4} "
        f"{'STATUS':<16} {'PEND':<5} {'SESSION':<10} {'LAST_TURN':<20} CWD"
    )
    print(header)
    print("-" * len(header))
    for rec in status.agents:
        win = "yes" if rec.window_alive else ("DEG" if rec.presentation_degraded else "NO")
        model = rec.model or "<default>"
        last = rec.last_turn_at or "-"
        print(
            f"{rec.name:<16} {rec.backend:<7} {rec.mode:<9} {rec.harness:<8} {model:<14} {win:<4} "
            f"{rec.status:<16} {rec.pending:<5} {_short(rec.session_id):<10} "
            f"{last:<20} {rec.cwd}"
        )

    print()
    for rec in status.agents:
        if rec.mode == lib.TUI_MODE:
            print(f"  {rec.name}: Herdr scrollback {rec.transcript}")
        else:
            print(f"  {rec.name}: transcript {rec.transcript}")
        if rec.presentation_degraded:
            print(
                f"  {rec.name}: runner alive but {rec.backend} presentation missing; "
                "use MCP subagent_recreate_window to restore presentation"
            )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
