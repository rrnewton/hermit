#!/usr/bin/env bash
#
# Demo 4: schedule bisection (the slow finale).
#
# hermit analyze first finds passing and failing schedules, then bisects their
# event streams to identify the ordering that changes the outcome. This step
# runs the guest many times and REQUIRES user-accessible CPU performance
# counters (PMU). It can emit scheduler-desynchronization diagnostics while
# converging; a successful run ends with "Completed analysis successfully".

set -euo pipefail

# shellcheck disable=SC2034  # consumed by common.sh demo_success/demo_failure
DEMO_LABEL="Demo 4: Schedule Bisection"
cat <<'DESC'
=== Demo 4: Schedule Bisection ===

hermit analyze first finds passing and failing schedules, then bisects their
event streams to identify the ordering that changes the outcome. It builds a
debug guest so the report can resolve source locations. This is intentionally
the slow finale: it runs the guest many times, requires PMU access, and can emit
scheduler-desynchronization diagnostics while converging. A successful run ends
with "Completed analysis successfully".
DESC

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

export PYTHON="${PYTHON:-/usr/bin/python3}"

demo_banner "Build a debug guest so the report can resolve source locations"
( cd "$HERMIT_REPO" && cargo build -p hermetic_infra_hermit_flaky-tests --bin hello_race )
export HELLO_RACE_DEBUG="$HERMIT_REPO/target/debug/hello_race"
export ANALYSIS_REPORT="$DEMO_ARTIFACTS/hello-race-analysis.json"

demo_banner "Search and bisect schedules (needs PMU; up to 10 minutes)"
# Keep the output clean: --log=error suppresses WARN-level noise so the evolving
# edit distance is easy to follow. When the host lacks CPUID faulting, add
# --no-virtualize-cpuid to the inner runs so CPUID does not become a host input
# that desyncs record/replay ("Expected match before pop").
analyze_run_args=(--base-env=host)
if ! hermit_supports_cpuid_faulting; then
  echo "note: host lacks CPUID faulting; adding --no-virtualize-cpuid to analyze runs" >&2
  analyze_run_args+=(--no-virtualize-cpuid)
fi
analyze_run_flags=()
for arg in "${analyze_run_args[@]}"; do
  analyze_run_flags+=("--run-arg=$arg")
done
timeout 600 "$HERMIT" --log=error analyze \
  "${HERMIT_ANALYZE_TMP_FLAGS[@]}" \
  "${analyze_run_flags[@]}" \
  --report-file="$ANALYSIS_REPORT" \
  --analyze-seed=0 \
  --search -- \
  --chaos --summary --preemption-timeout=400000 -- \
  "$HELLO_RACE_DEBUG"

demo_banner "Report the two critical adjacent events"
"$PYTHON" -c 'import json,sys; d=json.load(open(sys.argv[1])); print(d["header"]); print("critical events:", d["critical_event1"]["event_index"], d["critical_event2"]["event_index"])' "$ANALYSIS_REPORT"

echo
echo "Event numbers can vary with the binary and Hermit revision; the source-level"
echo "diagnosis (the racy access in flaky-tests/hello_race.rs) is the durable result."
demo_success
