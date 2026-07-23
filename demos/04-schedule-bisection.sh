#!/usr/bin/env bash
#
# Demo 4: schedule bisection (the slow finale).
#
# hermit analyze first finds passing and failing schedules, then bisects their
# event streams to identify the ordering that changes the outcome. This step
# runs the guest many times and REQUIRES user-accessible CPU performance
# counters (PMU). It can emit scheduler-desynchronization diagnostics while
# converging; a successful run ends with "Completed analysis successfully".
#
# Verbosity: by default this demo shows only the evolving per-pass search
# progress lines and the final race localization, filtering out hermit
# analyze's convergence diagnostics. Set DEMO_VERBOSE=1 to see the full,
# unfiltered analyze output.

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

By default only the per-pass search progress and the final result are shown;
run with DEMO_VERBOSE=1 for the full analyze diagnostics.
DESC

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

export PYTHON="${PYTHON:-/usr/bin/python3}"

demo_banner "Build a debug guest so the report can resolve source locations"
( cd "$HERMIT_REPO" && cargo build -p hermetic_infra_hermit_flaky-tests --bin hello_race )
export HELLO_RACE_DEBUG="$HERMIT_REPO/target/debug/hello_race"
export ANALYSIS_REPORT="$DEMO_ARTIFACTS/hello-race-analysis.json"

demo_banner "Search and bisect schedules (needs PMU; up to 10 minutes)"
# hermit analyze writes its search progress AND its convergence diagnostics
# (endpoint verification, Needleman-Wunsch fallbacks, jitter checks, sub-event
# refinement skips, ...) straight to stderr with eprintln!, not through the log
# framework -- so --log=error and RUST_LOG do NOT quiet them. To keep the demo
# readable we filter that stderr stream down to the evolving per-pass progress
# lines plus the final race localization. Set DEMO_VERBOSE=1 to bypass the
# filter and see everything.
#
# When the host lacks CPUID faulting, add --no-virtualize-cpuid to the inner
# runs so CPUID does not become a host input that desyncs record/replay
# ("Expected match before pop").
analyze_run_args=(--base-env=host)
if ! hermit_supports_cpuid_faulting; then
  echo "note: host lacks CPUID faulting; adding --no-virtualize-cpuid to analyze runs" >&2
  analyze_run_args+=(--no-virtualize-cpuid)
fi
analyze_run_flags=()
for arg in "${analyze_run_args[@]}"; do
  analyze_run_flags+=("--run-arg=$arg")
done

# Allowlist of analyze stderr lines to keep in the default (quiet) view: the
# per-pass search progress and the final race localization. NO_COLOR=1 forces
# these markers to plain text so the match is stable regardless of whether
# stderr is a TTY.
demo_analyze_keep='^:: Event-Level Search Pass |^:: Completed analysis successfully|^:: Critical events found|^:: Critical branch boundary|^Critical event index '

run_analyze() {
  NO_COLOR=1 timeout 600 "$HERMIT" --log=error analyze \
    "${HERMIT_ANALYZE_TMP_FLAGS[@]}" \
    "${analyze_run_flags[@]}" \
    --report-file="$ANALYSIS_REPORT" \
    --analyze-seed=0 \
    --search -- \
    --chaos --summary --preemption-timeout=400000 -- \
    "$HELLO_RACE_DEBUG"
}

if [ "${DEMO_VERBOSE:-0}" = "1" ]; then
  run_analyze
else
  # Filter only stderr (the noisy stream). stdout carries nothing the demo
  # needs -- the report is written to --report-file -- and the analyze exit
  # status is preserved so `set -e` still catches a genuine failure.
  run_analyze 2> >(grep --line-buffered -E "$demo_analyze_keep" >&2)
fi

demo_banner "Report the two critical adjacent events"
"$PYTHON" -c 'import json,sys; d=json.load(open(sys.argv[1])); print(d["header"]); print("critical events:", d["critical_event1"]["event_index"], d["critical_event2"]["event_index"])' "$ANALYSIS_REPORT"

echo
echo "Event numbers can vary with the binary and Hermit revision; the source-level"
echo "diagnosis (the racy access in flaky-tests/hello_race.rs) is the durable result."
demo_success
