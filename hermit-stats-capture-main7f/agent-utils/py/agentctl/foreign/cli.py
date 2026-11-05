#!/usr/bin/env python3
"""Command dispatcher for persistent foreign-harness worker compatibility."""

from __future__ import annotations

import argparse
import importlib
import importlib.resources
import sys
from collections.abc import Callable
from pathlib import Path
from typing import cast

if __package__ in (None, ""):
    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

COMMANDS = ("up", "send", "read", "status", "down", "inbox", "reset", "migrate", "backend", "turn", "keeper", "mcp")


def main(argv: list[str] | None = None) -> int:
    """Dispatch a worker subcommand or print version, help, or the packaged user guide."""
    arguments = list(sys.argv[1:] if argv is None else argv)
    if arguments == ["--version"]:
        from agentctl import __version__
        print(f"herdr-subagents {__version__}")
        return 0
    if arguments == ["--userguide"]:
        print(importlib.resources.files("agentctl").joinpath("FOREIGN_USER_GUIDE.md").read_text(), end="")
        return 0
    parser = argparse.ArgumentParser(description="Manage persistent foreign-harness workers.")
    parser.add_argument("command", choices=COMMANDS)
    if not arguments or arguments[0] in ("-h", "--help"):
        parser.print_help()
        return 0
    options = parser.parse_args(arguments[:1])
    module_name = "mcp.server" if options.command == "mcp" else f"agent_{options.command}"
    module = importlib.import_module(f"agentctl.foreign.{module_name}")
    entrypoint = cast(Callable[[], int], module.main)
    previous = sys.argv
    try:
        sys.argv = [f"herdr-subagents {options.command}", *arguments[1:]]
        return entrypoint()
    finally:
        sys.argv = previous


if __name__ == "__main__":
    raise SystemExit(main())
