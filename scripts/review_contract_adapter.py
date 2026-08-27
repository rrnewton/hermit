#!/usr/bin/env python3
"""Load the content-pinned ci-hub review-label contract without copying it."""

from __future__ import annotations

import argparse
from functools import cache
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import sys
from types import ModuleType
from typing import Callable, Sequence, cast


AUTHORITY_COMMIT = "9f9517bb94354c307de7324d507ff24af7974560"
AUTHORITY_SHA256 = "1b0d798e55f8a5976a4334255a5e7de1792a79e81c6d27034d31d11d4294c18d"
AUTHORITY_RELATIVE_PATH = Path("ci-hub/review_contract.py")
AUTHORITY_API_PATH = (
    "repos/rrnewton/dev-hermit/contents/ci-hub/review_contract.py"
    f"?ref={AUTHORITY_COMMIT}"
)


def _candidate_authorities() -> list[Path]:
    if parent := os.environ.get("DEV_HERMIT_PARENT"):
        return [Path(parent) / AUTHORITY_RELATIVE_PATH]

    candidates: list[Path] = []
    for start in (Path(__file__).resolve(), Path.cwd().resolve()):
        candidates.extend(parent / AUTHORITY_RELATIVE_PATH for parent in start.parents)
    return candidates


def _fetch_pinned_source() -> bytes:
    """Fetch the private parent file through authenticated GitHub tooling."""
    proxy = shutil.which("with-proxy")
    gh = shutil.which("gh")
    if proxy:
        command = [proxy, "gh"]
    elif gh:
        command = [gh]
    else:
        raise RuntimeError(
            "cannot fetch the pinned review-label contract: gh is unavailable; "
            "set DEV_HERMIT_PARENT to a dev-hermit checkout"
        )
    command.extend(
        [
            "api",
            AUTHORITY_API_PATH,
            "-H",
            "Accept: application/vnd.github.raw+json",
        ]
    )
    result = subprocess.run(command, capture_output=True, check=False)
    if result.returncode != 0:
        detail = result.stderr.decode(errors="replace").strip() or "no error output"
        raise RuntimeError(
            "cannot fetch the pinned review-label contract with gh api: "
            f"{detail}"
        )
    return result.stdout


def _verified_source() -> bytes:
    for candidate in _candidate_authorities():
        if candidate.is_file():
            source = candidate.read_bytes()
            if hashlib.sha256(source).hexdigest() == AUTHORITY_SHA256:
                return source

    source = _fetch_pinned_source()
    digest = hashlib.sha256(source).hexdigest()
    if digest != AUTHORITY_SHA256:
        raise RuntimeError(
            "canonical review-label contract digest mismatch: "
            f"expected {AUTHORITY_SHA256}, got {digest}"
        )
    return source


def _load_authority() -> ModuleType:
    name = "ci_hub_review_contract_authority"
    source_name = f"{AUTHORITY_API_PATH}@{AUTHORITY_SHA256}"
    module = ModuleType(name)
    module.__file__ = source_name
    sys.modules[name] = module
    try:
        exec(compile(_verified_source(), source_name, "exec"), module.__dict__)
    except BaseException:
        sys.modules.pop(name, None)
        raise
    return module


@cache
def _authority() -> ModuleType:
    """Load the contract only when a caller first requests its records."""
    return _load_authority()


def lint_records() -> tuple[str, ...]:
    """Return validated tab-separated records for the Hermit shell lint."""
    producer = cast(Callable[[], object], _authority().lint_records)
    records = producer()
    if not isinstance(records, tuple) or not records:
        raise RuntimeError("canonical review-label contract returned no lint records")
    if not all(isinstance(record, str) and record for record in records):
        raise RuntimeError("canonical review-label contract returned malformed lint records")
    return cast(tuple[str, ...], records)


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--format", choices=("lint-records",), default="lint-records")
    parser.parse_args(argv)
    print("\n".join(lint_records()))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
