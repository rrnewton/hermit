#!/usr/bin/env python3
"""Delegate check-status classification to the parent ci-hub authority.

Hermit does not implement check-status classification. The authority is
``ci-hub/check_outcome.py`` in the parent dev-hermit checkout, and this module
is the adapter Hermit's scripts call so that every caller reaches that one
implementation instead of keeping a second copy.

The adapter previously lived in agent-utils as ``py/ci_hub_check_outcome.py``.
agent-utils commit ``5ef91c5`` removed it and added
``py/pr_landing_planner/check_outcome.py``, which is a standalone library with
no command-line interface and no ``annotate_rollups``. That module cannot serve
Hermit's callers: three of them exec the file as a program, and a library run as
a program exits 0 having printed nothing, so a failure reads as an empty
result. The path search, the re-exports, and ``annotate_rollups`` below are
carried over from that removed adapter.

The authority is located via ``DEV_HERMIT_PARENT`` (the same variable
``scripts/validate.rs`` uses to find the parent checkout), then by searching the
ancestors of this file and of the working directory. Unlike the removed
adapter, this one does not fetch the authority over the network and does not
verify it against a pinned digest: it resolves a file on disk or fails. A
missing authority is reported and exits non-zero rather than degrading to a
silent empty result.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
from pathlib import Path
import sys
from types import ModuleType
from typing import Sequence

AUTHORITY_RELATIVE_PATH = Path("ci-hub/check_outcome.py")


def candidate_authorities() -> list[Path]:
    """Return every path that may hold the parent ci-hub authority, in order."""
    candidates: list[Path] = []
    if parent := os.environ.get("DEV_HERMIT_PARENT"):
        candidates.append(Path(parent) / AUTHORITY_RELATIVE_PATH)
    for start in (Path(__file__).resolve(), Path.cwd().resolve()):
        candidates.extend(parent / AUTHORITY_RELATIVE_PATH for parent in start.parents)
    return candidates


def authority_path() -> Path:
    """Return the parent ci-hub authority, or raise naming where we looked."""
    for candidate in candidate_authorities():
        if candidate.is_file():
            return candidate
    searched = "\n  ".join(str(path) for path in candidate_authorities())
    raise RuntimeError(
        "cannot find the parent ci-hub check-outcome authority "
        f"({AUTHORITY_RELATIVE_PATH}). Set DEV_HERMIT_PARENT to the dev-hermit "
        f"checkout. Searched:\n  {searched}"
    )


def _load_authority() -> ModuleType:
    path = authority_path()
    spec = importlib.util.spec_from_file_location("ci_hub_check_outcome_authority", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load the parent check-status authority at {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


AUTHORITY = _load_authority()


def classify_check(status: object, conclusion: object) -> str:
    """Return the authority's PASSED/FAILED/NO_RESULT value for one check."""
    result = AUTHORITY.classify_check(status, conclusion)
    value = getattr(result, "value", None)
    if value not in ("PASSED", "FAILED", "NO_RESULT"):
        raise RuntimeError(f"parent check-status authority returned {value!r}")
    return str(value)


def select_latest_checks(value: object, *, head_sha: str = "") -> list[object]:
    """Delegate exact-head/latest-context selection to the parent authority."""
    return list(AUTHORITY.select_latest_checks(value, head_sha=head_sha))


def annotate_rollups(value: object) -> object:
    """Attach the canonical result to every check-like object in JSON."""
    if isinstance(value, list):
        return [annotate_rollups(item) for item in value]
    if not isinstance(value, dict):
        return value
    result: dict[object, object] = {}
    for key, item in value.items():
        if key == "statusCheckRollup":
            item = select_latest_checks(item, head_sha=str(value.get("headRefOid") or ""))
        result[key] = annotate_rollups(item)
    if "status" in value or "conclusion" in value or "state" in value:
        result["_checkOutcome"] = classify_check(
            value.get("status"), value.get("conclusion", value.get("state"))
        )
    return result


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--status", default="")
    parser.add_argument("--conclusion", default="")
    parser.add_argument("--annotate-rollups", action="store_true")
    parser.add_argument("--select-latest-rollup", action="store_true")
    parser.add_argument("--head-sha", default="")
    args = parser.parse_args(argv)
    if args.annotate_rollups and args.select_latest_rollup:
        parser.error("select exactly one rollup mode")
    if args.annotate_rollups:
        json.dump(annotate_rollups(json.load(sys.stdin)), sys.stdout, separators=(",", ":"))
        print()
    elif args.select_latest_rollup:
        json.dump(
            select_latest_checks(json.load(sys.stdin), head_sha=args.head_sha),
            sys.stdout,
            separators=(",", ":"),
        )
        print()
    else:
        print(classify_check(args.status, args.conclusion))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
