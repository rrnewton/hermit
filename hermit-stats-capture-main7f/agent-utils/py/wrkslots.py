#!/usr/bin/env python3
"""Compatibility entry point for repository checkouts initialized before packaging."""

from __future__ import annotations

import os
import sys

_PACKAGE_PARENT = os.path.dirname(os.path.realpath(__file__))
if _PACKAGE_PARENT not in sys.path:
    sys.path.insert(0, _PACKAGE_PARENT)

from wrkslots.cli import main


if __name__ == "__main__":
    raise SystemExit(main())
