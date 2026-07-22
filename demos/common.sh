# shellcheck shell=bash
# Shared setup for the dev-hermit demo scripts.
#
# Source this from a demo script; do not execute it directly. It locates the
# pinned hermit/ submodule inside this parent workspace, builds the binaries the
# walkthrough uses, and defines the helper wrappers the demos share.
#
# The demos deliberately disable CPUID virtualization and PMU timer preemption
# so the short examples also run on hosts without those features. CPUID is
# therefore a host input in these commands, and CPU-bound guests receive fewer
# preemption opportunities. The schedule-bisection demo is the exception and
# does require user-accessible CPU performance counters.

set -euo pipefail

# Resolve the workspace root (parent of demos/) and the hermit submodule.
DEMO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$DEMO_DIR/.." && pwd)"
export HERMIT_REPO="${HERMIT_REPO:-$ROOT/hermit}"

if [ ! -f "$HERMIT_REPO/Cargo.toml" ]; then
  echo "hermit submodule is not populated at $HERMIT_REPO" >&2
  echo "Run: git submodule update --init hermit" >&2
  exit 1
fi

# Build the debug binaries used for the validated record/replay path and
# source-resolved analyzer output. The release build remains available for
# normal use. Set DEMO_SKIP_BUILD=1 to reuse an existing build.
if [ "${DEMO_SKIP_BUILD:-0}" != "1" ]; then
  ( cd "$HERMIT_REPO" && cargo build --release && cargo build )
fi

export HERMIT="${HERMIT:-$HERMIT_REPO/target/debug/hermit}"
export HELLO_RACE="${HELLO_RACE:-$HERMIT_REPO/target/debug/hello_race}"
export HEAP_PTRS="${HEAP_PTRS:-$HERMIT_REPO/target/debug/rustbin_heap_ptrs}"
export RACE_SH="${RACE_SH:-$HERMIT_REPO/examples/race.sh}"

test -x "$HERMIT" || { echo "missing hermit binary: $HERMIT" >&2; exit 1; }

# Per-run scratch (private tmp) and ignored build-artifact scratch (under the
# hermit target/ directory). Both are created once and shared by the demo steps.
export DEMO_TMP="${DEMO_TMP:-$(mktemp -d -t hermit-demo.XXXXXX)}"
export DEMO_ARTIFACTS="${DEMO_ARTIFACTS:-$HERMIT_REPO/target/${DEMO_TMP##*/}}"
mkdir -p "$DEMO_TMP" "$DEMO_ARTIFACTS"

# Portable run wrapper: minimal environment, CPUID and PMU preemption disabled.
run_hermit() {
  "$HERMIT" --log=error run \
    --base-env=minimal \
    --no-virtualize-cpuid \
    --preemption-timeout=disabled \
    "$@"
}

# Verify wrapper for the built-in --verify demonstration.
#
# Two deliberate differences from run_hermit:
#   1. --log=info (not error). --verify compares the deterministic execution
#      log; at --log=error that log is EMPTY and the comparison is meaningless
#      ("Logs contain 0 | 0 messages total"). info populates it with thousands
#      of DETLOG/scheduler messages.
#   2. It does NOT pass --preemption-timeout=disabled. The racy guest below is
#      only reliably determinized with real PMU-based preemption; with
#      preemption disabled the two runs can diverge. This step therefore
#      requires user-accessible CPU performance counters (PMU).
verify_hermit() {
  "$HERMIT" --log=info run --verify --no-virtualize-cpuid "$@"
}

# Chaos wrapper: seeded scheduler PRNG for concurrency exploration.
chaos_run() {
  local seed="$1"
  "$HERMIT" --log=error run \
    --chaos \
    --seed="$seed" \
    --base-env=minimal \
    --no-virtualize-cpuid \
    --preemption-timeout=disabled \
    --env=HERMIT_MODE=chaos \
    -- "$HELLO_RACE"
}

demo_banner() {
  printf '\n=== %s ===\n' "$*"
}
