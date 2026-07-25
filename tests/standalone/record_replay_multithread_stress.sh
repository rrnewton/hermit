#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Record ten multithreaded guests once, then replay each recording repeatedly.
# Every replay must preserve exit status and byte-identical stdout/stderr.
# Build the guests first with:
#   cargo build -p hermetic_infra_hermit_tests --release --bins

set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
readonly ROOT_DIR
readonly HERMIT_BIN="${HERMIT_BIN:-$ROOT_DIR/target/release/hermit}"
readonly REPLAYS="${REPLAYS:-5}"
readonly PHASE_TIMEOUT_SECONDS="${PHASE_TIMEOUT_SECONDS:-60}"
readonly KEEP_SUCCESS_ARTIFACTS="${KEEP_SUCCESS_ARTIFACTS:-0}"

if [[ ! $REPLAYS =~ ^[1-9][0-9]*$ ]]; then
    echo "REPLAYS must be a positive integer" >&2
    exit 2
fi
if [[ ! $PHASE_TIMEOUT_SECONDS =~ ^[1-9][0-9]*$ ]]; then
    echo "PHASE_TIMEOUT_SECONDS must be a positive integer" >&2
    exit 2
fi
if [[ ! -x $HERMIT_BIN ]]; then
    echo "Hermit binary is not executable: $HERMIT_BIN" >&2
    exit 2
fi

readonly WORK_DIR="${WORK_DIR:-$(mktemp -d /tmp/hermit-rr-multithread.XXXXXX)}"
FAILED=0

function cleanup {
    if ((FAILED == 1 || KEEP_SUCCESS_ARTIFACTS == 1)); then
        echo "Artifacts: $WORK_DIR" >&2
    else
        rm -rf -- "$WORK_DIR"
    fi
}
trap cleanup EXIT

readonly -a PROGRAMS=(
    "clock-total-order|$ROOT_DIR/target/release/rustbin_clock_total_order"
    "exit-group|$ROOT_DIR/target/release/rustbin_exit_group"
    "sched-yield|$ROOT_DIR/target/release/rustbin_sched_yield"
    "futex-and-print|$ROOT_DIR/target/release/rustbin_futex_and_print"
    "futex-wait-child|$ROOT_DIR/target/release/rustbin_futex_wait_child"
    "futex-wake-some|$ROOT_DIR/target/release/rustbin_futex_wake_some"
    "pipe-basics|$ROOT_DIR/target/release/rustbin_pipe_basics"
    "poll-spin|$ROOT_DIR/target/release/rustbin_poll_spin"
    "print-nanosleep-race|$ROOT_DIR/target/release/rustbin_print_nanosleep_race"
    "thread-random|$ROOT_DIR/target/release/rustbin_thread_random"
)

readonly OUTER_TIMEOUT_SECONDS=$((PHASE_TIMEOUT_SECONDS + 10))
record_extra_args=()
if "$HERMIT_BIN" record start --help | grep -q -- '--chaos'; then
    record_extra_args+=(--chaos)
    echo "Record chaos option detected: enabled"
else
    echo "Record chaos option unavailable: testing repeated replay of strict recordings"
fi
echo "Context: backend=ptrace log=info relaxations=none"

mkdir -p -- "$WORK_DIR"

function fail_case {
    local label=$1
    local message=$2
    FAILED=1
    echo "FAIL $label: $message" >&2
    exit 1
}

for entry in "${PROGRAMS[@]}"; do
    IFS='|' read -r label program <<<"$entry"
    if [[ ! -x $program ]]; then
        fail_case "$label" "guest binary is not executable: $program"
    fi

    case_dir="$WORK_DIR/$label"
    data_dir="$case_dir/recording"
    mkdir -p -- "$case_dir"

    record_stdout="$case_dir/record.stdout"
    record_stderr="$case_dir/record.stderr"
    if timeout --signal=TERM --kill-after=5 "$OUTER_TIMEOUT_SECONDS" \
        "$HERMIT_BIN" --log info --log-file "$case_dir/record.log" \
        record start "${record_extra_args[@]}" \
        --record-timeout "$PHASE_TIMEOUT_SECONDS" \
        --data-dir "$data_dir" -- "$program" \
        >"$record_stdout" 2>"$record_stderr"; then
        record_status=0
    else
        record_status=$?
    fi
    if ((record_status != 0)); then
        fail_case "$label" "record exited $record_status"
    fi

    reference_stdout=""
    reference_stderr=""
    for replay_index in $(seq 1 "$REPLAYS"); do
        replay_stdout="$case_dir/replay-$replay_index.stdout"
        replay_stderr="$case_dir/replay-$replay_index.stderr"
        if timeout --signal=TERM --kill-after=5 "$OUTER_TIMEOUT_SECONDS" \
            "$HERMIT_BIN" --log info \
            --log-file "$case_dir/replay-$replay_index.log" \
            replay --autopilot --data-dir "$data_dir" \
            >"$replay_stdout" 2>"$replay_stderr"; then
            replay_status=0
        else
            replay_status=$?
        fi
        if ((replay_status != record_status)); then
            fail_case "$label" \
                "replay $replay_index exited $replay_status; record exited $record_status"
        fi

        if ((replay_index == 1)); then
            reference_stdout=$replay_stdout
            reference_stderr=$replay_stderr
            if ! cmp -s -- "$record_stdout" "$reference_stdout"; then
                diff -u -- "$record_stdout" "$reference_stdout" || true
                fail_case "$label" "record stdout differs from replay 1"
            fi
        else
            if ! cmp -s -- "$reference_stdout" "$replay_stdout"; then
                diff -u -- "$reference_stdout" "$replay_stdout" || true
                fail_case "$label" "replay $replay_index stdout differs from replay 1"
            fi
            if ! cmp -s -- "$reference_stderr" "$replay_stderr"; then
                diff -u -- "$reference_stderr" "$replay_stderr" || true
                fail_case "$label" "replay $replay_index stderr differs from replay 1"
            fi
        fi
    done

    stdout_digest=$(sha256sum "$reference_stdout" | cut -d' ' -f1)
    stderr_digest=$(sha256sum "$reference_stderr" | cut -d' ' -f1)
    printf 'PASS %-24s replays=%s stdout=%s stderr=%s\n' \
        "$label" "$REPLAYS" "$stdout_digest" "$stderr_digest"
done

printf 'SUMMARY programs=%s replays-per-program=%s total-replays=%s mismatches=0\n' \
    "${#PROGRAMS[@]}" "$REPLAYS" "$((${#PROGRAMS[@]} * REPLAYS))"
