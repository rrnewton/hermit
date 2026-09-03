#!/usr/bin/env bash
#
# Controls for Demo 08's crash/replay evidence and Step 3 differential.
#
# Step 3 is the only thing that turns "the bug exists" into "the fix closes it".
# Two holes let it pass without showing that, and both were demonstrated
# reaching "Demo 08: SUCCESS" with outer rc=0:
#
#   * the success path trusted the exit status and never read chaos-fixed.out,
#     so a fixed run that exited 0 while printing an ASAN use-after-free was
#     called clean;
#   * the failure path accepted ANY nonzero exit lacking the ASAN string as a
#     "non-crash", which swallowed timeout 124, safehermit's cap 125, and any
#     exec or environment failure -- none of which ran the control at all.
#
# The crash and replay must each return the ASAN abort status 134 and contain a
# completed report.  A repeatable rc=0 or timeout-truncated rc=124 fragment is
# not the promised crash and must not reach SUCCESS.
#
# These brackets drive the real demos/08-btrfs-convert-uaf.sh through its
# documented DEMO08_DIR / HERMIT_RELEASE / SAFEHERMIT seams, so they exercise
# the shipped script rather than a copy of its logic.
#
# Usage: scripts/test-demo08-fixed-control.sh   (no arguments, no side effects
# outside its own temporary directory)

set -uo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
root=$(cd -- "$script_dir/.." && pwd)
demo="$root/demos/08-btrfs-convert-uaf.sh"

case "${1:-}" in
  -h|--help)
    echo "test-demo08-fixed-control.sh — controls for Demo 08's crash and fixed differential"
    echo
    echo "USAGE:"
    echo "  scripts/test-demo08-fixed-control.sh    run every control"
    echo "  scripts/test-demo08-fixed-control.sh -h show this help and exit"
    exit 0
    ;;
  "") ;;
  *) echo "unknown argument: $1" >&2; exit 2 ;;
esac

[ -x "$demo" ] || { echo "FAIL: missing or non-executable $demo" >&2; exit 2; }

tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT

# A UAF report the demo's own grep and asan_core must accept as real.
uaf_report() {
  cat <<'REPORT'
==1234==ERROR: AddressSanitizer: heap-use-after-free on address 0x606000000210
    #0 0x4e69f6 in task_period_wait common/task-utils.c:154
    #1 0x4e7100 in print_copied_inodes convert/main.c:169
SUMMARY: AddressSanitizer: heap-use-after-free common/task-utils.c:154 in task_period_wait
REPORT
}

partial_uaf_report() {
  cat <<'REPORT'
==1234==ERROR: AddressSanitizer: heap-use-after-free on address 0x606000000210
    #0 0x4e69f6 in task_period_wait common/task-utils.c:154
REPORT
}

# The stub stands in for `safehermit <hermit> run --chaos ... -- <conv> <img>`.
# It decides what to emit from which convert binary it was handed, so a single
# stub can make the buggy run crash and the fixed run behave per-scenario.
make_stub() {
  cat >"$tmp/safehermit" <<'STUB'
#!/usr/bin/env bash
# Recover the convert binary: it is the argument after the bare `--`.
conv=""
seen=0
for a in "$@"; do
  if [ "$seen" = 1 ]; then conv="$a"; break; fi
  [ "$a" = "--" ] && seen=1
done
case "$conv" in
  *buggy*)
    count=0
    [ ! -r "$DEMO08_TEST_BUGGY_COUNT_FILE" ] || count=$(cat "$DEMO08_TEST_BUGGY_COUNT_FILE")
    count=$((count + 1))
    printf '%s\n' "$count" >"$DEMO08_TEST_BUGGY_COUNT_FILE"
    case "$DEMO08_TEST_BUGGY_MODE" in
      complete-abort) cat "$DEMO08_TEST_UAF_FILE"; exit 134 ;;
      partial-rc0) cat "$DEMO08_TEST_PARTIAL_FILE"; exit 0 ;;
      partial-rc124) cat "$DEMO08_TEST_PARTIAL_FILE"; exit 124 ;;
      replay-partial-rc0)
        if [ "$count" -eq 1 ]; then cat "$DEMO08_TEST_UAF_FILE"; exit 134; fi
        cat "$DEMO08_TEST_PARTIAL_FILE"; exit 0 ;;
      replay-partial-rc124)
        if [ "$count" -eq 1 ]; then cat "$DEMO08_TEST_UAF_FILE"; exit 134; fi
        cat "$DEMO08_TEST_PARTIAL_FILE"; exit 124 ;;
      *) echo "stub: unknown DEMO08_TEST_BUGGY_MODE" >&2; exit 9 ;;
    esac ;;
  *fixed*)
    case "$DEMO08_TEST_FIXED_MODE" in
      clean)          exit 0 ;;
      exit-zero-uaf)  cat "$DEMO08_TEST_UAF_FILE"; exit 0 ;;
      timeout)        echo "…schedule still running…"; exit 124 ;;
      wrapper-failure) echo "safehermit: bound exceeded" >&2; exit 125 ;;
      regression-uaf) cat "$DEMO08_TEST_UAF_FILE"; exit 134 ;;
      *) echo "stub: unknown DEMO08_TEST_FIXED_MODE" >&2; exit 9 ;;
    esac ;;
  *) echo "stub: unexpected convert binary: $conv" >&2; exit 9 ;;
esac
STUB
  chmod +x "$tmp/safehermit"
}

setup_assets() {
  rm -rf "$tmp/assets" "$tmp/artifacts"
  mkdir -p "$tmp/assets/buggy" "$tmp/assets/fixed" "$tmp/artifacts"
  # The demo only needs these readable/executable; the stub decides behaviour.
  printf '#!/bin/sh\nexit 0\n' >"$tmp/assets/buggy/btrfs-convert"
  printf '#!/bin/sh\nexit 0\n' >"$tmp/assets/fixed/btrfs-convert"
  chmod +x "$tmp/assets/buggy/btrfs-convert" "$tmp/assets/fixed/btrfs-convert"
  # A non-empty file standing in for the ext4 image the demo copies per run.
  head -c 4096 /dev/zero >"$tmp/assets/pop-tiny.img"
  printf '#!/bin/sh\nexit 0\n' >"$tmp/hermit"
  chmod +x "$tmp/hermit"
  uaf_report >"$tmp/uaf.txt"
  partial_uaf_report >"$tmp/partial-uaf.txt"
  rm -f "$tmp/buggy-count"
}

pass=0
fail=0

# run_case <expected-rc> <label> <fixed-mode> [required-substring] [buggy-mode]
run_case() {
  local expected=$1 label=$2 mode=$3 needle=${4:-} buggy_mode=${5:-complete-abort}
  setup_assets
  make_stub
  local out rc=0
  out=$(env \
        DEMO08_DIR="$tmp/assets" \
        DEMO08_ARTIFACTS="$tmp/artifacts" \
        DEMO08_CRASH_SEED=7 \
        DEMO08_TIMEOUT=30 \
        DEMO08_REQUIRE_ASSETS=1 \
        HERMIT_RELEASE="$tmp/hermit" \
        SAFEHERMIT="$tmp/safehermit" \
        DEMO08_TEST_FIXED_MODE="$mode" \
        DEMO08_TEST_BUGGY_MODE="$buggy_mode" \
        DEMO08_TEST_BUGGY_COUNT_FILE="$tmp/buggy-count" \
        DEMO08_TEST_UAF_FILE="$tmp/uaf.txt" \
        DEMO08_TEST_PARTIAL_FILE="$tmp/partial-uaf.txt" \
        "$demo" 2>&1) || rc=$?
  if [ "$rc" -ne "$expected" ]; then
    printf 'FAIL: %s -- expected rc=%s got rc=%s\n' "$label" "$expected" "$rc" >&2
    printf '%s\n' "$out" | tail -12 >&2
    fail=$((fail + 1))
    return
  fi
  if [ -n "$needle" ] && ! printf '%s' "$out" | grep -qF "$needle"; then
    printf 'FAIL: %s -- rc=%s correct but message did not name the reason\n' \
      "$label" "$rc" >&2
    printf '  expected to find: %s\n' "$needle" >&2
    printf '%s\n' "$out" | tail -12 >&2
    fail=$((fail + 1))
    return
  fi
  # A refusal must never also announce success.
  if [ "$expected" -ne 0 ] && printf '%s' "$out" | grep -qF 'Demo 08: SUCCESS'; then
    printf 'FAIL: %s -- refused with rc=%s but still printed SUCCESS\n' \
      "$label" "$rc" >&2
    fail=$((fail + 1))
    return
  fi
  pass=$((pass + 1))
}

# Positive control. Without this the refusals below could all be produced by a
# script that rejects everything, which would prove nothing.
run_case 0 "a fixed control that completes rc=0 with no UAF is accepted" \
  clean

# The two branches the review demonstrated reaching SUCCESS.
run_case 1 "a fixed control that exits 0 while reporting a UAF is refused" \
  exit-zero-uaf "reported a use-after-free"
run_case 1 "a fixed control cut off by the timeout is refused, not called non-crash" \
  timeout "rc=124 is the 30s timeout"
run_case 1 "a fixed control killed by a safehermit bound is refused" \
  wrapper-failure "rc=125 is a safehermit bound"

# The pre-existing regression path must still refuse.
run_case 1 "a fixed control that crashes with a UAF is still refused" \
  regression-uaf "reported a use-after-free"

# The initial crash and its replay have the same complete-report/rc=134 bar as
# calibration.  These four controls catch both former acceptance points.
run_case 1 "an rc=0 partial initial report is not called a crash" \
  clean "expected 134" partial-rc0
run_case 1 "an rc=124 partial initial report is not called a crash" \
  clean "timeout cut the run off" partial-rc124
run_case 1 "an rc=0 partial replay does not reach SUCCESS" \
  clean "replay did not complete with the ASAN abort" replay-partial-rc0
run_case 1 "an rc=124 partial replay does not reach SUCCESS" \
  clean "replay did not complete with the ASAN abort" replay-partial-rc124

printf 'test-demo08-fixed-control: %s passed, %s failed (1 positive, 8 refusals)\n' \
  "$pass" "$fail"
[ "$fail" -eq 0 ] || exit 1
