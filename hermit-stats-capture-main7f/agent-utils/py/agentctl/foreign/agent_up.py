#!/usr/bin/env python3
"""Bring up a subagent in its own presentation window.

Creates the ``subagents`` tmux session (if needed) plus a window named after the
agent, launches the turn-runner inside it, records a registry row, and enqueues
the brief as the agent's first turn. Runs a gc pass first so a stale same-named
entry is reaped before we reuse the name.

Usage:
  agent_up.py NAME --cwd DIR [--model M] [--harness codex|agy] [--backend tmux|herdr]
              [--mode headless|tui]
              [--brief TEXT | --brief-file FILE]
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path
from typing import Optional

if __package__ in (None, ""):
    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from agentctl.foreign import lib


def _read_brief(args: argparse.Namespace) -> Optional[str]:
    if args.brief_file:
        p = Path(args.brief_file)
        if not p.is_file():
            lib.die(f"--brief-file not found: {p}")
        return p.read_text()
    if args.brief is not None:
        return str(args.brief)
    return None


def main() -> int:
    """Launch a named worker with the selected harness, presentation, and initial task."""
    ap = argparse.ArgumentParser(description="Bring up a subagent.")
    ap.add_argument("name")
    ap.add_argument("--cwd", required=True, help="working root for the agent")
    ap.add_argument("--model", default=None, help="default model (omit = harness default)")
    ap.add_argument("--harness", default=lib.DEFAULT_HARNESS, choices=list(lib.SUPPORTED_HARNESSES))
    ap.add_argument("--backend", choices=list(lib.SUPPORTED_BACKENDS), default=None)
    ap.add_argument(
        "--mode",
        choices=list(lib.SUPPORTED_MODES),
        default=None,
        help="per-agent mode override (otherwise environment/project default)",
    )
    ap.add_argument("--brief", default=None, help="first-turn brief text")
    ap.add_argument("--brief-file", default=None, help="file holding the first-turn brief")
    args = ap.parse_args()

    name = args.name
    brief = _read_brief(args)

    try:
        res = lib.bring_up_agent(
            name,
            cwd=args.cwd,
            brief=brief,
            model=args.model,
            harness=args.harness,
            backend=args.backend,
            mode=args.mode,
        )
    except lib.AgentOperationError as exc:
        lib.die(exc.message)

    for note in res.gc_notes:
        print(f"[gc] {note}")
    if res.queued_turn is not None:
        print(f"queued brief as turn {res.queued_turn}")

    print(
        f"agent {res.agent.name!r} up in {res.agent.backend}/{res.agent.mode} presentation {res.agent.tmux_target} "
        f"(cwd={res.agent.cwd})"
    )
    print(f"  transcript: {res.agent.transcript}")
    if res.agent.backend == "tmux":
        print(f"  attach:     tmux attach -t {lib.TMUX_SESSION}")
    else:
        print(f"  herdr tab:  {res.agent.tmux_target}")
        if res.agent.presentation_pane:
            print(f"  herdr pane: {res.agent.presentation_pane}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
