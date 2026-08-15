#!/usr/bin/env python3
"""Bracket the tier a `--verify` run is allowed to claim.

The acceptance rule under test is narrow and one-directional: `bitwise` is
claimable ONLY from a typed matched canonical verdict with `verified=true`,
`bitwise_parity=true`, log comparison, and equal positive integer counts on
both sides. Everything else must remain a
`gap` -- never move upward.

Both sides are bracketed: each positive plants a record that MUST reach its tier,
and each negative plants a record that MUST NOT reach `bitwise`.
"""

from __future__ import annotations

import json
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from run_matrix import (  # noqa: E402
    EVIDENCE_COLUMNS,
    L2_RANK,
    SCORECARD_HEADER,
    expectation,
    verify_tier_from_json,
)

FAILURES: list[str] = []


def check(label: str, condition: bool, detail: str = "") -> None:
    if condition:
        print(f"  \033[32mok\033[0m    {label}")
    else:
        FAILURES.append(label)
        print(f"  \033[31mFAIL\033[0m  {label}" + (f" -- {detail}" if detail else ""))


def tier_of(record) -> dict[str, str] | None:
    with tempfile.TemporaryDirectory(prefix="verify-tier-") as tmp:
        path = Path(tmp) / "verdict.json"
        if record is not None:
            path.write_text(json.dumps(record), encoding="utf-8")
            return verify_tier_from_json(path)
        return verify_tier_from_json(path)


def spec(strictness="canonical", compare_logs=True, **over):
    base = {
        "strictness": strictness,
        "compare_logs": compare_logs,
        "strip_lines": False,
        "full_trace": True,
        "canonicalize_addresses": True,
        "exact_remainder": True,
    }
    base.update(over)
    return base


def record(verified=True, bitwise=True, left=239, right=239, strictness="canonical",
           verdict="matched", compare_logs=True):
    counts = None if left is None else {"left": left, "right": right}
    return {
        "verified": verified,
        "bitwise_parity": bitwise,
        "verdict": verdict,
        "comparison": spec(strictness, compare_logs),
        "compared_log_messages": counts,
        "guest_exit_code": 0,
        "guest_signal": None,
    }


# --------------------------------------------------------------------------
print("case LEGACY — a stale stripped verdict is below the current contract")
got = tier_of(record(bitwise=True, strictness="stripped"))
check("tier is 'gap', NOT 'bitwise'", got and got["tier"] == "gap", repr(got))
check("bitwise_parity records 0", got and got["bitwise_parity"] == "0", repr(got))
check("strictness is carried", got and got["verify_compare"] == "stripped", repr(got))
check("counts travel with the verdict (#319)",
      got and got["compared_log_messages"] == "239|239", repr(got))

print("case BITWISE — a genuine canonical match may claim the top tier")
got = tier_of(record(bitwise=True, strictness="canonical", left=348, right=348))
check("tier is 'bitwise'", got and got["tier"] == "bitwise", repr(got))
check("bitwise_parity records 1", got and got["bitwise_parity"] == "1", repr(got))

print("case VACUOUS — bitwise_parity with a ZERO compared count is NOT bitwise")
# Two empty selections 'match' under the strictest possible spec.  Without the
# count conjunct a run that produced no DETLOG at all would certify as parity.
for left, right, why in ((0, 0, "0|0"), (0, 239, "left 0"), (239, 0, "right 0")):
    got = tier_of(record(bitwise=True, strictness="canonical", left=left, right=right))
    check(f"zero-count record ({why}) is refused the bitwise tier",
          got and got["tier"] != "bitwise", repr(got))
    check(f"zero-count record ({why}) reports bitwise_parity 0",
          got and got["bitwise_parity"] == "0", repr(got))

print("case OUTPUT-ONLY — verified without comparing the log stream is unqualified")
got = tier_of(record(bitwise=False, compare_logs=False, left=None))
check("tier is 'gap'", got and got["tier"] == "gap", repr(got))

print("case DIVERGED — an unverified record never claims a positive tier")
got = tier_of(record(verified=False, verdict="diverged"))
check("tier is 'gap'", got and got["tier"] == "gap", repr(got))

print("case CONTRADICTIONS — no boolean pair can overrule the terminal verdict")
got = tier_of(record(verified=True, bitwise=True, verdict="diverged"))
check("verified+bitwise with diverged remains a gap",
      got and got["tier"] == "gap", repr(got))
check("contradictory diverged record reports bitwise_parity 0",
      got and got["bitwise_parity"] == "0", repr(got))
got = tier_of(record(verified=False, bitwise=True, verdict="matched"))
check("matched with verified=false remains a gap",
      got and got["tier"] == "gap", repr(got))

print("case COUNTS — only equal positive integer counts are non-vacuous")
for left, right, why in (
    (-1, -1, "negative"),
    ("239", "239", "strings"),
    (True, True, "booleans"),
    (239, 240, "unequal"),
):
    got = tier_of(record(left=left, right=right))
    check(f"{why} counts remain a gap",
          got and got["tier"] == "gap", repr(got))

print("case NO-RECORD — absent / no_result / malformed fall back, never upward")
check("absent file yields None", tier_of(None) is None)
check("no_result yields None",
      tier_of({"verdict": "no_result", "verified": False}) is None)
with tempfile.TemporaryDirectory(prefix="verify-tier-") as tmp:
    bad = Path(tmp) / "verdict.json"
    bad.write_text("not json{", encoding="utf-8")
    check("malformed JSON yields None", verify_tier_from_json(bad) is None)

print("case RANK — the ladder orders the tiers and 'bitwise' is the ceiling")
check("gap < bitwise", L2_RANK["gap"] < L2_RANK["bitwise"], repr(L2_RANK))
check("'detlog' is no longer a tier name", "detlog" not in L2_RANK, repr(L2_RANK))
check("'stripped' is no longer a tier name", "stripped" not in L2_RANK, repr(L2_RANK))

print("case CONTRACT — only supported canonical verify paths demand bitwise evidence")
check("ptrace verify contract is 'bitwise'",
      expectation("ptrace", "exit_status", True)[0] == "bitwise")
check("dbt verify contract stays a 'gap' while protected evidence is unavailable",
      expectation("dbt", "hello_stdout", True)[0] == "gap")
check("dbt nonzero exit verify contract stays a 'gap'",
      expectation("dbt", "exit_status", True)[0] == "gap")
check("kvm output-only verify contract stays a 'gap'",
      expectation("kvm", "exit_status", True)[0] == "gap")

print("case SCORECARD — only typed canonical evidence may issue a positive")
import tempfile as _tf, csv as _csv  # noqa: E402
from run_matrix import (  # noqa: E402
    BITWISE_CAPABLE_COMPARATORS, append_parent_scorecard,
)


def emitted_row(evidence):
    with _tf.TemporaryDirectory(prefix="fallback-") as tmp:
        path = Path(tmp) / "sc.csv"
        path.write_text(",".join(SCORECARD_HEADER) + "\n", encoding="utf-8")
        append_parent_scorecard(
            path,
            [{"test_name": "t", "backend": "ptrace", "expectation": "bitwise",
              "result": "PASS", "seconds": "1.0", "detail": "d", "evidence": evidence}],
            strict=True, verify=True, probe_gaps=False)
        return list(_csv.DictReader(path.open(encoding="utf-8")))[-1]


typed = emitted_row({"tier": "bitwise", "verify_compare": "canonical",
                     "bitwise_parity": "1", "compared_log_messages": "348|348"})
check("a typed verdict DOES still claim deterministic=1 (not inert)",
      typed["deterministic"] == "1", repr(typed["deterministic"]))
check("typed row carries its counts into the row",
      typed["compared_log_messages"] == "348|348", repr(typed["compared_log_messages"]))
check("canonical is the only bitwise-capable comparator",
      BITWISE_CAPABLE_COMPARATORS == ("canonical",), repr(BITWISE_CAPABLE_COMPARATORS))

print("case SCHEMA — the evidence columns exist and sit in the canonical header")
for column in EVIDENCE_COLUMNS:
    check(f"{column} is in SCORECARD_HEADER", column in SCORECARD_HEADER)
check("evidence columns are the last four",
      SCORECARD_HEADER[-4:] == EVIDENCE_COLUMNS, repr(SCORECARD_HEADER[-4:]))

print()
if FAILURES:
    print(f"FAIL ({len(FAILURES)} assertions)")
    sys.exit(1)
print("PASS")
