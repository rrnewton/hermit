#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Canonical Hermit interpretation of GitHub check status and conclusion."""

from __future__ import annotations

import argparse
from enum import Enum
import json
import sys
from typing import Any, Sequence


class CheckOutcome(str, Enum):
    PASSED = "PASSED"
    FAILED = "FAILED"
    NO_RESULT = "NO_RESULT"


PASS_CONCLUSIONS = frozenset(("success",))
FAIL_CONCLUSIONS = frozenset(("failure", "timed_out", "error", "startup_failure"))


def classify_check(status: object, conclusion: object) -> CheckOutcome:
    """Preserve absence as NO_RESULT instead of forcing it into pass/fail."""
    normalized_status = str(status or "").strip().lower()
    normalized_conclusion = str(conclusion or "").strip().lower()

    # CheckRun supplies completed/success. A legacy StatusContext has no
    # separate status field, so an empty status with a terminal conclusion is
    # also valid.
    if normalized_status and normalized_status != "completed":
        return CheckOutcome.NO_RESULT
    if normalized_conclusion in PASS_CONCLUSIONS:
        return CheckOutcome.PASSED
    if normalized_conclusion in FAIL_CONCLUSIONS:
        return CheckOutcome.FAILED
    return CheckOutcome.NO_RESULT


def annotate_rollups(value: Any) -> Any:
    """Attach the canonical result to every check-like object in JSON."""
    if isinstance(value, list):
        return [annotate_rollups(item) for item in value]
    if not isinstance(value, dict):
        return value

    result = {key: annotate_rollups(item) for key, item in value.items()}
    if "status" in value or "conclusion" in value or "state" in value:
        result["_checkOutcome"] = classify_check(
            value.get("status"), value.get("conclusion", value.get("state"))
        ).value
    return result


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--status", default="")
    parser.add_argument("--conclusion", default="")
    parser.add_argument("--annotate-rollups", action="store_true")
    args = parser.parse_args(argv)
    if args.annotate_rollups:
        json.dump(annotate_rollups(json.load(sys.stdin)), sys.stdout, separators=(",", ":"))
        print()
    else:
        print(classify_check(args.status, args.conclusion).value)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
