#!/usr/bin/env bash
# Acceptance tests for bin/safehermit.
#
# WHY THE NEGATIVE TESTS MATTER MORE THAN THE POSITIVE ONES. On 2026-08-19 an
# agent's acceptance test passed against its own unfixed build, and separately
# several bounds were present and inert all day. So this asserts the FAILURE
# paths explicitly: that a missing bound is REPORTED, and that a hang is actually
# killed. Two real defects in safehermit were caught by these tests and not by
# reading the script:
#
#   * `systemctl --user is-system-running` returns "degraded" here while
#     `systemd-run --user` works, so gating on it silently dropped the wall and
#     memory bounds. T3 fails if the wall bound is not APPLIED on a box that
#     supports it.
#   * `2> >(capper …)` is not waited on, so `truncated=` read back as `unknown`
#     -- the truncation record, which is the whole point of the byte cap, was
#     missing on every run. T4 asserts it is a real boolean.
#
# A NOTE ON THE HANG FIXTURE. `sleep 300` is NOT a hang under hermit: hermit
# virtualizes time, so it returns almost immediately and an earlier version of T5
# passed in 1 second while testing nothing. The fixture must burn real CPU, and
# the test asserts the fixture genuinely hangs bare before trusting the result.
set -uo pipefail
SH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/safehermit"
HB="${SAFEHERMIT_TEST_BIN:-$(dirname "$SH")/hermit}"
pass=0; fail=0
ok(){ printf '  PASS  %s\n' "$1"; pass=$((pass+1)); }
no(){ printf '  FAIL  %s -- %s\n' "$1" "$2"; fail=$((fail+1)); }
[ -x "$HB" ] || { echo "no hermit binary at $HB (set SAFEHERMIT_TEST_BIN)"; exit 2; }

t=$(mktemp -d); trap 'rm -rf "$t"' EXIT

# T1 arbitrary binary at an ABSOLUTE PATH -- the case that caused the incident.
cp "$HB" "$t/hermit-arbitrary"; chmod +x "$t/hermit-arbitrary"
out=$("$SH" "$t/hermit-arbitrary" run -- /bin/echo marker-t1 2>"$t/1.err")
grep -q marker-t1 <<<"$out" && grep -q "binary=$t/hermit-arbitrary" "$t/1.err" \
  && ok "arbitrary binary by absolute path is honoured and named" \
  || no "arbitrary binary" "stdout or binary= field wrong"

# T2 exit code passes through verbatim.
"$SH" "$t/hermit-arbitrary" run -- /bin/sh -c 'exit 42' >/dev/null 2>/dev/null
[ $? -eq 42 ] && ok "non-zero child exit code passes through verbatim" || no "exit code" "not 42"

# T3 every bound is reported, and the wall bound is APPLIED where supported.
"$SH" "$t/hermit-arbitrary" run -- /bin/true >/dev/null 2>"$t/3.err"
for b in bound.wall bound.cgroup bound.disk bound.bytes bound.logfilter; do
    grep -q "$b=" "$t/3.err" || no "bounds reported" "$b missing"
done
grep -qE 'bound\.wall=(APPLIED|NOT_APPLIED):' "$t/3.err" && ok "every bound states APPLIED or NOT_APPLIED with a reason" || no "bounds" "wall not stated"
if systemd-run --user --unit=sht-probe-$$ --collect --quiet --property=RuntimeMaxSec=10 --pipe --wait -- /bin/true >/dev/null 2>&1; then
    grep -q 'bound.wall=APPLIED' "$t/3.err" && ok "wall bound APPLIED on a box that supports systemd --user" \
      || no "wall bound" "systemd --user works here but the bound was declared unavailable"
fi

# T4 the byte cap FIRES under a deliberately tiny threshold and records
# `truncated=true`. A boolean-only assertion passed when the old 4096-byte
# fixture never reached the cap, so it proved only that the metadata parser ran.
#
# Keep the child semantics explicit too. With a cgroup, the wrapper deliberately
# kills the run and returns 125. Without one, the cap must keep draining stderr
# rather than closing the pipe: the child reaches its own exit 42, not SIGPIPE
# (128+signal 13 = 141), while the retained log is still truncated.
cat > "$t/stderr-flood" <<'PY'
#!/usr/bin/env python3
import os
import sys
import time

for _ in range(128):
    os.write(2, b"x" * 1023 + b"\n")
time.sleep(2)
sys.exit(42)
PY
chmod +x "$t/stderr-flood"
"$SH" --sh-max-log-bytes 64 "$t/stderr-flood" >/dev/null 2>"$t/4.err"
rc=$?
tr_val=$(sed -n 's/^safehermit: truncated=//p' "$t/4.err")
killed_val=$(sed -n 's/^safehermit: killed_at_cap=//p' "$t/4.err")
if [ "$tr_val" != true ]; then
    no "byte cap" "rc=$rc truncated='$tr_val'; the reduced 64-byte threshold did not fire"
elif [ "$killed_val" = cgroup-killed ] && [ "$rc" -eq 125 ]; then
    ok "byte cap records truncated=true and returns the wrapper's cgroup-kill code 125"
elif [ "$killed_val" = no-cgroup-available ] && [ "$rc" -eq 42 ]; then
    ok "byte cap records truncated=true and drains stderr without SIGPIPE (child exit 42, not 141)"
else
    no "byte cap semantics" "rc=$rc truncated='$tr_val' killed_at_cap='$killed_val'; expected cgroup-killed/125 or no-cgroup-available/42, never SIGPIPE/141"
fi

# Force the degraded arm even on a host with systemd. This independently proves
# that stopping writes to the retained log does not close the child's stderr.
mkdir -p "$t/nosd"; printf '#!/bin/sh\nexit 127\n' > "$t/nosd/systemd-run"; chmod +x "$t/nosd/systemd-run"
PATH="$t/nosd:$PATH" "$SH" --sh-max-log-bytes 64 "$t/stderr-flood" >/dev/null 2>"$t/4-degraded.err"
rc=$?
tr_val=$(sed -n 's/^safehermit: truncated=//p' "$t/4-degraded.err")
killed_val=$(sed -n 's/^safehermit: killed_at_cap=//p' "$t/4-degraded.err")
if [ "$tr_val" = true ] && [ "$killed_val" = no-cgroup-available ] && [ "$rc" -eq 42 ]; then
    ok "degraded byte cap truncates, keeps draining, and preserves child exit 42 rather than SIGPIPE 141"
else
    no "degraded byte cap" "rc=$rc truncated='$tr_val' killed_at_cap='$killed_val'; expected true/no-cgroup-available/42"
fi

# T5 an ad-hoc Hermit binary copied under /tmp is bounded by the wrapper while
# the exact same invocation is not self-bounded when run bare. The outer
# eight-second `timeout` is test containment, not the property being credited to
# the bare run: exit 124 proves the fixture was still running when containment
# stopped it.
timeout 8 "$t/hermit-arbitrary" run -- /bin/sh -c 'while :; do :; done' >/dev/null 2>&1
[ $? -eq 124 ] || { no "hang fixture" "fixture does not actually hang bare; test would be vacuous"; }
s=$(date +%s)
"$SH" --sh-deadline 15 "$t/hermit-arbitrary" run -- /bin/sh -c 'while :; do :; done' >/dev/null 2>"$t/5.err"
rc=$?; e=$(( $(date +%s) - s ))
[ $rc -eq 124 ] && [ $e -lt 60 ] \
  && ok "a genuine hang is killed by its cgroup deadline (${e}s, rc=124)" \
  || no "deadline" "rc=$rc after ${e}s; expected 124 well under 60s"

# T6 FAIL LOUD: with systemd unavailable the run still proceeds and SAYS so.
# The byte limit still truncates retained evidence, but cannot kill the child;
# its bound line must not call that degraded behaviour lethal.
out=$(PATH="$t/nosd:$PATH" "$SH" "$t/hermit-arbitrary" run -- /bin/echo marker-t6 2>"$t/6.err")
grep -q marker-t6 <<<"$out" && grep -q 'bound.wall=NOT_APPLIED' "$t/6.err" \
  && ok "with no systemd the run proceeds AND declares the bound NOT_APPLIED" \
  || no "fail-loud" "either the run did not proceed or the missing bound was not declared"
grep -q 'bound.bytes=APPLIED:.*(TRUNCATION ONLY:' "$t/6.err" \
  && ! grep -q 'bound.bytes=.*(LETHAL:' "$t/6.err" \
  && ok "without a cgroup the byte bound reports truncation only, not a lethal cap" \
  || no "degraded byte report" "bound.bytes called a truncation-only cap lethal or did not explain the degraded behaviour"

# T7 THE CALLER'S ENVIRONMENT ACTUALLY REACHES HERMIT.
# This is a regression test for a defect the first six tests all passed over.
# `systemd-run --user` does NOT inherit the invoking shell's environment -- it
# starts the unit from the user manager's own -- so every caller variable was being
# discarded, while the report claimed the caller's RUST_LOG was "passed through
# untouched". Measured at the time: a guest whose stderr is 46,868,869 bytes in 8.4s
# when hermit runs directly produced 0 bytes through the wrapper, and the run was
# reported bytes_written=0 truncated=false exit_code=0 -- silent, clean-looking, and
# entirely wrong.
#
# ASSERT THE FIXTURE FIRST, exactly as T5 does. If RUST_LOG made no difference to
# this hermit build the test would pass while measuring nothing. Measured while
# writing this: RUST_LOG=info over /bin/echo emits 0 bytes, so a fixture chosen for
# convenience would have been silently vacuous. The filter below is the one demos 05
# and 06 actually set, over a guest that makes enough syscalls to show it: 207,067
# bytes loud against 52 bytes quiet on this box.
T7_FILTER='warn,detcore=info,reverie_ptrace::task=info'
T7_GUEST=(/bin/sh -c 'i=0; while [ $i -lt 300 ]; do echo $i; i=$((i+1)); done')
RUST_LOG=error      "$HB" run -- "${T7_GUEST[@]}" >/dev/null 2>"$t/7.bare-quiet"
RUST_LOG="$T7_FILTER" "$HB" run -- "${T7_GUEST[@]}" >/dev/null 2>"$t/7.bare-loud"
q=$(stat -c%s "$t/7.bare-quiet"); l=$(stat -c%s "$t/7.bare-loud")
if [ "$l" -lt 10000 ] || [ "$l" -le $(( q * 10 )) ]; then
    no "env fixture" "the demo log filter ($l bytes) is not both substantial and an order of magnitude louder than RUST_LOG=error ($q bytes) bare; T7 would be vacuous"
else
    RUST_LOG="$T7_FILTER" "$SH" --sh-report "$t/7.report" "$t/hermit-arbitrary" run -- "${T7_GUEST[@]}" \
        >/dev/null 2>"$t/7.err"
    w=$(stat -c%s "$t/7.err")
    # COMPARE AGAINST THE LOUD BASELINE, NOT THE QUIET ONE. An earlier version of
    # this line asserted w > q*10, and it PASSED against the unfixed script: the
    # quiet baseline measured 0 bytes, so q*10 was 0 and the 1,370 bytes an
    # environment-stripped run still emits cleared it easily, against 207,067 for a
    # bare loud run. A threshold anchored to zero is not a threshold. The wrapped
    # run must be within the same order of magnitude as a bare LOUD run.
    [ "$w" -gt $(( l / 2 )) ] \
      && ok "the caller's RUST_LOG reaches hermit through the transient unit ($w bytes wrapped vs $q quiet, $l bare loud)" \
      || no "env forwarding" "wrapped run emitted $w bytes against $l for a bare loud run (quiet baseline $q) -- the caller's environment is being dropped"
    grep -q 'env.forwarded=APPLIED' "$t/7.report" \
      && ok "environment forwarding is REPORTED, not assumed" \
      || no "env reported" "no env.forwarded=APPLIED line in the report"
fi

# T8 --sh-report keeps this script's own lines OUT of the child's stderr.
# Not cosmetic: demos/05-qemu-boot.py hashes its hermit log and demo_common
# .compare_runs line-diffs two runs of it, and six report lines embed run_id (a UTC
# timestamp plus pid). Verified against the real comparator -- hermit_log_diff on two
# otherwise-identical logs reported "first divergence at line 1 ... run_id" -- so
# without this the demo fails repeat verification on every run.
"$SH" --sh-report "$t/8.report" "$t/hermit-arbitrary" run -- /bin/echo marker-t8 \
    >"$t/8.out" 2>"$t/8.err"
# `grep -c` prints 0 AND exits 1 on no-match, so a `|| echo 0` fallback appends a
# SECOND zero and every later [ ] test dies with "integer expression expected".
# Count without the fallback and let the printed 0 stand.
n_err=$(grep -c '^safehermit: ' "$t/8.err"); n_rep=$(grep -c '^safehermit: ' "$t/8.report")
grep -q marker-t8 "$t/8.out" && [ "$n_err" -eq 0 ] && [ "$n_rep" -gt 0 ] \
  && ok "--sh-report moves the report off the child's stderr ($n_rep lines to the file, $n_err left on stderr)" \
  || no "--sh-report" "stdout marker missing, or $n_err report lines still on stderr, or report file empty ($n_rep)"

# T9 the report defaults to stderr -- the contract other callers already rely on.
"$SH" "$t/hermit-arbitrary" run -- /bin/echo marker-t9 >/dev/null 2>"$t/9.err"
[ "$(grep -c '^safehermit: ' "$t/9.err")" -gt 0 ] \
  && ok "without --sh-report the report still goes to stderr (default unchanged)" \
  || no "report default" "report vanished from stderr when no --sh-report was given"

# ---------------------------------------------------------------------------
# T10-T12 THE DEADLINE VERDICT COMES FROM SYSTEMD, NOT FROM GUESSWORK.
# These use plain fixtures rather than hermit: what is under test is how the wrapper
# CLASSIFIES an outcome, and a fixture can produce an exact outcome on demand where
# hermit cannot. T5 already covers a real hermit hang.
# ---------------------------------------------------------------------------
printf '#!/bin/bash\ntrap "exit 0" TERM\nsleep 600 & wait $!\n' > "$t/exit0-on-term"
printf '#!/bin/bash\nsleep 0.8; exit 1\n' > "$t/quickfail"
chmod +x "$t/exit0-on-term" "$t/quickfail"

# T10 A MISSED TIMEOUT. systemd accepts a duration suffix and enforces it, but the
# old code compared `elapsed` against the raw string with bash integer arithmetic:
# `[ 6 -ge 6s ]` aborts with "integer expression expected", the condition went false,
# and a run that WAS killed reported exit 1 with no DEADLINE line. Exit 1 is hermit's
# own convention for a failed guest, so the kill was indistinguishable from ordinary
# failure. Measured before the fix: exit 1 and 0 DEADLINE lines.
s=$(date +%s)
"$SH" --sh-deadline 6s --sh-report "$t/10.report" "$t/exit0-on-term" >/dev/null 2>"$t/10.err"
rc=$?; e=$(( $(date +%s) - s ))
# Assert the fixture was really killed before trusting the verdict, as T5 does: a
# 600s child that returns in under 30s did not finish on its own.
if [ "$e" -ge 30 ]; then
    no "suffixed-deadline fixture" "child ran ${e}s; it was not killed, so the test is vacuous"
else
    [ $rc -eq 124 ] && [ "$(grep -c '^safehermit: DEADLINE' "$t/10.report")" -gt 0 ] \
      && [ "$(grep -c 'integer expression expected' "$t/10.err")" -eq 0 ] \
      && ok "a suffixed deadline (--sh-deadline 6s) is still reported as a timeout (rc=$rc after ${e}s)" \
      || no "suffixed deadline" "rc=$rc after ${e}s with $(grep -c '^safehermit: DEADLINE' "$t/10.report") DEADLINE lines; the run was killed but not reported as a timeout"
fi

# T11 A FALSE TIMEOUT, the inverse error. The wrapper's own startup is counted in
# `elapsed` -- measured at ~0.27s, dominated by the systemd capability probe -- so a
# child that exits 1 well INSIDE its deadline could still push elapsed to the deadline
# and be relabelled 124. Measured before the fix: this exact fixture returned 124 with
# a DEADLINE line while systemd reported Result=exit-code, i.e. never killed. A real
# guest failure reported as a hang sends someone chasing a hang that never happened.
"$SH" --sh-deadline 1 --sh-report "$t/11.report" "$t/quickfail" >/dev/null 2>/dev/null
rc=$?
res=$(sed -n 's/^safehermit: unit_result=//p' "$t/11.report")
if [ "$res" = timeout ]; then
    no "false-timeout fixture" "systemd says this run really did time out, so the test cannot distinguish the two errors"
else
    [ $rc -eq 1 ] && [ "$(grep -c '^safehermit: DEADLINE' "$t/11.report")" -eq 0 ] \
      && ok "a run that fails on its own is NOT relabelled a timeout (rc=$rc, unit_result=$res)" \
      || no "false timeout" "rc=$rc with unit_result=$res and $(grep -c '^safehermit: DEADLINE' "$t/11.report") DEADLINE lines; an ordinary failure was reported as a deadline"
fi

# T12 THE VERDICT IS READ, NOT DEFAULTED. This is the --collect trap. `systemctl show`
# on a unit that no longer exists does not fail and does not return empty -- it returns
# the DEFAULT, Result=success. Measured three trials each on the same timed-out unit:
# with --collect it reads 'success', without it reads 'timeout'. So if --collect is
# ever added back, the wrapper would call every timed-out run a clean success. This
# asserts the value actually observed on a killed run, which no default can satisfy.
s=$(date +%s)
"$SH" --sh-deadline 5 --sh-report "$t/12.report" "$t/exit0-on-term" >/dev/null 2>/dev/null
e=$(( $(date +%s) - s ))
res=$(sed -n 's/^safehermit: unit_result=//p' "$t/12.report")
if [ "$e" -ge 30 ]; then
    no "unit-result fixture" "child ran ${e}s; it was not killed, so the test is vacuous"
else
    [ "$res" = timeout ] \
      && ok "the deadline verdict is systemd's own unit result (unit_result=$res), not a default" \
      || no "unit result" "unit_result='$res' on a run that was killed at its deadline; 'success' means --collect is back and the unit was garbage-collected before it could be read, 'UNREADABLE' means the query failed"
fi

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
