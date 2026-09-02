#!/usr/bin/env bash
# Bracket Demo 8 calibration accounting with no-engagement and real-ASAN-UAF fixtures.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PREP="$ROOT/scripts/prepare-demo08-assets.sh"
CLASSIFIER="$ROOT/scripts/demo08-calibration-path.sh"
TMP="$(mktemp -d -t demo08-calibration-test.XXXXXX)"
trap 'rm -rf -- "$TMP"' EXIT

make_assets() {
  local assets="$1"
  mkdir -p "$assets/buggy" "$assets/fixed"
  printf '#!/usr/bin/env bash\nexit 0\n' >"$assets/buggy/btrfs-convert"
  printf '#!/usr/bin/env bash\nexit 0\n' >"$assets/fixed/btrfs-convert"
  chmod +x "$assets/buggy/btrfs-convert" "$assets/fixed/btrfs-convert"
  : >"$assets/pop-tiny.img"
  printf '%s\n' \
    'prep=1 btrfs=4ab0e80be9e3bb1db2e6038e6d4316d35fb7ba8b' \
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
    if [ "$seed" = "${DEMO08_TEST_UAF_SEED:?UAF seed required}" ]; then
      ASAN_OPTIONS=detect_leaks=0:abort_on_error=1 \
        "${DEMO08_TEST_UAF_BIN:?UAF binary required}"
    else
      echo 'Conversion complete'
    fi
    ;;
  uaf-no-engagement)
    ASAN_OPTIONS=detect_leaks=0:abort_on_error=1 \
      "${DEMO08_TEST_UAF_BIN:?UAF binary required}"
    ;;
  runner-failure)
    echo 'fixture runner failed before guest execution' >&2
    exit 127
    ;;
  runner-failure-with-signatures)
    printf 'Copy inodes [o] [         0/         1]\r\n'
    ASAN_OPTIONS=detect_leaks=0:abort_on_error=1 \
      "${DEMO08_TEST_UAF_BIN:?UAF binary required}" || true
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
grep -q $'^1\tcold\treached\thit\t' "$artifacts/calibration.tsv"
grep -q 'AddressSanitizer: heap-use-after-free' \
  "$artifacts/calibration-cold-seed-1.out"

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
grep -q 'engagement=1/1 uaf_hits=1/1 executed=1/1' <<<"$cached_output"
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
grep -q $'^1\tcached\treached\thit\t' "$artifacts/calibration.tsv"
[ "$(wc -l <"$artifacts/calibration.tsv")" -eq 2 ]
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
grep -q $'^1\tcached\tdid-not-reach\tnone\t127\t' "$artifacts/calibration.tsv"
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
