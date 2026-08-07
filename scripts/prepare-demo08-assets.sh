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
CALIBRATION_TIMEOUT="${DEMO08_CALIBRATION_TIMEOUT:-30}"
# Test-only injection point. The runner receives SEED and IMAGE and writes the
# same combined stdout/stderr that a real Hermit invocation would produce.
CALIBRATION_RUNNER="${DEMO08_CALIBRATION_RUNNER:-}"

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

calibrate_crash_seed() {
  local artifacts="${DEMO08_ARTIFACTS:-$ROOT/ignored/demo08-run}"
  local image="$artifacts/chaos-buggy.img"
  local report="$artifacts/calibration.tsv"
  local output seed source rc engagement uaf
  local cached_seed=""
  local attempted=0
  local engaged=0
  local uaf_hits=0
  local found_seed=""
  local -a seeds=()
  local -a sources=()

  if [ -r "$ASSETS/.crash-seed" ]; then
    cached_seed="$(cat "$ASSETS/.crash-seed")"
    [[ $cached_seed =~ ^[0-9]+$ ]] || \
      fail "invalid cached Demo 8 crash seed: $cached_seed"
    # A cache is only a selection hint, never evidence. Replay it and record
    # both path engagement and the UAF signature before accepting it.
    seeds+=("$cached_seed")
    sources+=(cached)
  fi
  for ((seed = 0; seed < CALIBRATION_SEEDS; seed++)); do
    seeds+=("$seed")
    sources+=(cold)
  done

  if [ -z "$CALIBRATION_RUNNER" ] && [ ! -x "$HERMIT_RELEASE" ]; then
    echo "Building release Hermit for Demo 8 seed calibration..."
    make -C "$ROOT" --no-print-directory build-hermit
  fi
  if [ -z "$CALIBRATION_RUNNER" ]; then
    [ -x "$HERMIT_RELEASE" ] || \
      fail "release Hermit is unavailable: $HERMIT_RELEASE"
  else
    [ -x "$CALIBRATION_RUNNER" ] || \
      fail "Demo 8 calibration runner is not executable: $CALIBRATION_RUNNER"
  fi

  mkdir -p "$artifacts"
  printf 'seed\tsource\tengagement\tuaf\texit\toutput\n' >"$report"
  echo "Calibrating a deterministic crashing seed for this exact fixture..."
  for ((i = 0; i < ${#seeds[@]}; i++)); do
    seed="${seeds[$i]}"
    source="${sources[$i]}"
    # Do not repeat a cached seed after it has already proved the sweep can
    # fail. If the cached replay fails, the cold pass deliberately includes it
    # again so the configured seed range remains complete.
    if [ -n "$found_seed" ]; then
      break
    fi
    cp --reflink=auto "$ASSETS/pop-tiny.img" "$image"
    output="$artifacts/calibration-${source}-seed-${seed}.out"
    set +e
    if [ -n "$CALIBRATION_RUNNER" ]; then
      "$CALIBRATION_RUNNER" "$seed" "$image" >"$output" 2>&1
    else
      # Box the bare hermit run so a livelock/escapee is reaped by cgroup.kill instead of
      # leaking a burned core (a `timeout` wall-cap only reaches the outer hermit, not a
      # setsid/double-fork inner supervisor). --passthrough keeps stdout+stderr byte-identical
      # so the ASAN grep below still sees the guest output; the wall `timeout` still governs
      # per-seed duration and the box CPU-budget (4x) only reaps a true runaway.
      "$ROOT/scripts/hermit-box-run" --passthrough \
        --label "demo08.calib.${source}.${seed}" \
        --cpu-budget "$((CALIBRATION_TIMEOUT * 4))" -- \
        timeout "$CALIBRATION_TIMEOUT" "$HERMIT_RELEASE" --log=error run \
        --chaos --sched-seed "$seed" --no-virtualize-cpuid \
        -- "$ASSETS/buggy/btrfs-convert" "$image" >"$output" 2>&1
    fi
    rc=$?
    set -e

    attempted=$((attempted + 1))
    engagement=did-not-reach
    uaf=none
    # This marker is emitted inside print_copied_inodes after the progress
    # thread starts and immediately before its vulnerable task_period_wait.
    if grep -qa 'Copy inodes \[' "$output"; then
      engagement=reached
      engaged=$((engaged + 1))
    fi
    if grep -qa 'AddressSanitizer: heap-use-after-free' "$output"; then
      uaf=hit
      uaf_hits=$((uaf_hits + 1))
    fi
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
      "$seed" "$source" "$engagement" "$uaf" "$rc" "$output" >>"$report"
    printf '  seed=%s source=%s engagement=%s uaf=%s rc=%s output=%s\n' \
      "$seed" "$source" "$engagement" "$uaf" "$rc" "$output"

    # A signature without the engagement witness is not accepted: it is not
    # bound to the intended progress-thread path.
    if [ "$engagement" = reached ] && [ "$uaf" = hit ]; then
      found_seed="$seed"
    fi
  done

  printf 'Demo 8 calibration summary: engagement=%s/%s uaf_hits=%s/%s report=%s\n' \
    "$engaged" "$attempted" "$uaf_hits" "$attempted" "$report"
  if [ -n "$found_seed" ]; then
    printf '%s\n' "$found_seed" >"$ASSETS/.crash-seed"
    echo "Demo 8 crash seed calibrated: $found_seed"
    return
  fi
  if [ "$engaged" -eq 0 ]; then
    fail "Demo 8 calibration NO-RESULT: path engagement 0/$attempted; 0 UAF hits cannot be reported as clean (report: $report)"
  fi
  fail "no ASAN UAF found after $attempted attempted seeds; path engagement $engaged/$attempted (report: $report)"
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
printf '  crash_seed=%s\n' "$(cat "$ASSETS/.crash-seed")"
