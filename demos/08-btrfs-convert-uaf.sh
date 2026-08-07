#!/usr/bin/env bash
# Demo 08: schedule-dependent btrfs-convert progress-thread use-after-free.
#
# btrfs-progs' btrfs-convert runs a background "progress" subthread that
# dereferences a shared `struct task_info *info` while the main thread copies
# inodes. Before upstream commit 73e211a7, task_start() pthread_detach()ed that
# subthread and task_stop() never pthread_join()ed it, so task_deinit() could
# free(info) while the subthread was still reading it -- a use-after-free whose
# occurrence depends entirely on the teardown interleaving. It is therefore
# essentially invisible to blind/native execution, but hermit's chaos scheduler
# lands the racing schedule deterministically on specific seeds and replays it
# bit-for-bit.
#
# This demo runs prebuilt AddressSanitizer binaries (so the latent UAF becomes
# an observable abort) of two btrfs-convert variants -- `buggy` (pre-73e211a7)
# and `fixed` (73e211a7). It shows: native buggy is clean, chaos buggy crashes
# on a known seed, chaos fixed on the same seed is clean (the differential), and
# the chaos crash reproduces byte-for-byte on replay. See the companion
# 08-btrfs-convert-uaf.md for the bug, the build recipe, and the observability
# adaptation the ASAN binaries carry.

set -euo pipefail

DEMO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$DEMO_DIR/.." && pwd)"
ASSETS="${DEMO08_DIR:-$ROOT/ignored/demo08-btrfs}"
ARTIFACTS="${DEMO08_ARTIFACTS:-$ROOT/ignored/demo08-run}"

usage() {
  cat <<'EOF'
Usage: demos/08-btrfs-convert-uaf.sh

Demonstrate a schedule-dependent btrfs-convert progress-thread use-after-free
that native execution misses and hermit --chaos finds and replays.

Requires prebuilt ASAN btrfs-convert binaries and a populated ext4 image under
the ignored asset directory (see 08-btrfs-convert-uaf.md for the build recipe):
  ignored/demo08-btrfs/buggy/btrfs-convert
  ignored/demo08-btrfs/fixed/btrfs-convert
  ignored/demo08-btrfs/pop-tiny.img
When those assets are absent the demo prints SKIPPED and exits 0.

Useful overrides:
  DEMO08_DIR=/path        asset directory (buggy/, fixed/, pop-tiny.img)
  DEMO08_ARTIFACTS=/path  per-run scratch + saved ASAN report
  DEMO08_CRASH_SEED=N     override the fixture's recorded crashing seed
  DEMO08_TIMEOUT=90       per-run wall-clock timeout in seconds
  HERMIT_RELEASE=/path    release Hermit binary
  DEMO08_REQUIRE_ASSETS=1 fail instead of skipping when assets are absent
EOF
}

case "${1:-}" in
  "") ;;
  -h|--help) usage; exit 0 ;;
  *) usage >&2; exit 2 ;;
esac

BUGGY="$ASSETS/buggy/btrfs-convert"
FIXED="$ASSETS/fixed/btrfs-convert"
IMAGE="$ASSETS/pop-tiny.img"
REQUIRE_ASSETS="${DEMO08_REQUIRE_ASSETS:-0}"
if [ "$REQUIRE_ASSETS" != 0 ] && [ "$REQUIRE_ASSETS" != 1 ]; then
  echo "error: DEMO08_REQUIRE_ASSETS must be 0 or 1" >&2
  exit 2
fi

# Gate: the ASAN binaries and populated image are large and host-specific, so
# they live in the ignored asset directory rather than the repository. Skip
# cleanly (exit 0) when they are absent so run-all.sh stays green on hosts that
# have not built them.
for f in "$BUGGY" "$FIXED" "$IMAGE"; do
  if [ ! -r "$f" ]; then
    if [ "$REQUIRE_ASSETS" = 1 ]; then
      echo "=== Demo 08: FAILURE — required asset is missing: $f ===" >&2
    else
      echo "=== Demo 08: SKIPPED — missing asset: $f ==="
    fi
    echo "Build the ASAN btrfs-convert variants and image first; see"
    echo "  demos/08-btrfs-convert-uaf.md (Build recipe)."
    [ "$REQUIRE_ASSETS" = 0 ] && exit 0
    exit 1
  fi
done

HERMIT_RELEASE="${HERMIT_RELEASE:-$ROOT/hermit/target/release/hermit}"
if [ ! -x "$HERMIT_RELEASE" ]; then
  make -C "$ROOT" --no-print-directory -s build-hermit
fi
if [ ! -x "$HERMIT_RELEASE" ]; then
  echo "error: missing Hermit release binary: $HERMIT_RELEASE" >&2
  exit 1
fi

if [ -n "${DEMO08_CRASH_SEED:-}" ]; then
  CRASH_SEED="$DEMO08_CRASH_SEED"
elif [ -r "$ASSETS/.crash-seed" ]; then
  # Format is `<seed> <fixture-sha256>`: a crashing schedule is a property of the exact
  # fixture binary, so the seed is refused when it was calibrated against a different one
  # rather than silently used (see scripts/prepare-demo08-assets.sh and issue #1877).
  CRASH_SEED="$(cut -d' ' -f1 <"$ASSETS/.crash-seed")"
  SEED_FIXTURE="$(cut -s -d' ' -f2 <"$ASSETS/.crash-seed")"
  HAVE_FIXTURE="$(sha256sum "$ASSETS/buggy/btrfs-convert" | cut -d' ' -f1)"
  if [ -n "$SEED_FIXTURE" ] && [ "$SEED_FIXTURE" != "$HAVE_FIXTURE" ]; then
    echo "error: Demo 8 crash seed $CRASH_SEED was calibrated for fixture" \
      "${SEED_FIXTURE:0:12}, but the fixture present is ${HAVE_FIXTURE:0:12}." \
      "Re-run scripts/prepare-demo08-assets.sh to recalibrate." >&2
    exit 2
  fi
else
  # Backwards-compatible seed for the original hand-built checked-in demo
  # fixture. Nightly-generated fixtures persist their calibrated seed above.
  CRASH_SEED=15
fi
[[ $CRASH_SEED =~ ^[0-9]+$ ]] || {
  echo "error: Demo 8 crash seed must be a non-negative integer" >&2
  exit 2
}
TIMEOUT="${DEMO08_TIMEOUT:-90}"
mkdir -p "$ARTIFACTS"

# Fresh reflink copy of the populated image for one conversion. btrfs-convert
# rewrites the image in place, so every run needs its own copy.
fresh_image() {
  local dst="$1"
  cp --reflink=auto "$IMAGE" "$dst"
}

# The deterministic core of an ASAN heap-use-after-free report: the error line
# (faulting heap address + PC), the two guest frames, and the SUMMARY. Under
# hermit these are byte-identical across replays of the same seed; only hermit's
# own host-side log lines vary, and this filter drops them.
asan_core() {
  grep -aE 'AddressSanitizer: heap-use-after-free|task_period_wait|print_copied_inodes|SUMMARY: AddressSanitizer' "$1" || true
}

# hermit chaos invocation validated to reproduce this UAF. --no-virtualize-cpuid
# because CPUID faulting is unavailable on the demo hosts; --sched-seed selects
# the interleaving.
chaos_convert() {
  local conv="$1" seed="$2" img="$3" out="$4"
  timeout "$TIMEOUT" "$HERMIT_RELEASE" --log=error run \
    --chaos --sched-seed "$seed" --no-virtualize-cpuid \
    -- "$conv" "$img" >"$out" 2>&1
}

echo "=== Demo 08: schedule-dependent btrfs-convert progress-thread UAF ==="
echo "seed=$CRASH_SEED timeout=${TIMEOUT}s"
echo "buggy=$BUGGY"
echo "fixed=$FIXED"
echo

# --- Step 1: native buggy is clean (bug dormant under blind execution) --------
echo "--- Step 1: native buggy btrfs-convert (blind execution) ---"
NATIVE_IMG="$ARTIFACTS/native-buggy.img"
fresh_image "$NATIVE_IMG"
if "$BUGGY" "$NATIVE_IMG" >"$ARTIFACTS/native-buggy.out" 2>&1; then
  echo "native buggy: clean exit (UAF dormant, as expected)"
else
  rc=$?
  # A native crash is possible but rare; it does not invalidate the demo's point
  # (chaos makes it reproducible), so report it rather than failing hard.
  echo "native buggy: exited rc=$rc (rare native manifestation; continuing)"
fi
echo

# --- Step 2: chaos buggy reproduces the UAF on a known seed -------------------
echo "--- Step 2: chaos buggy, --sched-seed $CRASH_SEED (expect ASAN UAF) ---"
CHAOS_IMG="$ARTIFACTS/chaos-buggy.img"
fresh_image "$CHAOS_IMG"
if chaos_convert "$BUGGY" "$CRASH_SEED" "$CHAOS_IMG" "$ARTIFACTS/chaos-buggy.out"; then
  echo "unexpected: chaos buggy seed $CRASH_SEED did not crash" >&2
  echo "(try another seed via DEMO08_CRASH_SEED; the sweep in the experiment" >&2
  echo " directory records which seeds crash)" >&2
  exit 1
fi
if ! grep -qaE 'AddressSanitizer: heap-use-after-free' "$ARTIFACTS/chaos-buggy.out"; then
  echo "chaos buggy seed $CRASH_SEED exited non-zero without an ASAN UAF report" >&2
  echo "(likely a ${TIMEOUT}s timeout on a pathologically slow schedule; pick a" >&2
  echo " different DEMO08_CRASH_SEED)" >&2
  exit 1
fi
asan_core "$ARTIFACTS/chaos-buggy.out" | tee "$ARTIFACTS/asan-report.txt"
echo "chaos buggy: reproduced the use-after-free"
echo

# --- Step 3: chaos fixed on the same seed is clean (the differential) ---------
echo "--- Step 3: chaos fixed, --sched-seed $CRASH_SEED (expect clean) ---"
FIXED_IMG="$ARTIFACTS/chaos-fixed.img"
fresh_image "$FIXED_IMG"
if chaos_convert "$FIXED" "$CRASH_SEED" "$FIXED_IMG" "$ARTIFACTS/chaos-fixed.out"; then
  echo "chaos fixed: clean exit on the crashing seed (73e211a7 closes the window)"
else
  rc=$?
  if grep -qaE 'AddressSanitizer: heap-use-after-free' "$ARTIFACTS/chaos-fixed.out"; then
    echo "regression: fixed variant reproduced the UAF" >&2
    exit 1
  fi
  echo "chaos fixed: non-crash exit rc=$rc (no UAF; likely the slow-schedule" \
       "timeout, not the bug)"
fi
echo

# --- Step 4: the chaos crash replays byte-for-byte ----------------------------
# Reuse the exact same image path (hence byte-identical argv) as Step 2: hermit
# determinism is per-input, and the faulting heap address depends on argv (the
# image path length shifts the initial heap layout). A different path would give
# a legitimately different-but-still-deterministic address, which would not
# demonstrate replay. Re-copy a fresh ext4 image over the same path.
echo "--- Step 4: replay --sched-seed $CRASH_SEED, confirm identical crash ---"
fresh_image "$CHAOS_IMG"
if chaos_convert "$BUGGY" "$CRASH_SEED" "$CHAOS_IMG" "$ARTIFACTS/chaos-buggy-replay.out"; then
  echo "unexpected: replay did not crash" >&2
  exit 1
fi
asan_core "$ARTIFACTS/chaos-buggy-replay.out" >"$ARTIFACTS/asan-report-replay.txt"
if cmp -s "$ARTIFACTS/asan-report.txt" "$ARTIFACTS/asan-report-replay.txt"; then
  echo "replay: guest ASAN report byte-identical (same heap address, PC, frames)"
else
  echo "replay: ASAN reports differ between runs" >&2
  diff "$ARTIFACTS/asan-report.txt" "$ARTIFACTS/asan-report-replay.txt" >&2 || true
  exit 1
fi

echo
echo "=== Demo 08: SUCCESS ==="
echo "native missed the UAF; chaos found it on seed $CRASH_SEED, the fix closed"
echo "it, and the crash replayed deterministically."
