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
HB="${SAFEHERMIT_TEST_BIN:-$(dirname "$SH")/../target/debug/hermit}"
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

# T4 byte cap truncates AND records it; the child must not be SIGPIPEd.
"$SH" --sh-max-log-bytes 4096 "$t/hermit-arbitrary" --log=info run -- /bin/echo hi >/dev/null 2>"$t/4.err"
rc=$?
tr_val=$(sed -n 's/^safehermit: truncated=//p' "$t/4.err")
[ "$rc" = 0 ] && [ "$tr_val" = true -o "$tr_val" = false ] \
  && ok "truncation is RECORDED as a boolean (got '$tr_val') and the child was not SIGPIPEd" \
  || no "truncation record" "rc=$rc truncated='$tr_val' (must be true/false, never unknown)"

# T5 a genuine hang is killed by its deadline. Assert the fixture first.
timeout 8 "$t/hermit-arbitrary" run -- /bin/sh -c 'while :; do :; done' >/dev/null 2>&1
[ $? -eq 124 ] || { no "hang fixture" "fixture does not actually hang bare; test would be vacuous"; }
s=$(date +%s)
"$SH" --sh-deadline 15 "$t/hermit-arbitrary" run -- /bin/sh -c 'while :; do :; done' >/dev/null 2>"$t/5.err"
rc=$?; e=$(( $(date +%s) - s ))
[ $rc -eq 124 ] && [ $e -lt 60 ] \
  && ok "a genuine hang is killed by its cgroup deadline (${e}s, rc=124)" \
  || no "deadline" "rc=$rc after ${e}s; expected 124 well under 60s"

# T6 FAIL LOUD: with systemd unavailable the run still proceeds and SAYS so.
mkdir -p "$t/nosd"; printf '#!/bin/sh\nexit 127\n' > "$t/nosd/systemd-run"; chmod +x "$t/nosd/systemd-run"
out=$(PATH="$t/nosd:$PATH" "$SH" "$t/hermit-arbitrary" run -- /bin/echo marker-t6 2>"$t/6.err")
grep -q marker-t6 <<<"$out" && grep -q 'bound.wall=NOT_APPLIED' "$t/6.err" \
  && ok "with no systemd the run proceeds AND declares the bound NOT_APPLIED" \
  || no "fail-loud" "either the run did not proceed or the missing bound was not declared"

# T7 the binary path is MANDATORY -- no default, no resolution, no silent pick.
"$SH" run -- /bin/true >/dev/null 2>"$t/7.err"; rc=$?
[ $rc -eq 2 ] && grep -qi 'no such hermit binary\|first argument' "$t/7.err" \
  && ok "omitting the binary path is a usage error, not a silent default" \
  || no "mandatory path" "rc=$rc; a missing binary path must fail loudly"

# T8 bounded-run-space is IN THIS REPO -- no inner-to-outer dependency.
brs="$(dirname "$SH")/../scripts/bounded-run-space"
[ -x "$brs" ] && ok "scripts/bounded-run-space ships in this repo (hermit is standalone)" \
  || no "self-contained" "$brs missing -- safehermit would reach outward for its disk bound"

# T9 latesthermit must NEVER fall back to a stale binary when the build fails.
# This is the whole risk of that command: a wrapper whose NAME promises freshness
# quietly running an old binary is the most confusing possible failure.
LH="$(dirname "$SH")/latesthermit"
if [ -x "$LH" ]; then
    fake="$t/lhfake"; mkdir -p "$fake/bin" "$fake/target/release"
    cp "$LH" "$SH" "$fake/bin/"
    printf '#!/bin/sh\necho STALE-BINARY-RAN\n' > "$fake/target/release/hermit"; chmod +x "$fake/target/release/hermit"
    printf '#!/bin/sh\nexit 101\n' > "$fake/cargo"; chmod +x "$fake/cargo"
    o=$(PATH="$fake:$PATH" "$fake/bin/latesthermit" run -- /bin/true 2>&1); r=$?
    if [ $r -eq 2 ] && ! grep -q STALE-BINARY-RAN <<<"$o" && grep -q "BUILD FAILED" <<<"$o"; then
        ok "latesthermit refuses to run a stale binary when the build fails"
    else
        no "latesthermit staleness" "rc=$r; it must exit 2, say BUILD FAILED, and never run the stale binary"
    fi
fi

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
