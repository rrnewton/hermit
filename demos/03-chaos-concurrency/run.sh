#!/usr/bin/env bash
#
# Demo 3: chaos concurrency testing.
#
# hello_race contains an intentional data race. Chaos mode makes scheduler
# choices with a seeded PRNG, so different seeds explore different interleavings
# and the same seed reproduces the same result. A recorded schedule artifact
# reproduces an exact failure without relying only on the seed.

set -euo pipefail

# shellcheck source=demos/lib/display.sh
source "$(dirname "${BASH_SOURCE[0]}")/../lib/display.sh"

# shellcheck disable=SC2034  # consumed by common.sh demo_success/demo_failure
DEMO_LABEL="Demo 3: Chaos Concurrency Testing"
demo_header "$DEMO_LABEL"
echo 'hello_race contains an intentional data race. Chaos mode makes scheduler'
echo 'choices with a seeded PRNG, so different seeds explore different interleavings'
echo 'and the same seed reproduces the same result. Seed 1 passes; seed 0 reaches the'
echo "antagonistic schedule and returns the guest's expected failure status. The demo"
echo 'surveys seeds 0-15, then records a failing schedule to an artifact and replays'
echo 'that exact schedule, confirming the outputs match.'
echo ''
echo '=========================================='

# shellcheck source=demos/lib/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/../lib/common.sh"

# hello_race's two outcomes. It exits 0 after printing PASS_LINE when the
# second thread's store lands last, and exits 1 after printing FAIL_LINE when
# the first thread's store lands last. Any other status or output means the run
# did not reach either outcome (for example Hermit failed), and the demo stops.
PASS_LINE='Did not find antagonistic schedule. Succeeding.'
FAIL_LINE='Antagonistic schedule reached, failing.'
# The seeds among 0-15 that reach the failing order with this demo's flags.
EXPECTED_FAILING_SEEDS='0 5 6 12 15'

# check_outcome LABEL STATUS OUTPUT_FILE EXPECTED_STATUS EXPECTED_LINE
check_outcome() {
  local label="$1" status="$2" output="$3" want_status="$4" want_line="$5"
  if [ ! -s "$output" ]; then
    echo "$label: produced no output (exit status $status)" >&2
    exit 1
  fi
  if [ "$status" -ne "$want_status" ]; then
    echo "$label: exit status $status, expected $want_status" >&2
    cat "$output" >&2
    exit 1
  fi
  if ! grep -Fxq -- "$want_line" "$output"; then
    echo "$label: output lacks the line: $want_line" >&2
    cat "$output" >&2
    exit 1
  fi
}

demo_banner "Seed 1 passes; seed 0 reproduces the expected failure"
status=0
chaos_run 1 >"$DEMO_TMP/seed-1.txt" || status=$?
cat "$DEMO_TMP/seed-1.txt"
check_outcome "seed 1" "$status" "$DEMO_TMP/seed-1.txt" 0 "$PASS_LINE"

status=0
chaos_run 0 >"$DEMO_TMP/seed-0.txt" || status=$?
cat "$DEMO_TMP/seed-0.txt"
check_outcome "seed 0" "$status" "$DEMO_TMP/seed-0.txt" 1 "$FAIL_LINE"
echo 'seed 0 reproduced the expected concurrency failure'

demo_banner "Survey seeds 0..15, retaining each run's output"
failing_seeds=()
for seed in $(seq 0 15); do
  status=0
  chaos_run "$seed" >"$DEMO_TMP/chaos-$seed.txt" || status=$?
  case "$status" in
    0)
      check_outcome "seed $seed" "$status" "$DEMO_TMP/chaos-$seed.txt" 0 "$PASS_LINE"
      result=pass
      ;;
    1)
      check_outcome "seed $seed" "$status" "$DEMO_TMP/chaos-$seed.txt" 1 "$FAIL_LINE"
      result=fail
      failing_seeds+=("$seed")
      ;;
    *)
      echo "seed $seed: exit status $status, expected 0 (pass) or 1 (fail)" >&2
      cat "$DEMO_TMP/chaos-$seed.txt" >&2
      exit 1
      ;;
  esac
  printf 'seed=%s result=%s\n' "$seed" "$result"
done
if [ "${failing_seeds[*]}" != "$EXPECTED_FAILING_SEEDS" ]; then
  echo "failing seeds were: ${failing_seeds[*]}; expected: $EXPECTED_FAILING_SEEDS" >&2
  exit 1
fi
# The same seed must give the same output as it did in the first step.
for seed in 0 1; do
  if ! cmp -s "$DEMO_TMP/seed-$seed.txt" "$DEMO_TMP/chaos-$seed.txt"; then
    echo "seed $seed gave different output on its second run" >&2
    exit 1
  fi
done
echo "failing seeds: ${failing_seeds[*]}; seeds 0 and 1 repeated their first output byte for byte"

demo_banner "Save and replay the failing schedule"
export CHAOS_SCHEDULE="$DEMO_ARTIFACTS/hello-race-schedule.json"

# Both runs must reach the failing order: exit status 1 and FAIL_LINE.
status=0
hermit --log=error run \
  "${HERMIT_TMP_FLAGS[@]}" \
  --chaos --seed=0 \
  --base-env=minimal \
  --no-virtualize-cpuid \
  --max-timeslice=disabled \
  --env=HERMIT_MODE=chaos \
  --record-preemptions-to="$CHAOS_SCHEDULE" \
  -- "$HELLO_RACE" >"$DEMO_TMP/chaos-recorded.txt" || status=$?
check_outcome "recording the failing schedule" "$status" \
  "$DEMO_TMP/chaos-recorded.txt" 1 "$FAIL_LINE"
test -s "$CHAOS_SCHEDULE"

status=0
hermit --log=error run \
  "${HERMIT_TMP_FLAGS[@]}" \
  --chaos \
  --base-env=minimal \
  --no-virtualize-cpuid \
  --max-timeslice=disabled \
  --env=HERMIT_MODE=chaos \
  --replay-preemptions-from="$CHAOS_SCHEDULE" \
  -- "$HELLO_RACE" >"$DEMO_TMP/chaos-replayed.txt" || status=$?
check_outcome "replaying the failing schedule" "$status" \
  "$DEMO_TMP/chaos-replayed.txt" 1 "$FAIL_LINE"
cmp "$DEMO_TMP/chaos-recorded.txt" "$DEMO_TMP/chaos-replayed.txt"
echo 'recorded and replayed runs both failed, with identical output:'
cat "$DEMO_TMP/chaos-replayed.txt"

demo_success
