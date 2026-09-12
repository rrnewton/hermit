#!/usr/bin/env bash
# Bracket Demo 8 calibration accounting with no-engagement and real-ASAN-UAF fixtures.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PREP="$ROOT/scripts/prepare-demo08-assets.sh"
CLASSIFIER="$ROOT/scripts/demo08-calibration-path.sh"
TMP="$(mktemp -d -t demo08-calibration-test.XXXXXX)"
trap 'rm -rf -- "$TMP"' EXIT

# The same digest scripts/prepare-demo08-assets.sh puts in its cache stamp, over the patch and
# every Demo 8 variant source. Duplicated rather than exported because the script has no
# stamp-printing mode; if the two drift, make_assets writes a stamp prepare rejects and every
# case below fails loudly on the build path rather than silently skipping calibration.
fixture_source_digest() {
  {
    sha256sum <"$ROOT/demos/fixtures/demo08-convert-main-v7.1.patch"
    find "$ROOT/demos/fixtures/demo08" -type f -printf '%P\n' | LC_ALL=C sort |
      while read -r relative; do
        printf '%s ' "$relative"
        sha256sum <"$ROOT/demos/fixtures/demo08/$relative"
      done
  } | sha256sum | cut -d' ' -f1
}

make_assets() {
  local assets="$1"
  mkdir -p "$assets/buggy" "$assets/fixed"
  printf '#!/usr/bin/env bash\nexit 0\n' >"$assets/buggy/btrfs-convert"
  printf '#!/usr/bin/env bash\nexit 0\n' >"$assets/fixed/btrfs-convert"
  chmod +x "$assets/buggy/btrfs-convert" "$assets/fixed/btrfs-convert"
  : >"$assets/pop-tiny.img"
  # Must match expected_stamp in scripts/prepare-demo08-assets.sh exactly, or prepare takes
  # the BUILD path instead of the cached one and every case below stops testing calibration.
  # The fixture digest is computed the same way there; the two must be changed together.
  printf 'prep=2 btrfs=4ab0e80be9e3bb1db2e6038e6d4316d35fb7ba8b fixture-src=%s\n' \
    "$(fixture_source_digest)" \
    >"$assets/.nightly-prep-version"
}

cat >"$TMP/planted-uaf.c" <<'EOF'
#include <stdint.h>
#include <stdlib.h>

int main(void) {
  volatile uint8_t *value = malloc(1);
  if (value == NULL)
    return 2;
  *value = 7;
  free((void *)value);
  return *value;
}
EOF
# ASAN RUNTIME DISCOVERY. `-fsanitize=address` resolves to a linker script that
# names a versioned libasan; on this host that soname is absent from the default
# library path while the runtime itself is installed under another prefix, so a
# plain link fails with "cannot find /usr/lib64/libasan.so.<ver>" BEFORE the
# harness is ever exercised. Derive the directory instead of hardcoding one: a
# literal host path would be wrong on any other machine and is a portability-lint
# violation. The rpath matters as much as the -L -- without it the fixture LINKS
# but cannot RUN, which yields a passing build and a silently unexecuted check,
# i.e. exactly the fake-green this whole test exists to refuse.
asan_ld=()
if ! gcc -fsanitize=address -o "$TMP/asan-probe" -xc - >/dev/null 2>&1 <<<'int main(void){return 0;}'; then
  # `|| true` on both: `set -o pipefail` plus `head -1` closing the pipe makes
  # find/grep exit non-zero on success, which under `set -e` would abort the
  # script during the assignment itself -- silently, with no output at all.
  want=$(grep -oE 'libasan\.so\.[0-9.]+' "$(gcc -print-file-name=libasan.so)" 2>/dev/null | head -1 || true)
  [ -n "$want" ] || want='libasan.so'
  asan_dir=$(find /opt /usr/local -name "$want" -printf '%h\n' 2>/dev/null | head -1 || true)
  if [ -z "$asan_dir" ]; then
    echo "SKIP-REFUSED: no ASAN runtime ($want) found; the planted-UAF control cannot run," >&2
    echo "  and a sweep whose failure path is unproven must not be reported as clean." >&2
    exit 1
  fi
  asan_ld=(-L"$asan_dir" "-Wl,-rpath,$asan_dir")
  echo "note: ASAN runtime resolved to $asan_dir"
fi

gcc -O0 -g -fsanitize=address -fno-omit-frame-pointer \
  "$TMP/planted-uaf.c" -o "$TMP/planted-uaf" "${asan_ld[@]}"

# POSITIVE CONTROL ON THE CONTROL. Before using the planted UAF to prove the
# sweep can fail, prove the fixture itself actually reports one. A fixture that
# silently stopped detecting would make every downstream "sweep can fail" claim
# vacuous, and it would look identical to success.
control_out=$("$TMP/planted-uaf" 2>&1 || true)
if ! printf '%s' "$control_out" | grep -q 'heap-use-after-free'; then
  echo "SKIP-REFUSED: the planted-UAF fixture did not report heap-use-after-free;" >&2
  echo "  the failure control is inert, so nothing below could prove the sweep can fail." >&2
  # Print what it DID emit. A refusal that does not say what it saw sends the
  # next reader back to reproduce it by hand, which is how this test's own ASAN
  # link failure stayed unexplained across several attempts.
  echo "  fixture emitted ${#control_out} byte(s):" >&2
  printf '%s\n' "$control_out" | head -5 | sed 's/^/    /' >&2
  exit 1
fi

cat >"$TMP/runner.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
seed="${1:?seed required}"
# The third argument is the fixture variant. The calibration confirms a candidate seed by
# replaying the buggy variant and running the fixed one, so a runner that ignores the variant
# could not distinguish the demo's crash from its differential control.
variant="${3:-buggy}"

# Per-(variant, seed) invocation counter, so a mode can answer differently on the sweep run
# and on the confirmation replay of the same seed.
count=1
if [ -n "${DEMO08_TEST_COUNT_DIR:-}" ]; then
  mkdir -p "$DEMO08_TEST_COUNT_DIR"
  counter="$DEMO08_TEST_COUNT_DIR/$variant-$seed"
  [ ! -r "$counter" ] || count=$(($(cat "$counter") + 1))
  printf '%s\n' "$count" >"$counter"
fi

abort_with_uaf() {
  ASAN_OPTIONS=detect_leaks=0:abort_on_error=1 \
    "${DEMO08_TEST_UAF_BIN:?UAF binary required}"
}

partial_report() {
  echo '==123==ERROR: AddressSanitizer: heap-use-after-free on address 0x606000000210'
  echo 'READ of size 8 at 0x606000000210 thread T1'
}

case "${DEMO08_TEST_MODE:?mode required}" in
  no-engagement)
    echo 'fixture exited before progress-thread path'
    ;;
  engaged-no-hit)
    printf 'Copy inodes [o] [         0/         1]\r\n'
    echo 'Conversion complete'
    ;;
  planted-uaf)
    printf 'Copy inodes [o] [         0/         1]\r\n'
    if [ "$variant" = buggy ] && [ "$seed" = "${DEMO08_TEST_UAF_SEED:?UAF seed required}" ]; then
      abort_with_uaf
    else
      echo 'Conversion complete'
    fi
    ;;
  replay-timeout)
    # The candidate crashes on its sweep run and is cut off by the wall budget when the
    # calibration replays it: a seed the demo would also cut off at DEMO08_TIMEOUT.
    printf 'Copy inodes [o] [         0/         1]\r\n'
    if [ "$variant" != buggy ]; then
      echo 'Conversion complete'
    elif [ "$count" -eq 1 ]; then
      abort_with_uaf
    else
      partial_report
      exit 124
    fi
    ;;
  replay-clean)
    # The candidate crashes once and not again. That is a disagreement between two runs of one
    # seed, not a budget problem, so the calibration must refuse rather than try another seed.
    printf 'Copy inodes [o] [         0/         1]\r\n'
    if [ "$variant" != buggy ]; then
      echo 'Conversion complete'
    elif [ "$count" -eq 1 ]; then
      abort_with_uaf
    else
      echo 'Conversion complete'
    fi
    ;;
  fixed-timeout)
    printf 'Copy inodes [o] [         0/         1]\r\n'
    if [ "$variant" = buggy ]; then
      abort_with_uaf
    else
      echo 'Conversion incomplete'
      exit 124
    fi
    ;;
  fixed-uaf)
    printf 'Copy inodes [o] [         0/         1]\r\n'
    abort_with_uaf
    ;;
  uaf-no-engagement)
    abort_with_uaf
    ;;
  partial-uaf-rc0)
    # The exact shape the review caught in production: engagement, the first lines
    # of a UAF report on a thread whose process still exits 0, and NO SUMMARY, so
    # the report is truncated and there was no abort.
    printf 'Copy inodes [o] [         0/         1]\r\n'
    echo '==123==ERROR: AddressSanitizer: heap-use-after-free on address 0x606000000210'
    echo 'READ of size 8 at 0x606000000210 thread T1'
    exit 0
    ;;
  partial-uaf-rc124)
    # Same partial report, cut off by the per-seed wall timeout instead.
    printf 'Copy inodes [o] [         0/         1]\r\n'
    echo '==123==ERROR: AddressSanitizer: heap-use-after-free on address 0x606000000210'
    echo 'READ of size 8 at 0x606000000210 thread T1'
    exit 124
    ;;
  complete-uaf-rc0)
    # A COMPLETE report -- closing SUMMARY line included -- from a guest that never
    # aborted. A fixture built without abort_on_error prints exactly this and exits
    # normally, so every text-based check passes and only the exit status can tell
    # this apart from the crash the demo publishes.
    #
    # The fixed variant converts cleanly, so nothing ELSE would refuse this seed:
    # remove the exit-status check and the confirmation runs succeed and the seed is
    # persisted. That is what makes this bracket a test of the status check alone.
    printf 'Copy inodes [o] [         0/         1]\r\n'
    if [ "$variant" != buggy ]; then
      echo 'Conversion complete'
    else
      echo '==123==ERROR: AddressSanitizer: heap-use-after-free on address 0x606000000210'
      echo 'READ of size 8 at 0x606000000210 thread T1'
      echo 'SUMMARY: AddressSanitizer: heap-use-after-free common/task-utils.c:154 in task_period_wait'
      exit 0
    fi
    ;;
  runner-failure)
    echo 'fixture runner failed before guest execution' >&2
    exit 127
    ;;
  runner-failure-with-signatures)
    printf 'Copy inodes [o] [         0/         1]\r\n'
    abort_with_uaf || true
    exit 127
    ;;
  *)
    echo "unknown test mode: $DEMO08_TEST_MODE" >&2
    exit 2
    ;;
esac
EOF
chmod +x "$TMP/runner.sh"

run_prepare() {
  local assets="$1" artifacts="$2" seeds="$3"
  env \
    DEMO08_DIR="$assets" \
    DEMO08_BUILD_ROOT="$TMP/build-unused" \
    DEMO08_ARTIFACTS="$artifacts" \
    DEMO08_TEST_COUNT_DIR="$artifacts/counts" \
    DEMO08_CALIBRATION_SEEDS="$seeds" \
    DEMO08_CALIBRATION_TIMEOUT=1 \
    DEMO08_CALIBRATION_RUNNER="$TMP/runner.sh" \
    DEMO08_CALIBRATION_FIXTURE_MODE=1 \
    DEMO08_CALIBRATION_FIXTURE_ROOT="$TMP" \
    HERMIT_RELEASE="$TMP/not-used-hermit" \
    "$PREP"
}

# An ambient runner variable cannot bypass the normal safehermit path. Fixture
# mode must be explicit, and the runner must be contained by its supplied root.
assets="$TMP/assets-runner-guard"
artifacts="$TMP/artifacts-runner-guard"
make_assets "$assets"
set +e
runner_guard_output="$(DEMO08_TEST_MODE=no-engagement env \
  DEMO08_DIR="$assets" DEMO08_ARTIFACTS="$artifacts" \
  DEMO08_CALIBRATION_RUNNER="$TMP/runner.sh" \
  HERMIT_RELEASE="$TMP/not-used-hermit" "$PREP" 2>&1)"
runner_guard_rc=$?
set -e
[ "$runner_guard_rc" -ne 0 ]
grep -q 'requires DEMO08_CALIBRATION_FIXTURE_MODE=1' <<<"$runner_guard_output"

mkdir -p "$TMP/other-fixture-root"
set +e
runner_guard_output="$(DEMO08_TEST_MODE=no-engagement env \
  DEMO08_DIR="$assets" DEMO08_ARTIFACTS="$artifacts" \
  DEMO08_CALIBRATION_RUNNER="$TMP/runner.sh" \
  DEMO08_CALIBRATION_FIXTURE_MODE=1 \
  DEMO08_CALIBRATION_FIXTURE_ROOT="$TMP/other-fixture-root" \
  HERMIT_RELEASE="$TMP/not-used-hermit" "$PREP" 2>&1)"
runner_guard_rc=$?
set -e
[ "$runner_guard_rc" -ne 0 ]
grep -q 'is outside the fixture root' <<<"$runner_guard_output"

# Negative bracket: two attempted seeds, no path engagement. This must be a
# refused NO-RESULT, with both per-seed outputs and rows retained.
assets="$TMP/assets-no-engagement"
artifacts="$TMP/artifacts-no-engagement"
make_assets "$assets"
set +e
no_engagement_output="$(DEMO08_TEST_MODE=no-engagement \
  run_prepare "$assets" "$artifacts" 2 2>&1)"
no_engagement_rc=$?
set -e
[ "$no_engagement_rc" -ne 0 ]
grep -q 'NO-RESULT: path engagement 0/2' <<<"$no_engagement_output"
[ "$(wc -l <"$artifacts/calibration.tsv")" -eq 3 ]
[ "$(grep -c $'\tdid-not-reach\t' "$artifacts/calibration.tsv")" -eq 2 ]
[ "$(find "$artifacts" -maxdepth 1 -name 'calibration-cold-seed-*.out' | wc -l)" -eq 2 ]

# Positive engagement without a hit is evidence-bearing but still fails the
# crash-seed calibration. It must not be mislabeled NO-RESULT.
assets="$TMP/assets-engaged"
artifacts="$TMP/artifacts-engaged"
make_assets "$assets"
set +e
engaged_output="$(DEMO08_TEST_MODE=engaged-no-hit \
  run_prepare "$assets" "$artifacts" 2 2>&1)"
engaged_rc=$?
set -e
[ "$engaged_rc" -ne 0 ]
grep -q 'path engagement 2/2' <<<"$engaged_output"
! grep -q 'NO-RESULT' <<<"$engaged_output"
[ "$(grep -c $'\treached\tnone\t' "$artifacts/calibration.tsv")" -eq 2 ]

# A UAF report TEXT is not the success criterion; the ASAN ABORT is. A run that
# prints the first lines of a report on a thread and still exits 0 produced no
# abort, and the demo refuses such a seed -- so the calibration must never
# persist one. This is the exact shape found in production at 0fd1653f, where a
# seed selected on report text with rc=0 was handed to the demo and rejected.
assets="$TMP/assets-partial-rc0"
artifacts="$TMP/artifacts-partial-rc0"
make_assets "$assets"
set +e
partial0_output="$(DEMO08_TEST_MODE=partial-uaf-rc0 \
  run_prepare "$assets" "$artifacts" 2 2>&1)"
partial0_rc=$?
set -e
[ "$partial0_rc" -ne 0 ]
[ ! -f "$assets/.crash-seed" ]
[ "$(grep -cE $'\treached\thit\t0\t[0-9]+\tno\t' "$artifacts/calibration.tsv")" -eq 2 ]
! grep -q $'\tyes\t' "$artifacts/calibration.tsv"
# The summary must distinguish report TEXT from a qualifying abort, or a reader
# sees uaf_hits=2/2 next to a refusal and cannot tell why.
grep -q 'uaf_hits=2/2 qualified=0/2' <<<"$partial0_output"

# The same partial report truncated by the per-seed wall timeout is refused for
# the same reason: 124 is a cut-off, not an abort.
assets="$TMP/assets-partial-rc124"
artifacts="$TMP/artifacts-partial-rc124"
make_assets "$assets"
set +e
partial124_output="$(DEMO08_TEST_MODE=partial-uaf-rc124 \
  run_prepare "$assets" "$artifacts" 2 2>&1)"
partial124_rc=$?
set -e
[ "$partial124_rc" -ne 0 ]
[ ! -f "$assets/.crash-seed" ]
[ "$(grep -cE $'\treached\thit\t124\t[0-9]+\tno\t' "$artifacts/calibration.tsv")" -eq 2 ]
grep -q 'uaf_hits=2/2 qualified=0/2' <<<"$partial124_output"

# THE ABORT STATUS, NOT THE REPORT'S COMPLETENESS. The two brackets above are both refused
# because their report is truncated, so neither of them exercises the required exit status:
# delete that requirement and they still pass. This one closes that. The report here is
# COMPLETE -- `uaf=complete`, SUMMARY line and all -- and the guest still never aborted,
# which is what a fixture built without abort_on_error does. Every text check therefore
# succeeds and `run_qualifies`' exit-status check is the only thing left refusing the seed.
assets="$TMP/assets-complete-rc0"
artifacts="$TMP/artifacts-complete-rc0"
make_assets "$assets"
set +e
complete0_output="$(DEMO08_TEST_MODE=complete-uaf-rc0 \
  run_prepare "$assets" "$artifacts" 2 2>&1)"
complete0_rc=$?
set -e
[ "$complete0_rc" -ne 0 ]
[ ! -e "$assets/.crash-seed" ]
# `complete`, not `hit`: the text bar is fully met and the seed is refused anyway.
[ "$(grep -cE $'\tbuggy\treached\tcomplete\t0\t[0-9]+\tno\t' "$artifacts/calibration.tsv")" -eq 2 ]
! grep -q $'\tyes\t' "$artifacts/calibration.tsv"
# A seed that never qualified must not reach confirmation, so no replay or fixed row exists.
! grep -q $'\tbuggy-replay\t' "$artifacts/calibration.tsv"
! grep -q $'\tfixed\t' "$artifacts/calibration.tsv"
grep -q 'uaf_hits=2/2 qualified=0/2' <<<"$complete0_output"

# Falsifiability bracket: seed 1 reaches the path and runs an actual ASAN
# use-after-free. The harness must select it and preserve the signature.
assets="$TMP/assets-uaf"
artifacts="$TMP/artifacts-uaf"
make_assets "$assets"
uaf_output="$(DEMO08_TEST_MODE=planted-uaf \
  DEMO08_TEST_UAF_SEED=1 DEMO08_TEST_UAF_BIN="$TMP/planted-uaf" \
  run_prepare "$assets" "$artifacts" 3 2>&1)"
grep -q 'engagement=2/2 uaf_hits=1/2' <<<"$uaf_output"
grep -q 'Demo 8 crash seed calibrated: 1' <<<"$uaf_output"
! grep -q 'Demo 8 crash seed replayed:' <<<"$uaf_output"
printf '%s\n' "$uaf_output" >"$TMP/cold.log"
[ "$("$CLASSIFIER" --log "$TMP/cold.log" --force-cold false)" = cold-calibration ]
[ "$("$CLASSIFIER" --log "$TMP/cold.log" --force-cold true)" = cold-calibration ]
fixture=$(sha256sum "$assets/buggy/btrfs-convert" | cut -d' ' -f1)
[ "$(cat "$assets/.crash-seed")" = "1 $fixture" ]
grep -qE $'^1\tcold\tbuggy\treached\tcomplete\t134\t[0-9]+\tyes\t' "$artifacts/calibration.tsv"
grep -q 'AddressSanitizer: heap-use-after-free' \
  "$artifacts/calibration-cold-seed-1.out"
# The demo replays the same seed and then runs the fixed variant on it. A seed is only
# persisted after the calibration has observed both, so both rows must be in the report.
grep -qE $'^1\tcold\tbuggy-replay\treached\tcomplete\t134\t[0-9]+\tyes\t' \
  "$artifacts/calibration.tsv"
grep -qE $'^1\tcold\tfixed\treached\tnone\t0\t[0-9]+\tn/a\t' "$artifacts/calibration.tsv"
grep -q 'AddressSanitizer: heap-use-after-free' \
  "$artifacts/calibration-confirm-replay-seed-1.out"
[ "$(wc -l <"$artifacts/calibration.tsv")" -eq 5 ]

# BUDGET PARITY. The demo cuts every run off at DEMO08_TIMEOUT, so a calibration that searches
# above that budget certifies seeds the demo then refuses at rc=124. Reject the configuration
# outright rather than producing a seed nobody can use.
assets="$TMP/assets-budget"
artifacts="$TMP/artifacts-budget"
make_assets "$assets"
set +e
budget_output="$(DEMO08_TEST_MODE=planted-uaf DEMO08_TEST_UAF_SEED=1 \
  DEMO08_TEST_UAF_BIN="$TMP/planted-uaf" DEMO08_TIMEOUT=30 env \
  DEMO08_DIR="$assets" DEMO08_ARTIFACTS="$artifacts" \
  DEMO08_CALIBRATION_SEEDS=2 DEMO08_CALIBRATION_TIMEOUT=60 \
  DEMO08_CALIBRATION_RUNNER="$TMP/runner.sh" \
  DEMO08_CALIBRATION_FIXTURE_MODE=1 DEMO08_CALIBRATION_FIXTURE_ROOT="$TMP" \
  DEMO08_TEST_COUNT_DIR="$artifacts/counts" \
  HERMIT_RELEASE="$TMP/not-used-hermit" "$PREP" 2>&1)"
budget_rc=$?
set -e
[ "$budget_rc" -ne 0 ]
grep -q 'exceeds the demo.s per-run budget' <<<"$budget_output"
[ ! -e "$assets/.crash-seed" ]

# A seed that crashes on its sweep run and is then cut off by the same budget on its replay is
# exactly the seed the demo refuses. It must not be written to .crash-seed, and a sweep whose
# every candidate behaves that way must say so rather than report an absent UAF.
assets="$TMP/assets-replay-timeout"
artifacts="$TMP/artifacts-replay-timeout"
make_assets "$assets"
set +e
replay_timeout_output="$(DEMO08_TEST_MODE=replay-timeout \
  DEMO08_TEST_UAF_BIN="$TMP/planted-uaf" \
  run_prepare "$assets" "$artifacts" 2 2>&1)"
replay_timeout_rc=$?
set -e
[ "$replay_timeout_rc" -ne 0 ]
[ ! -e "$assets/.crash-seed" ]
grep -q 'replayed past the 1s budget' <<<"$replay_timeout_output"
grep -q 'qualifying seed(s) but confirmed none' <<<"$replay_timeout_output"
grep -q 'unconfirmed=2' <<<"$replay_timeout_output"
! grep -q 'no ASAN UAF found' <<<"$replay_timeout_output"
[ "$(grep -cE $'\tbuggy-replay\treached\thit\t124\t[0-9]+\tno\t' \
  "$artifacts/calibration.tsv")" -eq 2 ]

# A seed that crashes once and not again is a disagreement between two runs of one seed. That
# is what the demo exists to expose, so the calibration refuses instead of quietly moving on
# to a seed that happens to behave.
assets="$TMP/assets-replay-clean"
artifacts="$TMP/artifacts-replay-clean"
make_assets "$assets"
set +e
replay_clean_output="$(DEMO08_TEST_MODE=replay-clean \
  DEMO08_TEST_UAF_BIN="$TMP/planted-uaf" \
  run_prepare "$assets" "$artifacts" 2 2>&1)"
replay_clean_rc=$?
set -e
[ "$replay_clean_rc" -ne 0 ]
[ ! -e "$assets/.crash-seed" ]
grep -q 'did not on its replay' <<<"$replay_clean_output"
# It must stop at the first candidate rather than sweeping on: seed 0's two rows only.
[ "$(wc -l <"$artifacts/calibration.tsv")" -eq 3 ]

# The demo's Step 3 differential runs the fixed variant on the same seed. A fixed control the
# budget cuts off makes the seed unusable; a fixed control that reports the use-after-free is a
# product finding and must not be swapped away by choosing another seed.
assets="$TMP/assets-fixed-timeout"
artifacts="$TMP/artifacts-fixed-timeout"
make_assets "$assets"
set +e
fixed_timeout_output="$(DEMO08_TEST_MODE=fixed-timeout \
  DEMO08_TEST_UAF_BIN="$TMP/planted-uaf" \
  run_prepare "$assets" "$artifacts" 1 2>&1)"
fixed_timeout_rc=$?
set -e
[ "$fixed_timeout_rc" -ne 0 ]
[ ! -e "$assets/.crash-seed" ]
grep -q 'fixed control on seed 0 ran past the 1s budget' <<<"$fixed_timeout_output"
grep -q 'unconfirmed=1' <<<"$fixed_timeout_output"

assets="$TMP/assets-fixed-uaf"
artifacts="$TMP/artifacts-fixed-uaf"
make_assets "$assets"
set +e
fixed_uaf_output="$(DEMO08_TEST_MODE=fixed-uaf \
  DEMO08_TEST_UAF_BIN="$TMP/planted-uaf" \
  run_prepare "$assets" "$artifacts" 1 2>&1)"
fixed_uaf_rc=$?
set -e
[ "$fixed_uaf_rc" -ne 0 ]
[ ! -e "$assets/.crash-seed" ]
grep -q 'fixed variant reported a use-after-free on seed 0' <<<"$fixed_uaf_output"

# A UAF signature without the progress-thread witness does not qualify a seed.
assets="$TMP/assets-unbound-uaf"
artifacts="$TMP/artifacts-unbound-uaf"
make_assets "$assets"
set +e
unbound_output="$(DEMO08_TEST_MODE=uaf-no-engagement \
  DEMO08_TEST_UAF_BIN="$TMP/planted-uaf" \
  run_prepare "$assets" "$artifacts" 1 2>&1)"
unbound_rc=$?
set -e
[ "$unbound_rc" -ne 0 ]
grep -q 'NO-RESULT: path engagement 0/1' <<<"$unbound_output"
grep -q 'engagement=0/1 uaf_hits=1/1' <<<"$unbound_output"
[ ! -e "$assets/.crash-seed" ]

# A runner failure is an environment error, not evidence that the path ran
# without finding the UAF.
assets="$TMP/assets-runner-failure"
artifacts="$TMP/artifacts-runner-failure"
make_assets "$assets"
set +e
runner_failure_output="$(DEMO08_TEST_MODE=runner-failure \
  run_prepare "$assets" "$artifacts" 1 2>&1)"
runner_failure_rc=$?
set -e
[ "$runner_failure_rc" -ne 0 ]
grep -q 'never executed the guest: 0 of 1 seeds' <<<"$runner_failure_output"
! grep -q 'no ASAN UAF found' <<<"$runner_failure_output"

# Output text cannot turn a wrapper failure into a qualifying guest run, even
# when that output happens to contain both required signatures.
assets="$TMP/assets-runner-failure-with-signatures"
artifacts="$TMP/artifacts-runner-failure-with-signatures"
make_assets "$assets"
set +e
runner_failure_output="$(DEMO08_TEST_MODE=runner-failure-with-signatures \
  DEMO08_TEST_UAF_BIN="$TMP/planted-uaf" \
  run_prepare "$assets" "$artifacts" 1 2>&1)"
runner_failure_rc=$?
set -e
[ "$runner_failure_rc" -ne 0 ]
grep -q 'never executed the guest: 0 of 1 seeds' <<<"$runner_failure_output"
[ ! -e "$assets/.crash-seed" ]

# A fixture-bound cache is a selection hint, not evidence. Replay must produce
# the same qualifying execution, path engagement, and ASAN diagnostic.
assets="$TMP/assets-cached"
artifacts="$TMP/artifacts-cached"
make_assets "$assets"
fixture=$(sha256sum "$assets/buggy/btrfs-convert" | cut -d' ' -f1)
printf '1 %s\n' "$fixture" >"$assets/.crash-seed"
cached_output="$(DEMO08_TEST_MODE=planted-uaf \
  DEMO08_TEST_UAF_SEED=1 DEMO08_TEST_UAF_BIN="$TMP/planted-uaf" \
  run_prepare "$assets" "$artifacts" 3 2>&1)"
grep -q 'engagement=1/1 uaf_hits=1/1 qualified=1/1 executed=1/1' <<<"$cached_output"
grep -q 'Demo 8 crash seed replayed: cached seed 1' <<<"$cached_output"
! grep -q 'Demo 8 crash seed calibrated:' <<<"$cached_output"
printf '%s\n' "$cached_output" >"$TMP/cached.log"
[ "$("$CLASSIFIER" --log "$TMP/cached.log" --force-cold false)" = cached-seed-replay ]
set +e
forced_cached="$("$CLASSIFIER" --log "$TMP/cached.log" --force-cold true 2>/dev/null)"
forced_cached_rc=$?
set -e
[ "$forced_cached_rc" -ne 0 ]
[ "$forced_cached" = cached-seed-replay ]
[ "$(cat "$assets/.crash-seed")" = "1 $fixture" ]
grep -qE $'^1\tcached\tbuggy\treached\tcomplete\t134\t[0-9]+\tyes\t' "$artifacts/calibration.tsv"
grep -qE $'^1\tcached\tbuggy-replay\treached\tcomplete\t134\t[0-9]+\tyes\t' \
  "$artifacts/calibration.tsv"
grep -qE $'^1\tcached\tfixed\treached\tnone\t0\t[0-9]+\tn/a\t' "$artifacts/calibration.tsv"
[ "$(wc -l <"$artifacts/calibration.tsv")" -eq 4 ]
grep -q 'AddressSanitizer: heap-use-after-free' \
  "$artifacts/calibration-cached-seed-1.out"

# A fixture-bound cached seed whose replay never executes is refused rather
# than trusted as evidence from an earlier run.
assets="$TMP/assets-cached-refused"
artifacts="$TMP/artifacts-cached-refused"
make_assets "$assets"
fixture=$(sha256sum "$assets/buggy/btrfs-convert" | cut -d' ' -f1)
printf '1 %s\n' "$fixture" >"$assets/.crash-seed"
set +e
cached_refused_output="$(DEMO08_TEST_MODE=runner-failure \
  run_prepare "$assets" "$artifacts" 1 2>&1)"
cached_refused_rc=$?
set -e
[ "$cached_refused_rc" -ne 0 ]
grep -q 'never executed the guest: 0 of 2 seeds' <<<"$cached_refused_output"
grep -q $'^1\tcached\tbuggy\tdid-not-reach\tnone\t127\t' "$artifacts/calibration.tsv"
[ "$(wc -l <"$artifacts/calibration.tsv")" -eq 3 ]

# Missing and conflicting retained evidence are both refused by the exact
# classifier invoked from the workflow.
printf 'preparation completed without a marker\n' >"$TMP/no-evidence.log"
set +e
no_evidence="$("$CLASSIFIER" --log "$TMP/no-evidence.log" --force-cold false 2>/dev/null)"
no_evidence_rc=$?
set -e
[ "$no_evidence_rc" -ne 0 ]
[ "$no_evidence" = no-evidence ]

printf '%s\n%s\n' \
  'Demo 8 crash seed calibrated: 1' \
  'Demo 8 crash seed replayed: cached seed 1' >"$TMP/conflicting.log"
set +e
conflicting="$("$CLASSIFIER" --log "$TMP/conflicting.log" --force-cold false 2>/dev/null)"
conflicting_rc=$?
set -e
[ "$conflicting_rc" -ne 0 ]
[ "$conflicting" = conflicting-evidence ]

# Keep the workflow consumer on the tested helper, and retain preparation plus
# sweep output in the same append-only log under pipefail.
workflow="$ROOT/.github/workflows/demo-hot-path.yml"
grep -Fq 'scripts/prepare-demo08-assets.sh 2>&1 | tee -a "$log"' "$workflow"
grep -Fq '"${command[@]}" 2>&1 | tee -a "$log"' "$workflow"
grep -Fq 'actual="$(scripts/demo08-calibration-path.sh \' "$workflow"

echo 'PASS: Demo 8 calibration records engagement and detects a planted ASAN UAF'
