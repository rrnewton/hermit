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
Env: DEMO08_DIR, DEMO08_BUILD_ROOT, DEMO08_BTRFS_REPO, DEMO08_BUILD_JOBS, HERMIT_RELEASE,
DEMO08_CALIBRATION_SEEDS, DEMO08_TIMEOUT (the demo's per-run budget, which also caps each
calibration run), DEMO08_CALIBRATION_TIMEOUT (must not exceed it).
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
# The demo's own per-run wall budget. demos/08-btrfs-convert-uaf.sh reads DEMO08_TIMEOUT with
# this same default, and a calibrated seed is only usable there if it aborts inside that budget.
DEMO_TIMEOUT="${DEMO08_TIMEOUT:-90}"
# Calibrate under the demo's budget rather than one chosen here. Measured on a 176-core AMD EPYC
# 9D64 development host over seeds 0-15 of the freshly built v7.1 fixture: min 6s, median 11s,
# max 103s per seed. A 30s cap truncated that tail into false negatives, since a truncated seed
# cannot report a UAF it never reached. Replacing it with a 150s cap produced the opposite error:
# a seed in the 90-150s tail qualified here and was then cut off at 90s by the demo, which saw
# rc=124 and a truncated report and refused the seed. Both are the same error -- producer and
# consumer bounding different things -- so the cap is now derived from the consumer's budget.
CALIBRATION_TIMEOUT="${DEMO08_CALIBRATION_TIMEOUT:-$DEMO_TIMEOUT}"
# Test-only injection point. The runner receives SEED, IMAGE and VARIANT and writes the
# same combined stdout/stderr that a real Hermit invocation would produce.
CALIBRATION_RUNNER="${DEMO08_CALIBRATION_RUNNER:-}"
CALIBRATION_FIXTURE_MODE="${DEMO08_CALIBRATION_FIXTURE_MODE:-}"
CALIBRATION_FIXTURE_ROOT="${DEMO08_CALIBRATION_FIXTURE_ROOT:-}"

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

BUILD_TOOLS=(autoconf automake file git make mkfs.ext4 patch pkg-config truncate)

require_build_tools() {
  local command
  for command in "${BUILD_TOOLS[@]}"; do
    command -v "$command" >/dev/null 2>&1 || fail "$command is required to build the Demo 8 fixtures"
  done
}

if [ -n "$CALIBRATION_RUNNER" ]; then
  [ "$CALIBRATION_FIXTURE_MODE" = 1 ] || \
    fail "DEMO08_CALIBRATION_RUNNER requires DEMO08_CALIBRATION_FIXTURE_MODE=1"
  [ -n "$CALIBRATION_FIXTURE_ROOT" ] || \
    fail "DEMO08_CALIBRATION_RUNNER requires DEMO08_CALIBRATION_FIXTURE_ROOT"
  [ -d "$CALIBRATION_FIXTURE_ROOT" ] || \
    fail "Demo 8 calibration fixture root is not a directory: $CALIBRATION_FIXTURE_ROOT"
  [ -e "$CALIBRATION_RUNNER" ] || \
    fail "Demo 8 calibration runner does not exist: $CALIBRATION_RUNNER"
  [ ! -L "$CALIBRATION_RUNNER" ] || \
    fail "Demo 8 calibration runner must not be a symlink: $CALIBRATION_RUNNER"
  fixture_root="$(cd -- "$CALIBRATION_FIXTURE_ROOT" && pwd -P)"
  [ "$fixture_root" != / ] || fail "Demo 8 calibration fixture root must not be /"
  runner_dir="$(cd -- "$(dirname -- "$CALIBRATION_RUNNER")" && pwd -P)"
  runner_path="$runner_dir/$(basename -- "$CALIBRATION_RUNNER")"
  case "$runner_path" in
    "$fixture_root"/*) ;;
    *) fail "Demo 8 calibration runner is outside the fixture root: $runner_path" ;;
  esac
  CALIBRATION_RUNNER="$runner_path"
fi

command -v sha256sum >/dev/null 2>&1 || fail "sha256sum is required to calibrate Demo 8"
if [ -z "$CALIBRATION_RUNNER" ]; then
  command -v timeout >/dev/null 2>&1 || fail "timeout is required to calibrate Demo 8"
fi
[[ $JOBS =~ ^[1-9][0-9]*$ ]] || fail "DEMO08_BUILD_JOBS must be a positive integer"
[[ $CALIBRATION_SEEDS =~ ^[1-9][0-9]*$ ]] || \
  fail "DEMO08_CALIBRATION_SEEDS must be a positive integer"
[[ $DEMO_TIMEOUT =~ ^[1-9][0-9]*$ ]] || fail "DEMO08_TIMEOUT must be a positive integer"
[[ $CALIBRATION_TIMEOUT =~ ^[1-9][0-9]*$ ]] || \
  fail "DEMO08_CALIBRATION_TIMEOUT must be a positive integer"
# Fail closed rather than certifying a seed the demo cannot run. A cap above the demo's own
# per-run budget is exactly how a seed that needs longer than DEMO08_TIMEOUT gets written to
# .crash-seed and then refused downstream at rc=124.
[ "$CALIBRATION_TIMEOUT" -le "$DEMO_TIMEOUT" ] || \
  fail "DEMO08_CALIBRATION_TIMEOUT=$CALIBRATION_TIMEOUT exceeds the demo's per-run budget" \
    "DEMO08_TIMEOUT=$DEMO_TIMEOUT; a seed calibrated above that budget is cut off by the demo"

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

# Boxing prefix for one guest execution, decided once by the probe in calibrate_crash_seed and
# reused by every run. Keeping it in one place is what lets the sweep and the confirmation runs
# be the same invocation rather than two that can drift apart.
BOX=()

# Result of the most recent run_variant call. Set as globals rather than echoed so the run
# stays in the main shell: a command substitution would move `set -e` into a subshell, where a
# failure before the echo would silently become an empty record instead of stopping the script.
RUN_RC=
RUN_ELAPSED=
RUN_ENGAGEMENT=
RUN_UAF=

# Run ONE fixture variant ("buggy" or "fixed") on ONE seed, leaving the combined guest
# stdout/stderr in OUTPUT and the classification in the RUN_* globals above. RUN_ELAPSED is
# whole seconds of wall time, which is the quantity the demo's own per-run budget bounds.
run_variant() {
  local variant=$1 seed=$2 image=$3 output=$4
  local rc start engagement=did-not-reach uaf=none

  cp --reflink=auto "$ASSETS/pop-tiny.img" "$image"
  start=$SECONDS
  set +e
  if [ -n "$CALIBRATION_RUNNER" ]; then
    "$CALIBRATION_RUNNER" "$seed" "$image" "$variant" >"$output" 2>&1
  else
    # Box the bare hermit run so a livelock/escapee is reaped by cgroup.kill instead of
    # leaking a burned core (a `timeout` wall-cap only reaches the outer hermit, not a
    # setsid/double-fork inner supervisor). --passthrough keeps stdout+stderr byte-identical
    # so the ASAN greps below still see the guest output; the wall `timeout` still governs
    # per-run duration and the box CPU-budget (4x) only reaps a true runaway.
    "${BOX[@]}" -- \
      timeout "$CALIBRATION_TIMEOUT" "$SAFEHERMIT" "$HERMIT_RELEASE" --log=error run \
      --chaos --sched-seed "$seed" --no-virtualize-cpuid \
      -- "$ASSETS/$variant/btrfs-convert" "$image" >"$output" 2>&1
  fi
  rc=$?
  set -e
  RUN_ELAPSED=$((SECONDS - start))

  if grep -qa 'Copy inodes \[' "$output"; then
    engagement=reached
  fi
  # The UAF report TEXT is not the success criterion, and treating it as one is what
  # let this calibration persist a seed the demo then refused. The demo's criterion is
  # the ASAN ABORT: guest 134, which it turns into its own outer 0. A run can print the
  # first lines of a report on one thread and still exit 0, or be cut off at 124
  # mid-report; both leave a truncated report and no abort. So record the text and the
  # abort separately and require BOTH. `SUMMARY: AddressSanitizer` is ASAN's last line,
  # so its presence is what separates a completed report from a truncated one.
  if grep -qa 'AddressSanitizer: heap-use-after-free' "$output"; then
    uaf=hit
    if grep -qa 'SUMMARY: AddressSanitizer' "$output"; then
      uaf=complete
    fi
  fi
  RUN_RC=$rc
  RUN_ENGAGEMENT=$engagement
  RUN_UAF=$uaf
}

# Is this buggy-variant run the crash the demo requires: guest executed, progress-thread path
# reached, ASAN report complete, and the abort status itself?
run_qualifies() {
  local rc=$1 engagement=$2 uaf=$3 output=$4
  seed_executed "$rc" "$output" || return 1
  [ "$engagement" = reached ] || return 1
  [ "$uaf" = complete ] || return 1
  [ "$rc" = 134 ]
}

# Append one row to calibration.tsv and echo the same fields for the log.
record_run() {
  local report=$1 seed=$2 source=$3 variant=$4 engagement=$5 uaf=$6 rc=$7 elapsed=$8 \
    qualifies=$9 output=${10}
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$seed" "$source" "$variant" "$engagement" "$uaf" "$rc" "$elapsed" "$qualifies" \
    "$output" >>"$report"
  printf '  seed=%s source=%s variant=%s engagement=%s uaf=%s rc=%s elapsed=%ss qualifies=%s output=%s\n' \
    "$seed" "$source" "$variant" "$engagement" "$uaf" "$rc" "$elapsed" "$qualifies" "$output"
}

# Confirm that a candidate seed satisfies the DEMO's contract before it is written to
# .crash-seed, because a seed the demo refuses is worse than no seed: it reads as a
# calibrated crash and produces a downstream failure whose cause is one step away.
#
# The demo replays the same buggy seed and requires the same abort, then requires the fixed
# variant on that seed to complete cleanly. Both are re-run here under the demo's own budget.
#
# Return 0 to accept, 1 to reject THIS seed and keep sweeping. Rejection is reserved for the
# wall-clock case (rc=124): a run cut off by the budget says the seed does not fit the demo,
# which is a property of the seed and a fair reason to try another one. Every other
# disagreement between two runs of one seed is a determinism failure or an environment
# failure, and searching for a friendlier seed would hide exactly what the demo exists to
# show, so those refuse the whole calibration.
confirm_seed() {
  local report=$1 artifacts=$2 seed=$3 source=$4
  local rc elapsed engagement uaf output qualifies

  output="$artifacts/calibration-confirm-replay-seed-${seed}.out"
  run_variant buggy "$seed" "$artifacts/chaos-buggy.img" "$output"
  rc=$RUN_RC elapsed=$RUN_ELAPSED engagement=$RUN_ENGAGEMENT uaf=$RUN_UAF
  qualifies=no
  if run_qualifies "$rc" "$engagement" "$uaf" "$output"; then
    qualifies=yes
  fi
  record_run "$report" "$seed" "$source" buggy-replay "$engagement" "$uaf" "$rc" "$elapsed" \
    "$qualifies" "$output"
  if [ "$qualifies" != yes ]; then
    if [ "$rc" = 124 ]; then
      echo "Demo 8 seed $seed replayed past the ${CALIBRATION_TIMEOUT}s budget (rc=124);" \
        "the demo would cut the same run off, so this seed is not accepted." >&2
      return 1
    fi
    fail "Demo 8 seed $seed crashed on its first run and did not on its replay" \
      "(rc=$rc engagement=$engagement uaf=$uaf). Two runs of one seed must agree; this is a" \
      "determinism or environment failure and must not be worked around by choosing another" \
      "seed (report: $report)"
  fi

  output="$artifacts/calibration-confirm-fixed-seed-${seed}.out"
  run_variant fixed "$seed" "$artifacts/chaos-fixed.img" "$output"
  rc=$RUN_RC elapsed=$RUN_ELAPSED engagement=$RUN_ENGAGEMENT uaf=$RUN_UAF
  record_run "$report" "$seed" "$source" fixed "$engagement" "$uaf" "$rc" "$elapsed" n/a \
    "$output"
  if [ "$uaf" != none ]; then
    fail "Demo 8 fixed variant reported a use-after-free on seed $seed (rc=$rc): the fix does" \
      "not close the window on this schedule (report: $report)"
  fi
  if [ "$rc" = 124 ]; then
    echo "Demo 8 fixed control on seed $seed ran past the ${CALIBRATION_TIMEOUT}s budget" \
      "(rc=124); the demo's differential would be cut off, so this seed is not accepted." >&2
    return 1
  fi
  if [ "$rc" != 0 ]; then
    fail "Demo 8 fixed control on seed $seed did not complete: rc=$rc. The demo requires a" \
      "clean fixed run to show the fix closes the window (report: $report)"
  fi
  return 0
}

calibrate_crash_seed() {
  local artifacts="${DEMO08_ARTIFACTS:-$ROOT/ignored/demo08-run}"
  local image="$artifacts/chaos-buggy.img"
  local report="$artifacts/calibration.tsv"
  local output seed source rc elapsed fixture cached_fixture engagement uaf qualifies i
  local executed=0 attempted=0 engaged=0 uaf_hits=0 qualified=0 rejected=0
  local last_rc="" found_seed="" found_source=""
  local cached_seed=""
  local -a seeds=() sources=()

  fixture="$(fixture_identity)"

  if [ -r "$ASSETS/.crash-seed" ]; then
    seed="$(cut -d' ' -f1 <"$ASSETS/.crash-seed")"
    cached_fixture="$(cut -s -d' ' -f2 <"$ASSETS/.crash-seed")"
    if [[ $seed =~ ^[0-9]+$ ]] && [ "$cached_fixture" = "$fixture" ]; then
      cached_seed="$seed"
      echo "Replaying cached Demo 8 crash seed $seed for fixture ${fixture:0:12}."
    elif [ -z "$cached_fixture" ]; then
      echo "Cached Demo 8 crash seed carries no fixture identity; recalibrating." >&2
    else
      echo "Cached Demo 8 crash seed was calibrated for fixture ${cached_fixture:0:12}," \
        "but this fixture is ${fixture:0:12}; recalibrating." >&2
    fi
  fi

  if [ -z "$CALIBRATION_RUNNER" ]; then
    if [ ! -x "$HERMIT_RELEASE" ]; then
      echo "Building release Hermit for Demo 8 seed calibration..."
      make -C "$ROOT" --no-print-directory release-core
    fi
    [ -x "$HERMIT_RELEASE" ] || fail "release Hermit is unavailable: $HERMIT_RELEASE"
    [ -x "$SAFEHERMIT" ] || fail "safehermit wrapper is unavailable: $SAFEHERMIT"
  else
    [ -x "$CALIBRATION_RUNNER" ] || \
      fail "Demo 8 calibration runner is not executable: $CALIBRATION_RUNNER"
  fi

  if [ -n "$cached_seed" ]; then
    seeds+=("$cached_seed")
    sources+=(cached)
  fi
  for ((seed = 0; seed < CALIBRATION_SEEDS; seed++)); do
    seeds+=("$seed")
    sources+=(cold)
  done

  mkdir -p "$artifacts"
  # `qualifies` answers one question only -- is this a qualifying buggy crash -- so the fixed
  # confirmation row carries n/a rather than a yes/no that would mean something different in
  # the same column. Its outcome is the `exit` and `uaf` fields.
  printf 'seed\tsource\tvariant\tengagement\tuaf\texit\telapsed\tqualifies\toutput\n' >"$report"

  # Boxing is fail-closed: hermit-box-run exits 3, having run nothing, when cgroup-v2 /
  # systemd --user scope is unavailable. On a GitHub-managed runner that is the normal case,
  # and it made all 64 calibration seeds no-ops in under a second. Probe once and degrade
  # loudly rather than silently searching a space we never actually enter. The boxing exists
  # to stop a setsid/double-fork escapee leaking a burned core on the shared dev box; on an
  # ephemeral CI VM the per-seed wall `timeout` plus VM teardown covers that, so an unboxed
  # calibration there is an acceptable, and announced, degradation.
  BOX=()
  if [ -z "$CALIBRATION_RUNNER" ]; then
    BOX=("$ROOT/scripts/hermit-box-run" --passthrough --label demo08.calib
      --cpu-budget "$((CALIBRATION_TIMEOUT * 4))")
    # Probe with the EXACT flag set the seed loop uses. A probe that differs from the call it
    # stands for is itself a proxy: a bare `--cpu-budget N -- true` boxes successfully on a
    # GitHub-managed runner where the real `--passthrough --label` invocation exits 3, so it
    # reported "boxing available" for a call shape that could not box.
    set +e
    "${BOX[@]}" -- true >/dev/null 2>&1
    local box_rc=$?
    set -e
    if [ "$box_rc" -eq 3 ]; then
      echo "WARNING: cgroup boxing unavailable here (hermit-box-run exit 3); calibrating UNBOXED." >&2
      BOX+=(--allow-cgroup-failure)
    fi
  fi

  echo "Calibrating a deterministic crashing seed for fixture ${fixture:0:12}" \
    "(up to $CALIBRATION_SEEDS seeds, ${CALIBRATION_TIMEOUT}s each," \
    "demo budget ${DEMO_TIMEOUT}s)..."
  for ((i = 0; i < ${#seeds[@]}; i++)); do
    seed="${seeds[$i]}"
    source="${sources[$i]}"
    output="$artifacts/calibration-${source}-seed-${seed}.out"
    run_variant buggy "$seed" "$image" "$output"
    rc=$RUN_RC elapsed=$RUN_ELAPSED engagement=$RUN_ENGAGEMENT uaf=$RUN_UAF
    attempted=$((attempted + 1))
    last_rc=$rc
    if seed_executed "$rc" "$output"; then
      executed=$((executed + 1))
    fi
    if [ "$engagement" = reached ]; then
      engaged=$((engaged + 1))
    fi
    if [ "$uaf" != none ]; then
      uaf_hits=$((uaf_hits + 1))
    fi
    qualifies=no
    if run_qualifies "$rc" "$engagement" "$uaf" "$output"; then
      qualifies=yes
    fi
    record_run "$report" "$seed" "$source" buggy "$engagement" "$uaf" "$rc" "$elapsed" \
      "$qualifies" "$output"
    if [ "$qualifies" = yes ]; then
      qualified=$((qualified + 1))
      # One qualifying run is a candidate, not a calibrated seed. Only a seed that also
      # satisfies the demo's replay and fixed-variant contract is written to .crash-seed.
      if confirm_seed "$report" "$artifacts" "$seed" "$source"; then
        found_seed="$seed"
        found_source="$source"
        break
      fi
      rejected=$((rejected + 1))
    fi
  done

  # uaf_hits counts REPORT TEXT; qualified counts seeds that also aborted with a
  # completed report. They differ exactly when a run printed part of a report and did
  # not abort, which is the case the demo refuses, so print both rather than letting a
  # hit count imply a usable seed. unconfirmed counts qualifying seeds that did not survive
  # the demo-contract confirmation, so a sweep that found crashes and still has no seed
  # says so instead of reading as "no UAF here".
  printf 'Demo 8 calibration summary: engagement=%s/%s uaf_hits=%s/%s qualified=%s/%s executed=%s/%s unconfirmed=%s report=%s\n' \
    "$engaged" "$attempted" "$uaf_hits" "$attempted" "$qualified" "$attempted" \
    "$executed" "$attempted" "$rejected" "$report"
  rm -f -- "$image" "$artifacts/chaos-fixed.img"
  if [ -n "$found_seed" ]; then
    printf '%s %s\n' "$found_seed" "$fixture" >"$ASSETS/.crash-seed"
    if [ "$found_source" = cached ]; then
      echo "Demo 8 crash seed replayed: cached seed $found_seed" \
        "(guest exit $last_rc, fixture ${fixture:0:12})"
    else
      echo "Demo 8 crash seed calibrated: $found_seed" \
        "(guest exit $last_rc, fixture ${fixture:0:12})"
    fi
    return
  fi

  # Distinguish guest execution from reaching the vulnerable progress-thread path. "No seed
  # crashed" is a statement about the fixture only after both facts have been observed.
  if [ "$executed" -eq 0 ]; then
    fail "Demo 8 calibration never executed the guest: 0 of $attempted seeds produced a guest" \
      "exit status (0/124/134) with output; last rc=$last_rc. This is an environment failure" \
      "(hermit, hermit-box-run, or the fixture binary), NOT an absence of the UAF."
  fi
  if [ "$qualified" -gt 0 ]; then
    fail "Demo 8 calibration found $qualified qualifying seed(s) but confirmed none: every" \
      "candidate ran past the ${CALIBRATION_TIMEOUT}s per-run budget the demo also applies," \
      "so no seed is usable at DEMO08_TIMEOUT=$DEMO_TIMEOUT (report: $report)"
  fi
  if [ "$engaged" -eq 0 ]; then
    fail "Demo 8 calibration NO-RESULT: path engagement 0/$attempted; $uaf_hits UAF hits" \
      "cannot qualify without the progress-thread witness (report: $report)"
  fi
  fail "no ASAN UAF found after $attempted attempted seeds; path engagement" \
    "$engaged/$attempted ($executed seeds executed; report: $report)"
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
