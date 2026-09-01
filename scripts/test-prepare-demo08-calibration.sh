#!/usr/bin/env bash
# Bracket Demo 8 calibration accounting: no-engagement, engaged-but-no-hit, a real ASAN
# use-after-free, an honoured cached seed, a STALE cached seed, and the preference for a
# complete report over a truncated one.
#
# SCOPE. These brackets drive the harness through DEMO08_CALIBRATION_RUNNER, which
# substitutes the guest. They exercise this harness's bookkeeping and its failure controls;
# they do NOT exercise the Hermit path, and passing here is not evidence that Demo 8
# reproduces anything. The planted-UAF fixture below is a real ASAN binary so that the
# detector itself is held honest, with a positive control on that control.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PREP="$ROOT/scripts/prepare-demo08-assets.sh"
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

# The identity the harness will compute for these stub assets, so a cached record can be
# written that the harness accepts as being about THIS fixture.
fixture_hash() {
  sha256sum "$1/buggy/btrfs-convert" | cut -d' ' -f1
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
# ASAN RUNTIME DISCOVERY. `-fsanitize=address` resolves to a linker script that names a
# versioned libasan; on some hosts that soname is absent from the default library path while
# the runtime is installed under another prefix, so a plain link fails BEFORE the harness is
# ever exercised. Derive the directory instead of hardcoding one: a literal host path would
# be wrong on any other machine and is a portability-lint violation. The rpath matters as
# much as the -L -- without it the fixture LINKS but cannot RUN, which yields a passing build
# and a silently unexecuted check, i.e. exactly the fake-green this test exists to refuse.
asan_ld=()
if ! gcc -fsanitize=address -o "$TMP/asan-probe" -xc - >/dev/null 2>&1 <<<'int main(void){return 0;}'; then
  # `|| true` on both: `set -o pipefail` plus `head -1` closing the pipe makes find/grep exit
  # non-zero on success, which under `set -e` would abort during the assignment itself.
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

# POSITIVE CONTROL ON THE CONTROL. Before using the planted UAF to prove the sweep can fail,
# prove the fixture itself actually reports one. A fixture that silently stopped detecting
# would make every downstream "sweep can fail" claim vacuous and look identical to success.
control_out=$("$TMP/planted-uaf" 2>&1 || true)
if ! printf '%s' "$control_out" | grep -q 'heap-use-after-free'; then
  echo "SKIP-REFUSED: the planted-UAF fixture did not report heap-use-after-free;" >&2
  echo "  the failure control is inert, so nothing below could prove the sweep can fail." >&2
  echo "  fixture emitted ${#control_out} byte(s):" >&2
  printf '%s\n' "$control_out" | head -5 | sed 's/^/    /' >&2
  exit 1
fi

cat >"$TMP/runner.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
seed="${1:?seed required}"
banner() { echo 'btrfs-convert from btrfs-progs v7.1'; }
case "${DEMO08_TEST_MODE:?mode required}" in
  no-engagement)
    banner
    echo 'fixture exited before progress-thread path'
    ;;
  engaged-no-hit)
    banner
    printf 'Copy inodes [o] [         0/         1]\r\n'
    echo 'Conversion complete'
    ;;
  planted-uaf)
    banner
    printf 'Copy inodes [o] [         0/         1]\r\n'
    if [ "$seed" = "${DEMO08_TEST_UAF_SEED:?UAF seed required}" ]; then
      ASAN_OPTIONS=detect_leaks=0:abort_on_error=1 \
        "${DEMO08_TEST_UAF_BIN:?UAF binary required}"
    else
      echo 'Conversion complete'
    fi
    ;;
  truncated-then-complete)
    # Seed 0 reports the UAF but the process dies before ASAN writes frames or SUMMARY --
    # the real shape observed at seeds 3 and 13 on Hermit head 00ed139b. Seed 2 reports a
    # complete one. The harness must hold the truncated seed and prefer the complete seed.
    banner
    printf 'Copy inodes [o] [         0/         1]\r\n'
    if [ "$seed" = 0 ]; then
      echo '==3==ERROR: AddressSanitizer: heap-use-after-free on address 0x60600000 at pc 0x4e6'
    elif [ "$seed" = 2 ]; then
      ASAN_OPTIONS=detect_leaks=0:abort_on_error=1 \
        "${DEMO08_TEST_UAF_BIN:?UAF binary required}"
    else
      echo 'Conversion complete'
    fi
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
    HERMIT_RELEASE="$TMP/not-used-hermit" \
    "$PREP"
}

# A failing assertion in this harness used to exit with NO output at all, because the
# subject's output is captured into a variable and the failing grep just exits under `set -e`.
# A refusal that does not say what it saw sends the next reader back to reproduce it by hand.
check() {
  local what="$1" haystack="$2" needle="$3"
  if ! grep -q -- "$needle" <<<"$haystack"; then
    echo "FAIL [$what]: expected to find: $needle" >&2
    echo "--- actual output ---" >&2
    printf '%s\n' "$haystack" >&2
    echo "--- end ---" >&2
    exit 1
  fi
}

check_not() {
  local what="$1" haystack="$2" needle="$3"
  if grep -q -- "$needle" <<<"$haystack"; then
    echo "FAIL [$what]: expected NOT to find: $needle" >&2
    echo "--- actual output ---" >&2
    printf '%s\n' "$haystack" >&2
    echo "--- end ---" >&2
    exit 1
  fi
}

equals() {
  local what="$1" got="$2" want="$3"
  if [ "$got" != "$want" ]; then
    echo "FAIL [$what]: got '$got', want '$want'" >&2
    exit 1
  fi
}

# --- 1. Negative bracket: two attempted seeds, no path engagement. -------------------------
# This must be a refused NO-RESULT, with both per-seed outputs and rows retained. "Nothing
# reached the race" is a statement about this machine, not about the fixture.
assets="$TMP/assets-no-engagement"; artifacts="$TMP/artifacts-no-engagement"
make_assets "$assets"
set +e
out="$(DEMO08_TEST_MODE=no-engagement run_prepare "$assets" "$artifacts" 2 2>&1)"
rc=$?
set -e
equals "no-engagement exits nonzero" "$((rc != 0))" 1
check "no-engagement NO-RESULT" "$out" 'NO-RESULT: path engagement 0/2'
equals "no-engagement tsv rows" "$(wc -l <"$artifacts/calibration.tsv")" 3
equals "no-engagement did-not-reach rows" \
  "$(grep -c $'\tdid-not-reach\t' "$artifacts/calibration.tsv")" 2
equals "no-engagement retained outputs" \
  "$(find "$artifacts" -maxdepth 1 -name 'calibration-cold-seed-*.out' | wc -l)" 2

# --- 2. Engaged without a hit is evidence-bearing and must NOT be mislabeled NO-RESULT. ----
assets="$TMP/assets-engaged"; artifacts="$TMP/artifacts-engaged"
make_assets "$assets"
set +e
out="$(DEMO08_TEST_MODE=engaged-no-hit run_prepare "$assets" "$artifacts" 2 2>&1)"
rc=$?
set -e
equals "engaged-no-hit exits nonzero" "$((rc != 0))" 1
check "engaged-no-hit counts" "$out" 'engagement=2/2 uaf_hits=0/2'
check_not "engaged-no-hit is not NO-RESULT" "$out" 'NO-RESULT'
equals "engaged-no-hit rows" "$(grep -c $'\treached\tnone\t' "$artifacts/calibration.tsv")" 2

# --- 3. Falsifiability: seed 1 runs an actual ASAN use-after-free. -------------------------
assets="$TMP/assets-uaf"; artifacts="$TMP/artifacts-uaf"
make_assets "$assets"
out="$(DEMO08_TEST_MODE=planted-uaf DEMO08_TEST_UAF_SEED=1 DEMO08_TEST_UAF_BIN="$TMP/planted-uaf" \
  run_prepare "$assets" "$artifacts" 3 2>&1)"
check "uaf counts" "$out" 'engagement=2/2 uaf_hits=1/2'
equals "uaf selected seed" "$(cut -d' ' -f1 <"$assets/.crash-seed")" 1
equals "uaf records fixture" "$(cut -s -d' ' -f2 <"$assets/.crash-seed")" "$(fixture_hash "$assets")"
check "uaf tsv row" "$(cat "$artifacts/calibration.tsv")" $'^1\tcold\treached\thit\t'
check "uaf report retained" "$(cat "$artifacts/calibration-cold-seed-1.out")" \
  'AddressSanitizer: heap-use-after-free'

# --- 4. A cached seed is REPLAYED, not trusted because the file exists. --------------------
# The evidence denominator is therefore 1/1 rather than an unmeasured cache hit.
assets="$TMP/assets-cached"; artifacts="$TMP/artifacts-cached"
make_assets "$assets"
printf '1 %s deadbeef\n' "$(fixture_hash "$assets")" >"$assets/.crash-seed"
out="$(DEMO08_TEST_MODE=planted-uaf DEMO08_TEST_UAF_SEED=1 DEMO08_TEST_UAF_BIN="$TMP/planted-uaf" \
  run_prepare "$assets" "$artifacts" 3 2>&1)"
check "cached replayed" "$out" 'engagement=1/1 uaf_hits=1/1'
check "cached tsv row" "$(cat "$artifacts/calibration.tsv")" $'^1\tcached\treached\thit\t'
equals "cached ran exactly one seed" "$(wc -l <"$artifacts/calibration.tsv")" 2

# --- 5. A STALE cached seed is DETECTED, not reported as a regression. ---------------------
# This is the control for the defect the whole seed record exists to prevent: the cached seed
# names 0, only seed 1 reproduces, and the fixture is unchanged. The harness must say the
# SEED is stale, must not call it a regression, and must re-derive and land on seed 1.
assets="$TMP/assets-stale"; artifacts="$TMP/artifacts-stale"
make_assets "$assets"
printf '0 %s deadbeef\n' "$(fixture_hash "$assets")" >"$assets/.crash-seed"
out="$(DEMO08_TEST_MODE=planted-uaf DEMO08_TEST_UAF_SEED=1 DEMO08_TEST_UAF_BIN="$TMP/planted-uaf" \
  run_prepare "$assets" "$artifacts" 3 2>&1)"
check "stale is announced" "$out" 'STALE CACHED SEED'
check "stale names the seed" "$out" 'seed 0 no longer reproduces the UAF here'
check "stale exonerates the fixture" "$out" 'NOT a regression'
check "stale reports the changed hermit" "$out" 'Hermit binary also changed'
check "stale recalibrates" "$out" 'Demo 8 crash seed calibrated: 1'
equals "stale re-derived seed" "$(cut -d' ' -f1 <"$assets/.crash-seed")" 1
check "stale kept the cached miss row" "$(cat "$artifacts/calibration.tsv")" \
  $'^0\tcached\treached\tnone\t'
check "stale kept the cold hit row" "$(cat "$artifacts/calibration.tsv")" $'^1\tcold\treached\thit\t'

# --- 6. A complete report is preferred over a truncated one. -------------------------------
# ASAN can report the UAF on a thread whose process still exits before the frames are
# written. Such a seed is a real detection but a weak demonstration -- the demo's replay step
# compares frames the truncated report does not carry -- so the sweep holds it and keeps
# looking. Seed 0 is truncated here; seed 2 is complete.
assets="$TMP/assets-truncated"; artifacts="$TMP/artifacts-truncated"
make_assets "$assets"
out="$(DEMO08_TEST_MODE=truncated-then-complete DEMO08_TEST_UAF_BIN="$TMP/planted-uaf" \
  run_prepare "$assets" "$artifacts" 4 2>&1)"
check "truncated seed held" "$out" 'seed 0 reported the UAF but the report is truncated'
check "complete seed selected" "$out" 'Demo 8 crash seed calibrated: 2 (complete ASAN report'
equals "selected the complete seed" "$(cut -d' ' -f1 <"$assets/.crash-seed")" 2
check "truncated seed still recorded as a hit" "$(cat "$artifacts/calibration.tsv")" \
  $'^0\tcold\treached\thit\t'

echo 'PASS: Demo 8 calibration records engagement, detects a planted ASAN UAF, replays a'
echo '      cached seed, reports a stale seed as stale, and prefers a complete report.'
