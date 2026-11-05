#!/usr/bin/env python3
"""Repository-checkout wrapper for :mod:`dagrun.profile_report`."""

from __future__ import annotations

import sys
from pathlib import Path


REPO_PY = Path(__file__).resolve().parents[1] / "py"
if str(REPO_PY) not in sys.path:
    sys.path.insert(0, str(REPO_PY))

from dagrun.profile_report import _main


def main() -> int:
    """Run the repository-checkout report generator."""

    return _main()


if __name__ == "__main__":
    raise SystemExit(main())
