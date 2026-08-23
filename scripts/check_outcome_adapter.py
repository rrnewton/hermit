#!/usr/bin/env python3
"""Load the content-pinned ci-hub check-outcome authority without copying it."""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
from types import ModuleType
from typing import Callable, Protocol, Sequence, cast
from urllib.request import urlopen


AUTHORITY_COMMIT = "4b78d727f35bc8612ac460a6e270dda5f5df304c"
AUTHORITY_SHA256 = "2f1c61d5ec9d98b9697317fd9e66b705161defb69b808d23e6d83384e1e2a1e8"
AUTHORITY_RELATIVE_PATH = Path("ci-hub/check_outcome.py")


def _candidate_authorities() -> list[Path]:
    candidates: list[Path] = []
    if parent := os.environ.get("DEV_HERMIT_PARENT"):
        candidates.append(Path(parent) / AUTHORITY_RELATIVE_PATH)
    for start in (Path(__file__).resolve(), Path.cwd().resolve()):
        candidates.extend(parent / AUTHORITY_RELATIVE_PATH for parent in start.parents)
    return candidates


def _verified_source() -> bytes:
    for candidate in _candidate_authorities():
        if candidate.is_file():
            source = candidate.read_bytes()
            if hashlib.sha256(source).hexdigest() == AUTHORITY_SHA256:
                return source

    url = (
        "https://raw.githubusercontent.com/rrnewton/dev-hermit/"
        f"{AUTHORITY_COMMIT}/{AUTHORITY_RELATIVE_PATH}"
    )
    with urlopen(url, timeout=30) as response:  # exact commit plus digest below
        source = cast(bytes, response.read())
    digest = hashlib.sha256(source).hexdigest()
    if digest != AUTHORITY_SHA256:
        raise RuntimeError(f"canonical check-status authority digest mismatch: {digest}")
    return source


def _load_authority() -> ModuleType:
    with tempfile.NamedTemporaryFile(suffix=".py") as source_file:
        source_file.write(_verified_source())
        source_file.flush()
        spec = importlib.util.spec_from_file_location(
            "ci_hub_check_outcome_authority", source_file.name
        )
        if spec is None or spec.loader is None:
            raise RuntimeError("cannot load canonical check-status authority")
        module = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = module
        spec.loader.exec_module(module)
        return module


class _SelectLatestChecks(Protocol):
    # The authority is loaded dynamically, so its return shape is not statically
    # guaranteed: elements are `object`, forcing every consumer to narrow defensively.
    def __call__(self, value: object, *, head_sha: str = ...) -> list[object]: ...


AUTHORITY = _load_authority()
_classify = cast(Callable[[object, object], object], AUTHORITY.classify_check)
_select_latest = cast(_SelectLatestChecks, AUTHORITY.select_latest_checks)
FAIL_CONCLUSIONS = cast(frozenset[str], AUTHORITY.FAIL_CONCLUSIONS)


def classify_check(status: object, conclusion: object) -> str:
    """Return the canonical authority's PASSED/FAILED/NO_RESULT value."""
    result = _classify(status, conclusion)
    value = getattr(result, "value", None)
    if value not in ("PASSED", "FAILED", "NO_RESULT"):
        raise RuntimeError(f"canonical check-status authority returned {value!r}")
    return cast(str, value)


def select_latest_checks(value: object, *, head_sha: str = "") -> list[object]:
    """Delegate exact-head/latest-context selection to the pinned authority.

    Elements are typed `object` (not `dict`): the authority is loaded dynamically, so no
    element shape is statically guaranteed and callers must narrow defensively.
    """
    return _select_latest(value, head_sha=head_sha)


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
