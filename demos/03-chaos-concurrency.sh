#!/usr/bin/env bash
#
# Demo 3: chaos concurrency testing.
#
# hello_race contains an intentional data race. Chaos mode makes scheduler
# choices with a seeded PRNG, so different seeds explore different interleavings
# and the same seed reproduces the same result. A recorded schedule artifact
# reproduces an exact failure without relying only on the seed.

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

demo_banner "Seed 1 passes; seed 0 reproduces the expected failure"
chaos_run 1

if chaos_run 0; then
  echo 'unexpected pass for seed 0' >&2
  exit 1
else
  echo 'seed 0 reproduced the expected concurrency failure'
fi

demo_banner "Survey seeds 0..15, retaining each run's output"
for seed in $(seq 0 15); do
  if chaos_run "$seed" >"$DEMO_TMP/chaos-$seed.txt"; then
    result=pass
  else
    result=fail
  fi
  printf 'seed=%s result=%s\n' "$seed" "$result"
done

demo_banner "Save and replay the failing schedule"
export CHAOS_SCHEDULE="$DEMO_ARTIFACTS/hello-race-schedule.json"

# Both commands are expected to return the guest's failure status.
if "$HERMIT" --log=error run \
  --chaos --seed=0 \
  --base-env=minimal \
  --no-virtualize-cpuid \
  --preemption-timeout=disabled \
  --env=HERMIT_MODE=chaos \
  --record-preemptions-to="$CHAOS_SCHEDULE" \
  -- "$HELLO_RACE" >"$DEMO_TMP/chaos-recorded.txt"; then
  echo 'unexpected pass while recording the failing schedule' >&2
  exit 1
fi
test -s "$CHAOS_SCHEDULE"

if "$HERMIT" --log=error run \
  --chaos \
  --base-env=minimal \
  --no-virtualize-cpuid \
  --preemption-timeout=disabled \
  --env=HERMIT_MODE=chaos \
  --replay-preemptions-from="$CHAOS_SCHEDULE" \
  -- "$HELLO_RACE" >"$DEMO_TMP/chaos-replayed.txt"; then
  echo 'unexpected pass while replaying the failing schedule' >&2
  exit 1
fi
cmp "$DEMO_TMP/chaos-recorded.txt" "$DEMO_TMP/chaos-replayed.txt"

echo
echo "Demo 3 complete: the saved schedule reproduced the exact recorded failure."
