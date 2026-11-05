#!/usr/bin/env python3
"""Show or persist the default backend for future subagents.

Usage:
  agent_backend.py
  agent_backend.py --set tmux|herdr
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

if __package__ in (None, ""):
    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from agentctl.foreign import lib


def main() -> int:
    """Show or persist the default backend selected by the command-line arguments."""
    ap = argparse.ArgumentParser(description="Show or set the subagent presentation backend.")
    ap.add_argument("--set", choices=list(lib.SUPPORTED_BACKENDS), dest="backend")
    args = ap.parse_args()
    try:
        if args.backend is not None:
            backend = lib.set_default_backend(args.backend)
            print(f"saved default backend {backend!r} in {lib.BACKEND_CONFIG}")
        else:
            print(f"selected backend: {lib.selected_backend()}")
            print(f"config: {lib.BACKEND_CONFIG}")
    except lib.AgentOperationError as exc:
        lib.die(exc.message)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
