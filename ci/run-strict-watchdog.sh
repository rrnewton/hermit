#!/usr/bin/env bash
set -uo pipefail

runner=$1
dag=$2
sel=$3
jobs=$4
perf_dir=$5
raw_log=$6
phase_log=$7
shift 7

set +e
"$runner" run --dag "$dag" --only "$sel" -j "$jobs" --perf-dir "$perf_dir" \
    "$@" -v 2>&1 |
    tee "$raw_log" |
    TZ=UTC gawk -v out="$phase_log" '
        {
            stamp = strftime("%Y-%m-%dT%H:%M:%SZ", systime())
            print stamp, $0 >> out
            fflush(out)
        }
    '
statuses=("${PIPESTATUS[@]}")
set -e

if (( statuses[1] != 0 || statuses[2] != 0 )); then
    echo "run-strict-watchdog.sh: logger pipeline failed: tee=${statuses[1]} gawk=${statuses[2]}" >&2
    exit 125
fi
exit "${statuses[0]}"
