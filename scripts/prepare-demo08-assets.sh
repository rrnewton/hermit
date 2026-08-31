#!/usr/bin/env bash
# Build the pinned Demo 8 ASAN fixtures from public btrfs-progs sources.

set -euo pipefail

# Safe probes must be pure: show usage and exit 0 BEFORE the required-tool checks
# and the (heavy, network-touching) asset build below.
for arg in "$@"; do
    case "$arg" in
        -h | --help)
            cat <<'EOF'
prepare-demo08-assets.sh — build the pinned Demo 8 ASAN btrfs-progs fixtures

USAGE:
  scripts/prepare-demo08-assets.sh              build/refresh the fixtures (clones + compiles; cached)
  scripts/prepare-demo08-assets.sh -h|--help    show this help and exit (no side effects)

Bare invocation is the CI/nightly prep step and DOES real work (git clone + build);
it is idempotent and no-ops when the cached assets are already current.
Env: DEMO08_DIR, DEMO08_BUILD_ROOT, DEMO08_BTRFS_REPO, DEMO08_BUILD_JOBS, HERMIT_RELEASE.
EOF
            exit 0
            ;;
    esac
done

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ASSETS="${DEMO08_DIR:-$ROOT/ignored/demo08-btrfs}"
BUILD_ROOT="${DEMO08_BUILD_ROOT:-$ROOT/ignored/demo08-build}"
SOURCE="$BUILD_ROOT/btrfs-progs-v7.1"
STAGING="$BUILD_ROOT/staging"
PATCH="$ROOT/demos/fixtures/demo08-convert-main-v7.1.patch"
VARIANT_SOURCE="$ROOT/demos/fixtures/demo08"
BTRFS_REPO="${DEMO08_BTRFS_REPO:-https://github.com/kdave/btrfs-progs.git}"
BTRFS_TAG=v7.1
BTRFS_COMMIT=4ab0e80be9e3bb1db2e6038e6d4316d35fb7ba8b
PREP_VERSION=1
STAMP="$ASSETS/.nightly-prep-version"
JOBS="${DEMO08_BUILD_JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 2)}"
HERMIT_RELEASE="${HERMIT_RELEASE:-$ROOT/target/release/hermit}"
SAFEHERMIT="${SAFEHERMIT:-$ROOT/bin/safehermit}"
CALIBRATION_SEEDS="${DEMO08_CALIBRATION_SEEDS:-64}"
# Measured on a 176-core AMD EPYC 9D64 development host over seeds 0-15 of the freshly built v7.1
# fixture: min 6s, median 11s, max 103s per seed. The former 30s default truncated the tail
# into false negatives, since a truncated seed cannot report a UAF it never reached.
CALIBRATION_TIMEOUT="${DEMO08_CALIBRATION_TIMEOUT:-150}"

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

# Tools the CALIBRATION path uses. Gated up front because an ungated dependency fails
# mid-sweep, where this harness would record it as a seed that simply did not crash -- a
# missing tool masquerading as a fact about the fixture.
for command in sha256sum timeout; do
  command -v "$command" >/dev/null 2>&1 || fail "$command is required to prepare Demo 8"
done

# Tools only the BUILD path uses. They are checked immediately before the build instead of
# here, so the cached path -- stamp matches, fixtures present, verify the seed and exit
# without building -- still works on a host with no build toolchain.
require_build_tools() {
  local command
  for command in autoconf automake file git make mkfs.ext4 patch pkg-config truncate; do
    command -v "$command" >/dev/null 2>&1 || fail "$command is required to build the Demo 8 fixtures"
  done
}
[[ $JOBS =~ ^[1-9][0-9]*$ ]] || fail "DEMO08_BUILD_JOBS must be a positive integer"
[[ $CALIBRATION_SEEDS =~ ^[1-9][0-9]*$ ]] || \
  fail "DEMO08_CALIBRATION_SEEDS must be a positive integer"
[[ $CALIBRATION_TIMEOUT =~ ^[1-9][0-9]*$ ]] || \
  fail "DEMO08_CALIBRATION_TIMEOUT must be a positive integer"

# A crashing seed is a property of the exact inputs it was calibrated against, so it is stored
# WITH their identities rather than as a bare integer:
#
#     <seed> <fixture-sha256> <hermit-sha256>
#
# The fixture field has been recorded and checked since #1877. The third field was already
# present in the asset directories in the field, written by hand and read by nobody -- both
# readers stopped at `cut -f2` -- so the data needed to notice a stale seed was on disk and
# ignored. It is now written by this script and reported by both readers.
#
# IT IS NOT THE GATE, and it must not be mistaken for one. Measured 2026-08-31 at Hermit head
# 00ed139b: seeds 3, 6, 10 and 13 reproduce the UAF while 15, 17 and 19 do not, against a
# FIXTURE-IDENTICAL binary. That establishes only that the fixture alone does not determine
# which seeds crash. It does NOT establish which other input does: no older Hermit was built
# to isolate the variable, and host, kernel and toolchain were equally uncontrolled. So a hash
# comparison can only ever be a hint. It would happily vouch for a stale seed if the deciding
# input is one nobody hashed, and it discards good seeds whenever a release rebuild is not
# bit-reproducible.
#
# The gate is REPLAY: a cached seed is run once and must actually report the UAF before it is
# used. That is an observation instead of an inference, and it holds whichever input changed.
# The hashes are kept because they make the reason legible in the message.
fixture_identity() {
  sha256sum "$ASSETS/buggy/btrfs-convert" | cut -d' ' -f1
}

hermit_identity() {
  sha256sum "$HERMIT_RELEASE" | cut -d' ' -f1
}

# The ASAN report TEXT is the detector. ASAN can report the UAF on a thread whose process
# still exits 0 -- seeds 3 and 13 at head 00ed139b do exactly that -- so the exit status
# cannot decide this. demos/08-btrfs-convert-uaf.sh applies the same rule. When the two
# disagreed, calibration selected seeds that the demo then rejected as "did not crash".
uaf_reported() {
  grep -qa 'AddressSanitizer: heap-use-after-free' "$1"
}

# A report that also reached its SUMMARY line carries the frames and PC that the demo's
# replay step compares. A truncated report is still a real detection and is still usable;
# it is just weaker evidence, so a complete one is preferred when the sweep finds one.
uaf_report_complete() {
  uaf_reported "$1" && grep -qa 'SUMMARY: AddressSanitizer' "$1"
}

# Whether the guest ran, and whether it reached the racing progress-thread path, are read out
# of the guest's own transcript rather than inferred from an exit status. The previous status
# allowlist (0/124/134) misread the real ASAN-abort status seen through safehermit, which is
# 1, as "this seed never ran".
guest_ran() {
  grep -qa 'btrfs-convert from btrfs-progs' "$1"
}

path_engaged() {
  grep -qa 'Copy inodes' "$1"
}

# Per-seed accounting, appended as it happens so a killed sweep still leaves its evidence:
#   seed <TAB> cold|cached <TAB> reached|did-not-reach <TAB> hit|none <TAB> rc <TAB> output
CALIBRATION_TSV=""

record_seed() {
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" "$5" "$6" >>"$CALIBRATION_TSV"
}

# One calibration run of one seed, written to $2. Both the cached-seed replay and the cold
# sweep go through here, so the run that VERIFIES a seed is the same call shape as the run
# that FOUND it. A verification that differs from the thing it stands for is not one.
# A nonzero guest status is ordinary here -- an ASAN abort is the outcome we are hunting --
# so it is captured with `|| rc=$?` rather than by toggling errexit. An earlier version did
# `set +e` around the run and `set -e` before returning, which handed the caller a nonzero
# return with errexit already back on: the whole sweep died on the first crashing seed, and
# died silently, because the output was still buffered in the caller's capture.
run_one_seed() {
  local seed="$1" output="$2" rc=0
  cp --reflink=auto "$ASSETS/pop-tiny.img" "$CALIBRATION_IMAGE"
  if [ -n "${DEMO08_CALIBRATION_RUNNER:-}" ]; then
    # Accounting-level test hook. It substitutes the guest, so a bracket using it exercises
    # this harness's bookkeeping and failure controls, NOT the Hermit path.
    "$DEMO08_CALIBRATION_RUNNER" "$seed" >"$output" 2>&1 || rc=$?
  else
    # Box the bare hermit run so a livelock/escapee is reaped by cgroup.kill instead of
    # leaking a burned core (a `timeout` wall-cap only reaches the outer hermit, not a
    # setsid/double-fork inner supervisor). --passthrough keeps stdout+stderr byte-identical
    # so the ASAN grep still sees the guest output; the wall `timeout` still governs per-seed
    # duration and the box CPU-budget (4x) only reaps a true runaway.
    # --strict, and the same flag set demos/08-btrfs-convert-uaf.sh uses. Without it the
    # guest's random filesystem UUID is not virtualized, the run is not reproducible, and a
    # seed selected here is not evidence about the run the demo will do. A seed is only
    # meaningful together with the flags it was derived under, so these two must not drift.
    "${CALIBRATION_BOX[@]}" -- \
      timeout "$CALIBRATION_TIMEOUT" "$SAFEHERMIT" "$HERMIT_RELEASE" --log=error run \
      --chaos --sched-seed "$seed" --no-virtualize-cpuid --strict \
      -- "$ASSETS/buggy/btrfs-convert" "$CALIBRATION_IMAGE" >"$output" 2>&1 || rc=$?
  fi
  rm -f -- "$CALIBRATION_IMAGE"
  return "$rc"
}

# Everything the seed loop needs, established once. Note that this now runs on the CACHED
# path too: verifying a seed by replaying it costs one guest run, so a cached seed can no
# longer be honoured on a host that cannot run Hermit. That is the price of not trusting the
# cache, and it is deliberate -- the untrusted-cache shortcut is what #1877 was.
calibration_setup() {
  local artifacts="${DEMO08_ARTIFACTS:-$ROOT/ignored/demo08-run}"
  mkdir -p "$artifacts"
  CALIBRATION_ARTIFACTS="$artifacts"
  CALIBRATION_IMAGE="$artifacts/chaos-buggy.img"
  CALIBRATION_TSV="$artifacts/calibration.tsv"
  printf 'seed\torigin\tengagement\tuaf\trc\toutput\n' >"$CALIBRATION_TSV"

  if [ -n "${DEMO08_CALIBRATION_RUNNER:-}" ]; then
    CALIBRATION_BOX=()
    return
  fi

  if [ ! -x "$HERMIT_RELEASE" ]; then
    echo "Building release Hermit for Demo 8 seed calibration..."
    make -C "$ROOT" --no-print-directory release-core
  fi
  [ -x "$HERMIT_RELEASE" ] || fail "release Hermit is unavailable: $HERMIT_RELEASE"
  [ -x "$SAFEHERMIT" ] || fail "safehermit wrapper is unavailable: $SAFEHERMIT"

  # Boxing is fail-closed: hermit-box-run exits 3, having run nothing, when cgroup boxing is
  # unavailable. On a GitHub-managed runner that is the normal case, and it made all 64
  # calibration seeds no-ops in under a second. Probe once and degrade loudly rather than
  # silently searching a space we never actually enter. The boxing exists to stop a
  # setsid/double-fork escapee leaking a burned core on the shared dev box; on an ephemeral
  # CI VM the per-seed wall `timeout` plus VM teardown covers that, so an unboxed calibration
  # there is an acceptable, and announced, degradation.
  CALIBRATION_BOX=("$ROOT/scripts/hermit-box-run" --passthrough --label demo08.calib
    --cpu-budget "$((CALIBRATION_TIMEOUT * 4))")
  # Probe with the EXACT flag set the seed loop uses. A probe that differs from the call it
  # stands for is itself a proxy: a bare `--cpu-budget N -- true` boxes successfully on a
  # GitHub-managed runner where the real `--passthrough --label` invocation exits 3, so it
  # reported "boxing available" for a call shape that could not box.
  set +e
  "${CALIBRATION_BOX[@]}" -- true >/dev/null 2>&1
  local box_rc=$?
  set -e
  if [ "$box_rc" -eq 3 ]; then
    echo "WARNING: cgroup boxing unavailable here (hermit-box-run exit 3); calibrating UNBOXED." >&2
    CALIBRATION_BOX+=(--allow-cgroup-failure)
  fi
}

# Replay a cached seed and require it to actually report the UAF. Returns 0 when the cached
# seed is still good, 1 when it is stale. A stale seed is a fact about this machine's inputs,
# NOT a regression in the fixture, and it is reported as such and recalibrated.
verify_cached_seed() {
  local seed="$1" fixture="$2" hermit="$3" cached_hermit="$4"
  local output="$CALIBRATION_ARTIFACTS/calibration-cached-seed-$seed.out" rc engaged uaf

  if [ -z "$cached_hermit" ]; then
    echo "Cached Demo 8 crash seed $seed records no Hermit identity; verifying it by replay."
  elif [ "$cached_hermit" != "$hermit" ]; then
    echo "Cached Demo 8 crash seed $seed was calibrated against Hermit" \
      "${cached_hermit:0:12} and this Hermit is ${hermit:0:12}; verifying it by replay."
  else
    echo "Verifying cached Demo 8 crash seed $seed by replaying it."
  fi

  rc=0
  run_one_seed "$seed" "$output" || rc=$?
  engaged=did-not-reach; path_engaged "$output" && engaged=reached
  uaf=none; uaf_reported "$output" && uaf=hit
  record_seed "$seed" cached "$engaged" "$uaf" "$rc" "$output"

  if [ "$uaf" = hit ]; then
    printf '%s %s %s\n' "$seed" "$fixture" "$hermit" >"$ASSETS/.crash-seed"
    echo "Demo 8 crash seed ready: cached seed $seed reproduced the UAF on replay" \
      "(engagement=1/1 uaf_hits=1/1, guest exit $rc, fixture ${fixture:0:12}," \
      "hermit ${hermit:0:12})"
    return 0
  fi

  # The distinction this whole path exists to make. Say STALE, and say what changed.
  echo "STALE CACHED SEED: Demo 8 seed $seed no longer reproduces the UAF here" \
    "(engagement=1/1 uaf_hits=0/1, guest exit $rc). The fixture is unchanged" \
    "(${fixture:0:12}), so this is a stale seed record and NOT a regression in the" \
    "fixture or the fix. Recalibrating." >&2
  if [ -n "$cached_hermit" ] && [ "$cached_hermit" != "$hermit" ]; then
    echo "  The Hermit binary also changed, ${cached_hermit:0:12} -> ${hermit:0:12}." \
      "Which input decides the crashing-seed set has not been isolated; the seed is" \
      "re-derived rather than guessed at." >&2
  fi
  return 1
}

calibrate_crash_seed() {
  local seed rc fixture hermit engaged uaf
  local engaged_count=0 attempted=0 hits=0 last_rc="" last_output=""
  local partial_seed="" partial_rc=""
  local cached_seed cached_fixture cached_hermit

  fixture="$(fixture_identity)"
  calibration_setup
  hermit=""
  [ -n "${DEMO08_CALIBRATION_RUNNER:-}" ] || hermit="$(hermit_identity)"

  if [ -r "$ASSETS/.crash-seed" ]; then
    cached_seed="$(cut -d' ' -f1 <"$ASSETS/.crash-seed")"
    cached_fixture="$(cut -s -d' ' -f2 <"$ASSETS/.crash-seed")"
    cached_hermit="$(cut -s -d' ' -f3 <"$ASSETS/.crash-seed")"
    if [[ $cached_seed =~ ^[0-9]+$ ]] && [ "$cached_fixture" = "$fixture" ]; then
      if verify_cached_seed "$cached_seed" "$fixture" "$hermit" "$cached_hermit"; then
        return
      fi
    elif [ -z "$cached_fixture" ]; then
      echo "Cached Demo 8 crash seed carries no fixture identity; recalibrating." >&2
    else
      echo "Cached Demo 8 crash seed was calibrated for fixture ${cached_fixture:0:12}," \
        "but this fixture is ${fixture:0:12}; recalibrating." >&2
    fi
  fi

  echo "Calibrating a deterministic crashing seed for fixture ${fixture:0:12}" \
    "(up to $CALIBRATION_SEEDS seeds, ${CALIBRATION_TIMEOUT}s each)..."
  for ((seed = 0; seed < CALIBRATION_SEEDS; seed++)); do
    local output="$CALIBRATION_ARTIFACTS/calibration-cold-seed-$seed.out"
    rc=0
    run_one_seed "$seed" "$output" || rc=$?
    attempted=$((attempted + 1))
    last_rc=$rc
    last_output="$output"
    engaged=did-not-reach
    if path_engaged "$output"; then
      engaged=reached
      engaged_count=$((engaged_count + 1))
    fi
    uaf=none
    if uaf_reported "$output"; then
      uaf=hit
      hits=$((hits + 1))
    fi
    record_seed "$seed" cold "$engaged" "$uaf" "$rc" "$output"

    if [ "$uaf" = hit ]; then
      if uaf_report_complete "$output"; then
        printf '%s %s %s\n' "$seed" "$fixture" "$hermit" >"$ASSETS/.crash-seed"
        echo "Demo 8 crash seed calibrated: $seed (complete ASAN report, guest exit $rc," \
          "engagement=$engaged_count/$attempted uaf_hits=$hits/$attempted," \
          "fixture ${fixture:0:12}, hermit ${hermit:0:12})"
        return
      fi
      # A truncated report is a real detection but a weak demonstration: the demo's replay
      # step compares frames and a PC this report does not carry. Keep it as the fallback and
      # keep looking for a complete one; settle for it only if the sweep finds nothing better.
      if [ -z "$partial_seed" ]; then
        partial_seed="$seed"
        partial_rc="$rc"
        echo "  seed $seed reported the UAF but the report is truncated (no SUMMARY line);" \
          "holding it as a fallback and continuing for a complete one." >&2
      fi
    fi
  done

  if [ -n "$partial_seed" ]; then
    printf '%s %s %s\n' "$partial_seed" "$fixture" "$hermit" >"$ASSETS/.crash-seed"
    echo "Demo 8 crash seed calibrated: $partial_seed (TRUNCATED ASAN report -- the guest" \
      "exited before ASAN finished writing frames; guest exit $partial_rc," \
      "engagement=$engaged_count/$attempted uaf_hits=$hits/$attempted," \
      "fixture ${fixture:0:12}, hermit ${hermit:0:12})"
    return
  fi

  # Three different failures that the original message conflated into one. "No seed crashed"
  # is a statement about the fixture; "no seed reached the racing path" and "no seed ran" are
  # statements about this machine, and reporting either as the first is what kept #1877
  # undiagnosed for five hours.
  echo "--- last calibration output (rc=$last_rc) ---" >&2
  tail -n 25 -- "$last_output" >&2 || true
  echo "--- end ---" >&2
  echo "Per-seed accounting: $CALIBRATION_TSV" >&2
  if [ "$engaged_count" -eq 0 ]; then
    fail "NO-RESULT: path engagement 0/$attempted. Not one seed reached btrfs-convert's" \
      "progress-thread path, so this sweep observed nothing about the UAF at all; last" \
      "rc=$last_rc. This is an environment failure (Hermit, hermit-box-run, or the fixture" \
      "binary), NOT an absence of the UAF."
  fi
  fail "no ASAN UAF found in seeds 0-$((CALIBRATION_SEEDS - 1)) for fixture ${fixture:0:12}" \
    "(engagement=$engaged_count/$attempted uaf_hits=0/$attempted; raise" \
    "DEMO08_CALIBRATION_SEEDS or DEMO08_CALIBRATION_TIMEOUT if seeds are being truncated)"
}

expected_stamp="prep=$PREP_VERSION btrfs=$BTRFS_COMMIT"
if [ "$(cat "$STAMP" 2>/dev/null || true)" = "$expected_stamp" ] \
   && [ -x "$ASSETS/buggy/btrfs-convert" ] \
   && [ -x "$ASSETS/fixed/btrfs-convert" ] \
   && [ -r "$ASSETS/pop-tiny.img" ]; then
  calibrate_crash_seed
  echo "Demo 8 assets ready (cached at $ASSETS)"
  exit 0
fi

require_build_tools

mkdir -p "$BUILD_ROOT"
if [ ! -d "$SOURCE/.git" ]; then
  tmp="$BUILD_ROOT/.btrfs-progs-v7.1.$$"
  rm -rf -- "$tmp"
  echo "Fetching btrfs-progs $BTRFS_TAG..."
  if timeout 20 git ls-remote --exit-code "$BTRFS_REPO" "refs/tags/$BTRFS_TAG" \
       >/dev/null 2>&1; then
    git clone --depth 1 --branch "$BTRFS_TAG" "$BTRFS_REPO" "$tmp"
  elif command -v with-proxy >/dev/null 2>&1; then
    echo "  direct connection failed; retrying through with-proxy..." >&2
    with-proxy git clone --depth 1 --branch "$BTRFS_TAG" "$BTRFS_REPO" "$tmp"
  else
    fail "cannot fetch $BTRFS_REPO directly and with-proxy is unavailable"
  fi
  [ "$(git -C "$tmp" rev-parse HEAD)" = "$BTRFS_COMMIT" ] || \
    fail "btrfs-progs $BTRFS_TAG did not resolve to pinned commit $BTRFS_COMMIT"
  mv "$tmp" "$SOURCE"
fi
[ "$(git -C "$SOURCE" rev-parse HEAD)" = "$BTRFS_COMMIT" ] || \
  fail "$SOURCE is not pinned btrfs-progs $BTRFS_COMMIT"

rm -rf -- "$STAGING"
mkdir -p "$STAGING"

build_variant() {
  local name="$1"
  local tree="$BUILD_ROOT/$name"

  rm -rf -- "$tree"
  cp -a --reflink=auto "$SOURCE" "$tree"
  cp "$VARIANT_SOURCE/$name/common/task-utils.c" "$tree/common/task-utils.c"
  cp "$VARIANT_SOURCE/$name/common/task-utils.h" "$tree/common/task-utils.h"
  patch --directory "$tree" --strip=1 --forward <"$PATCH"

  (
    cd "$tree"
    ./autogen.sh >/dev/null
    ./configure --disable-documentation --disable-python --disable-libudev \
      --disable-zoned --disable-backtrace --with-convert=ext2 \
      --with-crypto=builtin >/dev/null
    make -j "$JOBS" \
      EXTRA_CFLAGS='-fsanitize=address -fno-omit-frame-pointer -g -O1 -D_FORTIFY_SOURCE=0' \
      EXTRA_LDFLAGS='-fsanitize=address' btrfs-convert
  )
  install -D -m 0755 "$tree/btrfs-convert" "$STAGING/$name/btrfs-convert"
}

echo "Building Demo 8 buggy and fixed ASAN fixtures..."
build_variant buggy
build_variant fixed

populate="$BUILD_ROOT/populate"
image="$STAGING/pop-tiny.img"
rm -rf -- "$populate"
mkdir -p "$populate"
for n in $(seq 1 100); do
  printf 'Hermit Demo 8 fixture %03d\n' "$n" >"$populate/file-$n.txt"
done
truncate -s 256M "$image"
mkfs.ext4 -F -q -b 4096 -N 200 -d "$populate" "$image"

mkdir -p "$ASSETS/buggy" "$ASSETS/fixed"
install -m 0755 "$STAGING/buggy/btrfs-convert" "$ASSETS/buggy/btrfs-convert"
install -m 0755 "$STAGING/fixed/btrfs-convert" "$ASSETS/fixed/btrfs-convert"
install -m 0644 "$image" "$ASSETS/pop-tiny.img"
printf '%s\n' "$expected_stamp" >"$STAMP"
rm -f -- "$ASSETS/.crash-seed"
calibrate_crash_seed

printf 'Demo 8 assets prepared at %s\n' "$ASSETS"
printf '  buggy_sha256=%s\n' "$(sha256sum "$ASSETS/buggy/btrfs-convert" | cut -d' ' -f1)"
printf '  fixed_sha256=%s\n' "$(sha256sum "$ASSETS/fixed/btrfs-convert" | cut -d' ' -f1)"
printf '  image_sha256=%s\n' "$(sha256sum "$ASSETS/pop-tiny.img" | cut -d' ' -f1)"
printf '  crash_seed=%s\n' "$(cut -d' ' -f1 <"$ASSETS/.crash-seed")"
printf '  crash_seed_fixture=%s\n' "$(cut -s -d' ' -f2 <"$ASSETS/.crash-seed")"
printf '  crash_seed_hermit=%s\n' "$(cut -s -d' ' -f3 <"$ASSETS/.crash-seed")"
