# shellcheck shell=bash
# Shared setup for the dev-hermit demo scripts.
#
# Source this from a demo script; do not execute it directly. It locates the
# pinned hermit/ submodule inside this parent workspace, builds the binaries the
# walkthrough uses, and defines the helper wrappers the demos share.
#
# The demos deliberately disable CPUID virtualization so the short examples also
# run on hosts without CPUID faulting; CPUID is therefore a host input in these
# commands. PMU timer preemption is disabled for the chaos wrapper (portable
# syscall-boundary scheduling) but ENABLED for demo 1's run_hermit wrapper, whose
# --verify step already needs PMU and whose many-threaded python3 guest hangs
# under syscall-boundary-only scheduling (see run_hermit and HERMIT_DEMO_MAX_TIMESLICE).
# The schedule-bisection demo uses the portable syscall-boundary mode by default
# and offers precise PMU preemption as an explicit opt-in.

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

# Check the native dependencies before Cargo reaches unwind-sys's build script.
# Keeping this in the Makefile gives fresh machines and every demo the same
# package names and remediation path.
if ! command -v make >/dev/null 2>&1; then
  echo "ERROR: make is required to check the Hermit build dependencies." >&2
  exit 1
fi
# Build the release hermit binary (the portable run/verify wrappers below use
# it: the debug build serializes many-threaded guests like python3 so slowly it
# can OOM under load) plus the debug guest binaries whose source info the
# analyzer resolves (demo 4). Set DEMO_SKIP_BUILD=1 to reuse an existing build.
if [ "${DEMO_SKIP_BUILD:-0}" = "1" ]; then
  make --no-print-directory -s -C "$ROOT" check-deps
else
  case "${DEMO_BUILD_MODE:-all}" in
    release)
      make --no-print-directory -s -C "$ROOT" build
      ;;
    all)
      make --no-print-directory -s -C "$ROOT" check-deps
      ( cd "$HERMIT_REPO" && \
        cargo build --release -p hermit --bin hermit --no-default-features && \
        cargo build -p hermetic_infra_hermit_flaky-tests --bin hello_race && \
        cargo build -p hermetic_infra_hermit_tests --bin rustbin_heap_ptrs )
      ;;
    *)
      echo "ERROR: unsupported DEMO_BUILD_MODE: $DEMO_BUILD_MODE" >&2
      exit 1
      ;;
  esac
fi

# The hermit binary is the release build (fast, non-OOM for python3 and other
# many-threaded guests). The guest programs stay debug so demo 4's analyzer can
# resolve their source locations.
export HERMIT="${HERMIT:-$HERMIT_REPO/target/release/hermit}"
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

# Run wrapper: minimal environment, CPUID virtualization disabled, PMU-backed
# preemption ENABLED. run_hermit is used only by demo 1, whose final --verify
# step already requires user-accessible CPU performance counters (PMU), so the
# earlier steps use PMU preemption too. This matters for many-threaded guests
# such as python3: with preemption disabled (--max-timeslice=disabled) a
# CPU-spinning guest thread only yields at syscall boundaries, so under hermit's
# deterministic scheduler python3 can intermittently starve and hang for minutes
# (a hermit scheduler limitation, not a demo defect). PMU preemption makes each
# such run finish in about a second. On a host without accessible performance
# counters, set HERMIT_DEMO_MAX_TIMESLICE=disabled to restore the portable
# syscall-boundary-only behavior (python3 may then hang).
HERMIT_PREEMPTION_FLAGS=()
if [ -n "${HERMIT_DEMO_MAX_TIMESLICE:-}" ]; then
  HERMIT_PREEMPTION_FLAGS=(--max-timeslice="$HERMIT_DEMO_MAX_TIMESLICE")
fi
run_hermit() {
  "$HERMIT" --log=error run \
    "${HERMIT_TMP_FLAGS[@]}" \
    "${HERMIT_PREEMPTION_FLAGS[@]}" \
    --base-env=minimal \
    --no-virtualize-cpuid \
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
