#!/usr/bin/env bash
#
# Demo 4: schedule bisection (the slow finale).
#
# hermit analyze first finds passing and failing schedules, then bisects their
# event streams to identify the ordering that changes the outcome. This step
# runs the guest many times and REQUIRES user-accessible CPU performance
# counters (PMU). It can emit scheduler-desynchronization diagnostics while
# converging; a successful run ends with "Completed analysis successfully".

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

export PYTHON="${PYTHON:-/usr/bin/python3}"

demo_banner "Build a debug guest so the report can resolve source locations"
( cd "$HERMIT_REPO" && cargo build -p hermetic_infra_hermit_flaky-tests --bin hello_race )
export HELLO_RACE_DEBUG="$HERMIT_REPO/target/debug/hello_race"
export ANALYSIS_REPORT="$DEMO_ARTIFACTS/hello-race-analysis.json"

demo_banner "Search and bisect schedules (needs PMU; up to 10 minutes)"
timeout 600 "$HERMIT" analyze \
  "${HERMIT_ANALYZE_TMP_FLAGS[@]}" \
  --run-arg=--base-env=host \
  --report-file="$ANALYSIS_REPORT" \
  --analyze-seed=0 \
  --search -- \
  --chaos --summary --preemption-timeout=400000 -- \
  "$HELLO_RACE_DEBUG"

demo_banner "Report the two critical adjacent events"
"$PYTHON" -c 'import json,sys; d=json.load(open(sys.argv[1])); print(d["header"]); print("critical events:", d["critical_event1"]["event_index"], d["critical_event2"]["event_index"])' "$ANALYSIS_REPORT"

echo
echo "Demo 4 complete. Event numbers can vary with the binary and Hermit revision;"
echo "the source-level diagnosis (the racy access in flaky-tests/hello_race.rs) is"
echo "the durable result."
