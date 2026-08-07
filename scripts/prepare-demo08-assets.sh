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
VARIANT_SOURCE="$ROOT/experiments/btrfs-convert-progress-uaf-chaos_20260729/src"
BTRFS_REPO="${DEMO08_BTRFS_REPO:-https://github.com/kdave/btrfs-progs.git}"
BTRFS_TAG=v7.1
BTRFS_COMMIT=4ab0e80be9e3bb1db2e6038e6d4316d35fb7ba8b
PREP_VERSION=1
STAMP="$ASSETS/.nightly-prep-version"
JOBS="${DEMO08_BUILD_JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 2)}"
HERMIT_RELEASE="${HERMIT_RELEASE:-$ROOT/hermit/target/release/hermit}"
CALIBRATION_SEEDS="${DEMO08_CALIBRATION_SEEDS:-64}"
# Measured on devbig176 (176-core AMD EPYC 9D64) over seeds 0-15 of the freshly built v7.1
# fixture: min 6s, median 11s, max 103s per seed. The former 30s default truncated the tail
# into false negatives, since a truncated seed cannot report a UAF it never reached.
CALIBRATION_TIMEOUT="${DEMO08_CALIBRATION_TIMEOUT:-150}"

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

for command in autoconf automake file git make mkfs.ext4 patch pkg-config sha256sum truncate; do
  command -v "$command" >/dev/null 2>&1 || fail "$command is required to prepare Demo 8"
done
[[ $JOBS =~ ^[1-9][0-9]*$ ]] || fail "DEMO08_BUILD_JOBS must be a positive integer"
[[ $CALIBRATION_SEEDS =~ ^[1-9][0-9]*$ ]] || \
  fail "DEMO08_CALIBRATION_SEEDS must be a positive integer"
[[ $CALIBRATION_TIMEOUT =~ ^[1-9][0-9]*$ ]] || \
  fail "DEMO08_CALIBRATION_TIMEOUT must be a positive integer"

# A crashing seed is a property of the exact fixture binary it was calibrated against, so it
# is stored WITH that binary's identity rather than as a bare integer. A cached seed whose
# recorded fixture hash does not match the fixture actually present is not evidence about this
# fixture and is discarded. Reusing a bare cached seed across a rebuilt fixture is how the gate
# came to depend on a value it had never re-derived (issue #1877).
fixture_identity() {
  sha256sum "$ASSETS/buggy/btrfs-convert" | cut -d' ' -f1
}

# A guest that ran leaves one of: a clean conversion (0), an ASAN abort (134), or a wall-clock
# truncation (124). Any other status is the wrapper/toolchain failing before or around the guest,
# which is NOT the same fact as "this seed did not crash" and must never be reported as one.
seed_executed() {
  local rc=$1 output=$2
  case "$rc" in
    0 | 124 | 134) [ -s "$output" ] && return 0 ;;
  esac
  return 1
}

calibrate_crash_seed() {
  local artifacts="${DEMO08_ARTIFACTS:-$ROOT/ignored/demo08-run}"
  local image="$artifacts/chaos-buggy.img"
  local output="$artifacts/.calibration.out"
  local seed rc fixture executed=0 attempted=0 last_rc="" cached_fixture

  fixture="$(fixture_identity)"

  if [ -r "$ASSETS/.crash-seed" ]; then
    seed="$(cut -d' ' -f1 <"$ASSETS/.crash-seed")"
    cached_fixture="$(cut -s -d' ' -f2 <"$ASSETS/.crash-seed")"
    if [[ $seed =~ ^[0-9]+$ ]] && [ "$cached_fixture" = "$fixture" ]; then
      echo "Demo 8 crash seed ready (cached seed $seed for fixture ${fixture:0:12})"
      return
    fi
    if [ -z "$cached_fixture" ]; then
      echo "Cached Demo 8 crash seed carries no fixture identity; recalibrating." >&2
    else
      echo "Cached Demo 8 crash seed was calibrated for fixture ${cached_fixture:0:12}," \
        "but this fixture is ${fixture:0:12}; recalibrating." >&2
    fi
  fi

  if [ ! -x "$HERMIT_RELEASE" ]; then
    echo "Building release Hermit for Demo 8 seed calibration..."
    make -C "$ROOT" --no-print-directory build-hermit
  fi
  [ -x "$HERMIT_RELEASE" ] || fail "release Hermit is unavailable: $HERMIT_RELEASE"

  mkdir -p "$artifacts"

  # Boxing is fail-closed: hermit-box-run exits 3, having run nothing, when cgroup-v2 /
  # systemd --user scope is unavailable. On a GitHub-managed runner that is the normal case,
  # and it made all 64 calibration seeds no-ops in under a second. Probe once and degrade
  # loudly rather than silently searching a space we never actually enter. The boxing exists
  # to stop a setsid/double-fork escapee leaking a burned core on the shared dev box; on an
  # ephemeral CI VM the per-seed wall `timeout` plus VM teardown covers that, so an unboxed
  # calibration there is an acceptable, and announced, degradation.
  local -a box=("$ROOT/scripts/hermit-box-run" --passthrough --label demo08.calib
    --cpu-budget "$((CALIBRATION_TIMEOUT * 4))")
  # Probe with the EXACT flag set the seed loop uses. A probe that differs from the call it
  # stands for is itself a proxy: a bare `--cpu-budget N -- true` boxes successfully on a
  # GitHub-managed runner where the real `--passthrough --label` invocation exits 3, so it
  # reported "boxing available" for a call shape that could not box.
  set +e
  "${box[@]}" -- true >/dev/null 2>&1
  local box_rc=$?
  set -e
  if [ "$box_rc" -eq 3 ]; then
    echo "WARNING: cgroup boxing unavailable here (hermit-box-run exit 3); calibrating UNBOXED." >&2
    box+=(--allow-cgroup-failure)
  fi

  echo "Calibrating a deterministic crashing seed for fixture ${fixture:0:12}" \
    "(up to $CALIBRATION_SEEDS seeds, ${CALIBRATION_TIMEOUT}s each)..."
  for ((seed = 0; seed < CALIBRATION_SEEDS; seed++)); do
    cp --reflink=auto "$ASSETS/pop-tiny.img" "$image"
    set +e
    # Box the bare hermit run so a livelock/escapee is reaped by cgroup.kill instead of
    # leaking a burned core (a `timeout` wall-cap only reaches the outer hermit, not a
    # setsid/double-fork inner supervisor). --passthrough keeps stdout+stderr byte-identical
    # so the ASAN grep below still sees the guest output; the wall `timeout` still governs
    # per-seed duration and the box CPU-budget (4x) only reaps a true runaway.
    "${box[@]}" -- \
      timeout "$CALIBRATION_TIMEOUT" "$HERMIT_RELEASE" --log=error run \
      --chaos --sched-seed "$seed" --no-virtualize-cpuid \
      -- "$ASSETS/buggy/btrfs-convert" "$image" >"$output" 2>&1
    rc=$?
    set -e
    attempted=$((attempted + 1))
    last_rc=$rc
    if seed_executed "$rc" "$output"; then
      executed=$((executed + 1))
    fi
    # ASAN can report the UAF on a thread whose process still exits 0, so the report text --
    # not the exit status -- is the detector. The exit status is what proves the guest ran.
    if grep -qa 'AddressSanitizer: heap-use-after-free' "$output"; then
      printf '%s %s\n' "$seed" "$fixture" >"$ASSETS/.crash-seed"
      rm -f -- "$image" "$output"
      echo "Demo 8 crash seed calibrated: $seed (guest exit $rc, fixture ${fixture:0:12})"
      return
    fi
  done

  # Distinguish the two failures the old message conflated. "No seed crashed" is a statement
  # about the fixture; "no seed ran" is a statement about this machine, and reporting the
  # second as the first is what kept #1877 undiagnosed for five hours.
  echo "--- last calibration output (rc=$last_rc) ---" >&2
  tail -n 25 -- "$output" >&2 || true
  echo "--- end ---" >&2
  if [ "$executed" -eq 0 ]; then
    rm -f -- "$image" "$output"
    fail "Demo 8 calibration never executed the guest: 0 of $attempted seeds produced a guest" \
      "exit status (0/124/134) with output; last rc=$last_rc. This is an environment failure" \
      "(hermit, hermit-box-run, or the fixture binary), NOT an absence of the UAF."
  fi
  rm -f -- "$image" "$output"
  fail "no ASAN UAF found in seeds 0-$((CALIBRATION_SEEDS - 1)) for fixture ${fixture:0:12}" \
    "($executed of $attempted seeds executed; raise DEMO08_CALIBRATION_SEEDS or" \
    "DEMO08_CALIBRATION_TIMEOUT if seeds are being truncated)"
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
