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

# Verify the native build prerequisites BEFORE attempting a cargo build. The
# Hermit dependency `unwind-sys` runs `pkg-config --libs --cflags
# libunwind-ptrace` in its build script and panics if the package is missing,
# which otherwise surfaces as a confusing mid-build failure on a fresh machine.
require_prereqs() {
  local missing=()
  if ! command -v pkg-config >/dev/null 2>&1; then
    missing+=("pkg-config")
  else
    # unwind-sys's build script runs `pkg-config ... libunwind-ptrace` and panics
    # if that module is absent, so this is the hard, reproducible requirement.
    pkg-config --exists libunwind-ptrace 2>/dev/null \
      || missing+=("libunwind-dev (provides the libunwind-ptrace pkg-config module)")
  fi
  # liblzma is a documented build dependency but is linked directly (-llzma)
  # rather than through pkg-config, so accept any of a pkg-config module, a
  # shared library, or the dev header to avoid false positives.
  if ! { pkg-config --exists liblzma 2>/dev/null \
         || ldconfig -p 2>/dev/null | grep -q 'liblzma\.so' \
         || [ -e /usr/include/lzma.h ]; }; then
    missing+=("liblzma-dev")
  fi
  if [ "${#missing[@]}" -ne 0 ]; then
    {
      echo "ERROR: missing build prerequisites: ${missing[*]}"
      echo "Install them and re-run this demo:"
      echo "  Debian/Ubuntu: sudo apt install libunwind-dev liblzma-dev pkg-config"
      echo "  Fedora/CentOS: sudo dnf install libunwind-devel xz-devel pkgconf-pkg-config"
    } >&2
    exit 1
  fi
}
require_prereqs

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

# Hermit's `run` mounts a private tmpfs over /tmp, so the guest does not see the
# real /tmp directory. When this checkout lives under /tmp, that hides the demo's
# own guest programs and scripts (hello_race, rustbin_heap_ptrs, race.sh, and the
# recorded schedule under target/), and hermit fails with "Could not execute ...
# No such file or directory". In that case, bind-mount the real /tmp (an identity
# mount via --tmp=/tmp) so those paths remain visible. For checkouts outside /tmp
# the default isolation is kept unchanged. HERMIT_ANALYZE_TMP_FLAGS forwards the
# same flag to the guest runs that `hermit analyze` spawns (demo 4).
HERMIT_TMP_FLAGS=()
HERMIT_ANALYZE_TMP_FLAGS=()
case "$HERMIT_REPO/" in
  /tmp/*)
    HERMIT_TMP_FLAGS=(--tmp=/tmp)
    HERMIT_ANALYZE_TMP_FLAGS=(--run-arg=--tmp=/tmp)
    ;;
esac

# Per-run scratch (private tmp) and ignored build-artifact scratch (under the
# hermit target/ directory). Both are created once and shared by the demo steps.
export DEMO_TMP="${DEMO_TMP:-$(mktemp -d -t hermit-demo.XXXXXX)}"
export DEMO_ARTIFACTS="${DEMO_ARTIFACTS:-$HERMIT_REPO/target/${DEMO_TMP##*/}}"
mkdir -p "$DEMO_TMP" "$DEMO_ARTIFACTS"

# Portable run wrapper: minimal environment, CPUID and PMU preemption disabled.
run_hermit() {
  "$HERMIT" --log=error run \
    "${HERMIT_TMP_FLAGS[@]}" \
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
  "$HERMIT" --log=info run --verify --no-virtualize-cpuid "${HERMIT_TMP_FLAGS[@]}" "$@"
}

# Chaos wrapper: seeded scheduler PRNG for concurrency exploration.
chaos_run() {
  local seed="$1"
  "$HERMIT" --log=error run \
    "${HERMIT_TMP_FLAGS[@]}" \
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

# Returns success when the host can virtualize CPUID (CPUID faulting available).
# Without it CPUID is a host input, which produces "Unable to intercept CPUID"
# warnings and can desync record/replay; callers should then add
# --no-virtualize-cpuid. The portable run wrappers above already pass it
# unconditionally; direct `hermit` invocations (e.g. demo 4's analyze) use this.
hermit_supports_cpuid_faulting() {
  ! "$HERMIT" --log=error run --base-env=minimal -- /bin/true 2>&1 \
    | grep -q "does not support CPUID faulting"
}

# Clear pass/fail verdict for a demo. The demo sets DEMO_LABEL before sourcing
# this file; the ERR trap fires on the first failing command under `set -e`, and
# demo_success prints on clean completion.
demo_success() { printf '\n=== %s: SUCCESS ===\n' "${DEMO_LABEL:-demo}"; }
demo_failure() {
  local rc=$?
  printf '\n=== %s: FAILURE (exit %d) — see errors above ===\n' "${DEMO_LABEL:-demo}" "$rc" >&2
}
trap demo_failure ERR
