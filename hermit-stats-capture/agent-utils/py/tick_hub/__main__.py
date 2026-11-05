#!/usr/bin/env python3
"""Executable entry point for the console command and ``python -m`` invocation."""

from __future__ import annotations

import os
import sys

_PKG_PARENT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
if _PKG_PARENT not in sys.path:
    sys.path.insert(0, _PKG_PARENT)

from tick_hub.cli import main

if __name__ == "__main__":
    raise SystemExit(main())
