#!/usr/bin/env python3
"""Public command entrypoint for persistent foreign-harness workers."""

from __future__ import annotations

import sys
from pathlib import Path

if __package__ in (None, ""):
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from agentctl.foreign.cli import main as _dispatch


def main(argv: list[str] | None = None) -> int:
    """Run the persistent-worker dispatcher with the supplied command arguments."""
    return _dispatch(argv)


if __name__ == "__main__":
    raise SystemExit(main())
