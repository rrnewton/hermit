#!/usr/bin/env python3
"""Bracket the tier a `--verify` run is allowed to claim.

The bug this guards: `run_matrix.py` decided the assurance kind by scraping
hermit's stderr for `"Determinism verified"` and then labelled the row
`"L2 DETLOG-bitwise"`.  That banner is printed by a plain `--verify` run whose
own `--verify-json` reports `bitwise_parity: false`, so the label asserted
bitwise identity for a comparison that had merely normalised-and-compared.
Mutation testing measured the consequence: 3 of 5 planted defects (a differing
read() return length, a differing pointer argument, a differing openat path) pass
that comparison undetected.

So the acceptance rule under test is narrow and one-directional: `bitwise` is
claimable ONLY from a typed verdict that says `bitwise_parity` AND carries a
nonzero compared-message count on both sides. A well-formed weaker match stays
`stripped` or `guest`; a non-match or malformed current record is refused --
never promoted into a plausible positive tier.

Both sides are bracketed: each positive plants a record that MUST reach its tier,
and each negative plants a record that MUST NOT reach `bitwise`.
"""

from __future__ import annotations

import json
import shlex
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from run_matrix import (  # noqa: E402
    EVIDENCE_COLUMNS,
    L2_RANK,
    DEFAULT_VERIFY_POLICY,
    SCORECARD_HEADER,
    VerifyPolicy,
    expectation,
    hermit_command,
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


def spec(strictness, compare_logs=True, **over):
    base = {
        "strictness": strictness,
        "display_name": "BitwiseInfoV1" if strictness == "canonical" else "Stripped",
        "compare_logs": compare_logs,
        "compare_io_buffers": True,
        "log_scope": "info",
        "record_envelope": "all_records_v1",
        "virtualize_time": True,
        "strip_lines": strictness == "stripped",
        "full_trace": strictness == "canonical",
        "canonicalize_addresses": strictness == "canonical",
        "exact_remainder": strictness == "canonical",
        "stripped_prefixes": [],
        "canonicalizations": [],
        "ignore_lines": False,
        "skip_commit": False,
        "skip_detlog": False,
    }
    base.update(over)
    return base


def record(verified=True, bitwise=False, left=239, right=239, strictness="stripped",
           verdict="matched", compare_logs=True):
    counts = None if left is None else {"left": left, "right": right}
    return {
        "verified": verified,
        "bitwise_parity": bitwise,
        "verdict": verdict,
        "no_result_reason": None,
        "infrastructure_error": None,
        "comparison": spec(strictness, compare_logs),
        "compared_log_messages": counts,
        "dbt_counted_branches": None,
        "runtime": None,
        "guest_exit_code": 0,
        "guest_signal": None,
        "first_divergent_scheduler_turn": None,
        "first_divergent_virtual_nanoseconds": None,
        "first_divergent_record": None,
        "first_divergent_syscall": None,
        "first_divergent_left_message": None,
        "first_divergent_right_message": None,
    }


# --------------------------------------------------------------------------
print("case STRIPPED — the exact shape the scorecard producer emits today")
# Verbatim from a live probe run: rc=0, banner ":: Success: deterministic.
# Determinism verified.", and bitwise_parity false in the same record.
got = tier_of(record(bitwise=False, strictness="stripped"))
check("tier is 'stripped', NOT 'bitwise'", got and got["tier"] == "stripped", repr(got))
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

print("case GUEST — verified without comparing the log stream is guest-visible")
got = tier_of(record(bitwise=False, compare_logs=False, left=None))
check("tier is 'guest'", got and got["tier"] == "guest", repr(got))

print("case DIVERGED — an unverified record never claims a positive tier")
got = tier_of(record(verified=False, verdict="diverged"))
check("typed divergent verdict is refused rather than assigned a positive tier",
      got is None, repr(got))

# --------------------------------------------------------------------------
# Ported from the closed hermit#2303, re-expected against THIS ladder.
#
# ⚠️ THE SCENARIOS COME FROM THAT BRANCH; THE EXPECTATIONS DO NOT.  #2303 also
# collapsed the ladder to {gap, bitwise}, so every one of these cases asserted
# `gap` there.  Here the same records must land on `stripped` -- verified, and
# the log stream WAS compared, just not canonically.  Taking that branch's
# expected values along with its scenarios would have quietly imported the
# collapse through the test file, which is why they are re-derived rather than
# copied.  Whether a plain `--verify` match reports as `stripped` or as `gap`
# changes what a published count MEANS and is an owner ruling, deliberately not
# settled here.
#
# Each case isolates ONE conjunct: every other field is set to the value that
# would certify bitwise, so a failure names the conjunct that broke.

print("case CONTRADICTION — no boolean pair overrules the terminal verdict")
got = tier_of(record(verified=True, bitwise=True, strictness="canonical",
                     verdict="diverged", left=348, right=348))
check("diverged+parity is refused the bitwise tier",
      got is None, repr(got))

print("case COMPARATOR — canonical is the only bitwise-capable comparator")
# The conflation the ladder exists to prevent: a Stripped comparison that
# nonetheless carries bitwise_parity true must NOT read as byte identity.
got = tier_of(record(bitwise=True, strictness="stripped", left=348, right=348))
check("parity under a stripped comparator is not bitwise",
      got and got["tier"] == "stripped", repr(got))
check("parity under a stripped comparator reports bitwise_parity 0",
      got and got["bitwise_parity"] == "0", repr(got))

print("case PARITY TYPE — only a real JSON true is parity")
# bool("0") and bool("false") are both True in Python, so a stringly-typed
# record certified bitwise under the previous predicate.
for value, why in (("0", 'string "0"'), ("false", 'string "false"'), (1, "int 1")):
    got = tier_of(record(bitwise=value, strictness="canonical", left=348, right=348))
    check(f"bitwise_parity as {why} is refused by the typed reader",
          got is None, repr(got))

print("case COUNTS — only equal positive integer counts are non-vacuous")
# `type(x) is int` and not isinstance: bool subclasses int, so true|true would
# otherwise read as a count.
for left, right, why in (
    (-1, -1, "negative"),
    ("239", "239", "strings"),
    (True, True, "booleans"),
    (239, 240, "unequal"),
):
    got = tier_of(record(bitwise=True, strictness="canonical", left=left, right=right))
    typed_shape_is_invalid = (
        type(left) is not int or type(right) is not int or left < 0 or right < 0
    )
    check(f"{why} counts are refused the bitwise tier",
          got is None if typed_shape_is_invalid else got and got["tier"] != "bitwise",
          repr(got))

print("case NAMED REFUSAL — a rejection nobody can see is not a check")
# ⚠️ THE TIER ALONE CANNOT CARRY THIS. Degrading a self-contradictory record to
# `guest` files it under the same label as an honest output-only run: same row,
# different facts. So a record that CLAIMS bitwise_parity and is refused must say
# which conjunct refused it.
#
# The shape is the KVM comparator reporting `matched` while `compare_logs` was
# false -- a verdict disagreeing with its own evidence.


def refusal_for(rec) -> str:
    import contextlib
    import io

    buf = io.StringIO()
    with contextlib.redirect_stderr(buf):
        tier_of(rec)
    return buf.getvalue()


msg = refusal_for(record(bitwise=True, strictness="canonical", compare_logs=False,
                         left=348, right=348))
check("a parity claim over an uncompared log stream is REFUSED BY NAME",
      "REFUSED the bitwise tier" in msg, repr(msg))
check("the refusal names compare_logs as the conjunct that refused it",
      "compare_logs" in msg and "not compared" in msg, repr(msg))

msg = refusal_for(record(bitwise=True, strictness="stripped", left=348, right=348))
check("a parity claim under a stripped comparator names the comparator",
      "not canonical" in msg, repr(msg))

msg = refusal_for(record(bitwise=True, strictness="canonical", verdict="diverged",
                         left=348, right=348))
check("a parity claim on a diverged verdict names the verdict",
      "verdict is diverged" in msg, repr(msg))

msg = refusal_for(record(bitwise=True, strictness="canonical", left=239, right=240))
check("a parity claim with unequal counts names the counts",
      "equal positive integers" in msg, repr(msg))

# The other half of the contract, and the reason this is not just noise.
msg = refusal_for(record(bitwise=False, strictness="stripped", left=239, right=239))
check("an ordinary stripped run claiming nothing stays SILENT",
      msg == "", repr(msg))
msg = refusal_for(record(bitwise=True, strictness="canonical", left=348, right=348))
check("a record that EARNS the tier stays silent",
      msg == "", repr(msg))

print("case REAL RECORD — the shape a genuine measured L2 run actually produced")
# ⚠️ THE CONTROL FOR EVERY NEGATIVE ABOVE.  A predicate that rejects everything
# is not a stricter predicate, it is a broken one, so the tightening has to be
# shown NOT to demote real evidence.
#
# These are the exact values `ci/compat-envelope/cells.json` records for a ptrace
# measurement at hermit e69c0a62cecef9aa44e3810ae88c06ad24155048 (debug build):
# "verified=true, verdict=matched, bitwise_parity=true, strictness=canonical,
# compare_logs=true, record_envelope=all_records_v1, 266/266 INFO messages
# compared, exit 0".
#
# That the manifest already DECLARES this conjunction is the reason the port is a
# correction rather than a new policy: the standard was written down, the code
# simply was not enforcing it.
got = tier_of(record(verified=True, bitwise=True, verdict="matched",
                     strictness="canonical", left=266, right=266))
check("the measured ptrace record still certifies bitwise",
      got and got["tier"] == "bitwise", repr(got))
check("the measured ptrace record reports bitwise_parity 1",
      got and got["bitwise_parity"] == "1", repr(got))
check("the measured ptrace record carries its 266/266 counts",
      got and got["compared_log_messages"] == "266|266", repr(got))

print("case NO_RESULT INTERACTION — the typed reasons and these conjuncts must agree")
# ⚠️ TWO CONSTRAINTS ON VERDICT SHAPE LANDED SEPARATELY, so they are pinned
# together here: if they ever disagree, one starts refusing the other silently.
#
# `NoResultReason` distinguishes `not_run` -- stamped BEFORE verification, so its
# survival means the invocation died before reaching a comparison -- from
# `first_run_rejected`, where the guest ran and its exit status was rejected.
# Those need opposite responses, which is why the untyped `no_result` was not
# enough.
#
# These conjuncts sit strictly DOWNSTREAM of the `no_result` gate, so they can
# never reclassify either reason. Verified in both directions rather than assumed.
for reason, why in (
    ({"kind": "not_run"}, "not_run"),
    ({"kind": "first_run_rejected", "exit_code": 1, "signal": None,
      "stdout_bytes": 0, "stderr_bytes": 0}, "first_run_rejected"),
):
    no_result = record(verified=False, bitwise=False, verdict="no_result")
    no_result["no_result_reason"] = reason
    no_result["comparison"] = None
    no_result["compared_log_messages"] = None
    got = tier_of(no_result)
    check(f"a typed no_result ({why}) still yields None, untouched by these conjuncts",
          got is None, repr(got))

# The positive interaction, and the reason this pairing is worth having: the
# pre-stamped record is `verdict=no_result` with every field null. A path that
# flips the verdict without filling the comparison would previously have carried
# `bitwise_parity: true` straight to the top tier.
partial = record(verified=True, bitwise=True, verdict="matched")
partial["no_result_reason"] = {"kind": "not_run"}
partial["comparison"] = None
partial["compared_log_messages"] = None
got = tier_of(partial)
check("a partially updated pre-stamp claiming parity is refused",
      got is None, repr(got))
check("and it is refused BY NAME, not silently",
      "comparison is null but verdict is matched" in refusal_for(partial), "silent")

print("case LADDER — the rungs this change deliberately did NOT touch")
# Pinned so a future collapse to {gap, bitwise} is a visible test failure rather
# than a silent redefinition of every published count.  See hermit#2303.
check("'stripped' is still a rung", "stripped" in L2_RANK, repr(L2_RANK))
check("'guest' is still a rung", "guest" in L2_RANK, repr(L2_RANK))
check("the ladder still has four rungs", len(L2_RANK) == 4, repr(L2_RANK))

print("case NO-RECORD — absent / no_result / malformed fall back, never upward")
check("absent file yields None", tier_of(None) is None)
check("no_result yields None",
      tier_of({"verdict": "no_result", "verified": False}) is None)
with tempfile.TemporaryDirectory(prefix="verify-tier-") as tmp:
    bad = Path(tmp) / "verdict.json"
    bad.write_text("not json{", encoding="utf-8")
    check("malformed JSON yields None", verify_tier_from_json(bad) is None)

print("case RANK — the ladder orders the tiers and 'bitwise' is the ceiling")
check("guest < stripped < bitwise",
      L2_RANK["guest"] < L2_RANK["stripped"] < L2_RANK["bitwise"], repr(L2_RANK))
check("'detlog' is no longer a tier name", "detlog" not in L2_RANK, repr(L2_RANK))

print("case MODE — the summary and Hermit flags come from one comparison policy")
stripped_summary = DEFAULT_VERIFY_POLICY.mode_summary()
mode_host_tmp = Path("/host-tmp")
stripped_command = hermit_command(
    Path("/hermit"), "ptrace", ["/bin/true"], "hello_stdout", True,
    host_tmp=mode_host_tmp,
    verify=True,
)
expected_prefix = ["/hermit", "run", *DEFAULT_VERIFY_POLICY.displayed_flags()]
check("the policy supplies the exact Hermit command prefix",
      stripped_command[:len(expected_prefix)] == expected_prefix,
      repr(stripped_command))
check("the command uses the supplied host temporary directory",
      f"--tmp={mode_host_tmp}" in stripped_command,
      repr(stripped_command))
check("the summary prints the exact requested flags",
      shlex.join(DEFAULT_VERIFY_POLICY.displayed_flags()) in stripped_summary,
      stripped_summary)
check("Stripped summary names the lossy policy", "Stripped" in stripped_summary,
      stripped_summary)
check("Stripped summary does NOT claim byte identity",
      "byte-identical" not in stripped_summary, stripped_summary)
check("Stripped summary does NOT label the mode L2",
      "MODE: L2" not in stripped_summary and "below L2" in stripped_summary,
      stripped_summary)
check("Stripped command does NOT request --verify-strict",
      "--verify-strict" not in stripped_command, repr(stripped_command))
check("the default policy preserves the exact command flags",
      DEFAULT_VERIFY_POLICY.hermit_flags ==
      ("--verify", "--verify-allow", "both"),
      repr(DEFAULT_VERIFY_POLICY.hermit_flags))

canonical_policy = VerifyPolicy.checked(
    hermit_flags=("--verify", "--verify-strict", "--verify-allow", "both"),
    expected_non_kvm_tier="bitwise",
    comparison_claim="canonical BitwiseInfoV1 INFO comparison",
)
canonical_summary = canonical_policy.mode_summary()
check("a genuinely canonical policy still claims L2",
      canonical_policy.assurance_label() == "L2" and "L2" in canonical_summary,
      canonical_summary)
check("the canonical bracket requests --verify-strict",
      "--verify-strict" in canonical_policy.hermit_flags,
      repr(canonical_policy.hermit_flags))
for name, flags, tier in (
    ("bitwise without --verify-strict",
     ("--verify", "--verify-allow", "both"), "bitwise"),
    ("--verify-strict without bitwise",
     ("--verify", "--verify-strict", "--verify-allow", "both"), "stripped"),
    ("missing exit-status handling", ("--verify",), "stripped"),
):
    try:
        VerifyPolicy.checked(flags, tier, "invalid test policy")
    except ValueError:
        refused = True
    else:
        refused = False
    check(f"policy refuses {name}", refused)

check_only = subprocess.run(
    [sys.executable, str(Path(__file__).with_name("run_matrix.py")),
     "--check", "--verify", "--backend", "ptrace"],
    capture_output=True,
    text=True,
    check=False,
)
check("the public check-only path succeeds", check_only.returncode == 0,
      check_only.stderr)
check("the public output has no false L2 mode or ratchet label",
      "MODE: L2" not in check_only.stdout and "RATCHET-L2" not in check_only.stdout,
      check_only.stdout)
check("the public ratchet names the actual --verify mode",
      "RATCHET --verify ptrace:" in check_only.stdout, check_only.stdout)

print("case CONTRACT — today's contracts demand 'stripped', not 'bitwise'")
# Asserting bitwise before an INFO-tier comparator exists would red every
# ptrace/DBT cell for a comparator limitation, not a guest defect.
check("ptrace verify contract is 'stripped'",
      expectation("ptrace", "exit_status", True)[0] == "stripped")
# `exit_status` is a declared dbt L2 gap, so it would report "gap" regardless of
# tiering; use a case dbt is actually contracted for.
check("dbt verify contract is 'stripped'",
      expectation("dbt", "hello_stdout", True)[0] == "stripped")
check("a declared dbt L2 gap still reports 'gap'",
      expectation("dbt", "exit_status", True)[0] == "gap")
check("kvm verify contract stays 'guest'",
      expectation("kvm", "exit_status", True)[0] == "guest")

print("case SCORECARD — a typed verdict is the only source of a determinism positive")
import tempfile as _tf, csv as _csv  # noqa: E402
from run_matrix import (  # noqa: E402
    append_parent_scorecard,
)


def emitted_row(evidence, *, backend="dbt", detail="d"):
    with _tf.TemporaryDirectory(prefix="fallback-") as tmp:
        path = Path(tmp) / "sc.csv"
        path.write_text(",".join(SCORECARD_HEADER) + "\n", encoding="utf-8")
        append_parent_scorecard(
            path,
            [{"test_name": "t", "backend": backend, "expectation": "stripped",
              "result": "PASS", "seconds": "1.0", "detail": detail,
              "evidence": evidence}],
            strict=True, verify=True, probe_gaps=False)
        return list(_csv.DictReader(path.open(encoding="utf-8")))[-1]


typed = emitted_row({"tier": "bitwise", "verify_compare": "canonical",
                     "bitwise_parity": "1", "compared_log_messages": "348|348"})
check("a typed verdict DOES still claim deterministic=1 (not inert)",
      typed["deterministic"] == "1", repr(typed["deterministic"]))
check("typed row carries its counts into the row",
      typed["compared_log_messages"] == "348|348", repr(typed["compared_log_messages"]))

guest = emitted_row(
    {"tier": "guest", "verify_compare": "output_only",
     "bitwise_parity": "0", "compared_log_messages": ""},
    backend="kvm", detail="output+exit matched",
)
check("typed KVM evidence is not mislabeled L2",
      guest["reason"].startswith("Guest-visible verification only") and
      not guest["reason"].startswith("L2"), repr(guest["reason"]))

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
