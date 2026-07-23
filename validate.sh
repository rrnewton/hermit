#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -uo pipefail

# Deny warnings for every compiler and rustdoc invocation while preserving any
# caller-provided flags.
export RUSTFLAGS="${RUSTFLAGS:+${RUSTFLAGS} }-D warnings"
export RUSTDOCFLAGS="${RUSTDOCFLAGS:+${RUSTDOCFLAGS} }-D warnings"
ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

readonly ROOT_DIR
cd "$ROOT_DIR" || exit 1

checks=0
failures=0
declare -a background_pids=()
declare -a background_names=()
declare -a background_logs=()
declare -a background_duration_files=()

VALIDATION_TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/hermit-validate.XXXXXX")
if [[ -z $VALIDATION_TMP_DIR ]]; then
    echo "Unable to create validation workspace." >&2
    exit 1
fi
readonly VALIDATION_TMP_DIR

LOG_FILE=$(mktemp "${TMPDIR:-/tmp}/hermit-validate.XXXXXX.log")
if [[ -z $LOG_FILE ]]; then
    echo "Unable to create validation log." >&2
    exit 1
fi
readonly LOG_FILE
printf "Hermit validation log\nRoot: %s\n\n" "$ROOT_DIR" >"$LOG_FILE"

# cargo-nextest version pinned by ensure_cargo_nextest (the actual nextest
# invocations live in scripts/test-suite.sh).
readonly NEXTEST_VERSION=0.9.100

function cleanup {
    local pid
    for pid in "${background_pids[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null || true
    rm -rf "$VALIDATION_TMP_DIR"
}

function interrupted {
    trap - INT TERM
    printf "❌ Validation interrupted (full log: %s)\n" "$LOG_FILE"
    exit 130
}
trap cleanup EXIT
trap interrupted INT TERM

function failure_summary {
    local output_start=$1
    local output
    local summary

    output=$(
        tail -n "+$output_start" "$LOG_FILE" |
            sed $'s/\033\\[[0-9;]*[[:alpha:]]//g; s/^[[:space:]]*//; s/[[:space:]][[:space:]]*/ /g'
    )
    summary=$(
        printf "%s\n" "$output" |
            grep -E '(^error(\[[^]]+\])?:|^FAIL:|^test result: FAILED|^failures:|panicked at|Unexpected .*:|differed between|timed out|command not found|No such file)' |
            tail -n 1
    ) || true

    if [[ -z $summary ]]; then
        summary=$(printf "%s\n" "$output" | sed '/^[[:space:]]*$/d' | tail -n 1)
    fi
    if [[ -z $summary ]]; then
        summary="command exited without an error message"
    elif ((${#summary} > 180)); then
        summary="${summary:0:177}..."
    fi
    printf "%s" "$summary"
}

function run_check {
    local name=$1
    shift

    local started_at=$SECONDS
    local output_start
    local status
    local summary

    {
        printf "=== %s ===\n" "$name"
        printf "Command:"
        printf " %q" "$@"
        printf "\n"
    } >>"$LOG_FILE"
    output_start=$(($(wc -l <"$LOG_FILE") + 1))

    if "$@" >>"$LOG_FILE" 2>&1; then
        status=0
        printf "✅ %s (1 passed, 0 failed, %ss)\n" \
            "$name" "$((SECONDS - started_at))"
    else
        status=$?
        failures=$((failures + 1))
        summary=$(failure_summary "$output_start")
        printf "❌ %s (0 passed, 1 failed, exit %s: %s; full log: %s)\n" \
            "$name" "$status" "$summary" "$LOG_FILE"
    fi

    {
        printf "Exit: %s\n" "$status"
        printf "Duration: %ss\n\n" "$((SECONDS - started_at))"
    } >>"$LOG_FILE"
    checks=$((checks + 1))
}

function start_check {
    local name=$1
    shift

    local index=${#background_pids[@]}
    local log_file="$VALIDATION_TMP_DIR/check-$index.log"
    local duration_file="$VALIDATION_TMP_DIR/check-$index.duration"

    (
        local started_at=$SECONDS
        local status

        printf "Command:"
        printf " %q" "$@"
        printf "\n"
        "$@"
        status=$?
        printf "%s\n" "$((SECONDS - started_at))" >"$duration_file"
        exit "$status"
    ) >"$log_file" 2>&1 &

    background_pids+=("$!")
    background_names+=("$name")
    background_logs+=("$log_file")
    background_duration_files+=("$duration_file")
    checks=$((checks + 1))
}

function wait_for_background_checks {
    local i
    for i in "${!background_pids[@]}"; do
        local pid=${background_pids[$i]}
        local name=${background_names[$i]}
        local log_file=${background_logs[$i]}
        local duration_file=${background_duration_files[$i]}
        local output_start
        local status
        local duration
        local summary

        if wait "$pid"; then
            status=0
        else
            status=$?
            failures=$((failures + 1))
        fi

        printf "=== %s ===\n" "$name" >>"$LOG_FILE"
        output_start=$(($(wc -l <"$LOG_FILE") + 1))
        cat "$log_file" >>"$LOG_FILE"
        if [[ -r $duration_file ]]; then
            duration=$(<"$duration_file")
        else
            duration=0
        fi

        if ((status == 0)); then
            printf "✅ %s (1 passed, 0 failed, %ss)\n" "$name" "$duration"
        else
            summary=$(failure_summary "$output_start")
            printf "❌ %s (0 passed, 1 failed, exit %s: %s; full log: %s)\n" \
                "$name" "$status" "$summary" "$LOG_FILE"
        fi
        {
            printf "Exit: %s\n" "$status"
            printf "Duration: %ss\n\n" "$duration"
        } >>"$LOG_FILE"
    done

    background_pids=()
    background_names=()
    background_logs=()
    background_duration_files=()
}

function ensure_cargo_nextest {
    if cargo nextest show-config version >/dev/null 2>&1; then
        return 0
    fi

    local -ar install_command=(
        cargo install cargo-nextest --locked --version "$NEXTEST_VERSION"
    )
    if command -v with-proxy >/dev/null 2>&1; then
        with-proxy "${install_command[@]}"
    else
        "${install_command[@]}"
    fi

    cargo nextest show-config version
}

function print_summary {
    local passed=$((checks - failures))
    if ((failures == 0)); then
        printf "✅ Validation summary (%s passed, 0 failed; full log: %s)\n" \
            "$passed" "$LOG_FILE"
    else
        printf "❌ Validation summary (%s passed, %s failed; full log: %s)\n" \
            "$passed" "$failures" "$LOG_FILE"
    fi
}

run_check "cargo-nextest available" ensure_cargo_nextest

# The test matrix itself lives in scripts/test-suite.sh, the single source of
# truth shared with CI (.github/workflows/ci.yml). We ask that script which
# tiers make up the local matrix and run each through the logging harness above:
#   validate.sh ──> scripts/test-suite.sh <── ci.yml
# Parallel-safe tiers (lint/docs) run in the background. Capability-gated tiers
# (PMU, mount namespaces, the rr submodule) are omitted from the local list with
# a notice when this host lacks the capability, so local validation still gives
# useful signal without falsely failing. CI runs the same tiers via `--portable`
# / `--hardware` and fails loudly when a required capability is missing.
readonly TEST_SUITE="$ROOT_DIR/scripts/test-suite.sh"
if [[ ! -x $TEST_SUITE ]]; then
    echo "❌ Missing shared test runner: $TEST_SUITE" >&2
    exit 1
fi

while IFS=$'\t' read -r schedule tier; do
    [[ -z ${tier:-} ]] && continue
    if [[ $schedule == bg ]]; then
        start_check "$tier" "$TEST_SUITE" "$tier"
    else
        run_check "$tier" "$TEST_SUITE" "$tier"
    fi
done < <(TS_MODE=local "$TEST_SUITE" --list local --plain)

wait_for_background_checks
print_summary
((failures == 0))
