#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -uo pipefail

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

readonly NEXTEST_VERSION=0.9.100
NEXTEST_PROFILE_NAME=${NEXTEST_PROFILE:-}
if [[ -z $NEXTEST_PROFILE_NAME && -n ${CI:-} ]]; then
    NEXTEST_PROFILE_NAME=ci
fi
declare -a NEXTEST_RUN=(cargo nextest run)
if [[ -n $NEXTEST_PROFILE_NAME ]]; then
    NEXTEST_RUN+=(--profile "$NEXTEST_PROFILE_NAME")
fi
readonly NEXTEST_PROFILE_NAME NEXTEST_RUN

readonly HERMIT_BIN="$ROOT_DIR/target/debug/hermit"
readonly HERMIT_SMOKE_TIMEOUT="30s"
readonly SMOKE_MARKER="hermit-validation-smoke"
declare -ar HERMIT_RUN_ARGS=(
    run
    --base-env=minimal
    --no-virtualize-cpuid
    --preemption-timeout=disabled
)

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
function hermit_echo {
    timeout "$HERMIT_SMOKE_TIMEOUT" \
        "$HERMIT_BIN" "${HERMIT_RUN_ARGS[@]}" -- \
        /bin/echo "$SMOKE_MARKER"
}

function hermit_run_smoke {
    local output
    local status

    output=$(hermit_echo)
    status=$?
    if ((status != 0)); then
        return "$status"
    fi

    if [[ "$output" != "$SMOKE_MARKER" ]]; then
        printf "Unexpected Hermit stdout: %q\n" "$output" >&2
        return 1
    fi
}

function hermit_determinism_check {
    local first_output
    local second_output
    local status

    first_output=$(hermit_echo)
    status=$?
    if ((status != 0)); then
        return "$status"
    fi

    second_output=$(hermit_echo)
    status=$?
    if ((status != 0)); then
        return "$status"
    fi

    if [[ "$first_output" != "$second_output" ]]; then
        echo "Hermit stdout differed between identical runs:" >&2
        diff -u \
            <(printf "%s\n" "$first_output") \
            <(printf "%s\n" "$second_output") >&2 || true
        return 1
    fi
}

function hermit_verify_smoke {
    timeout "$HERMIT_SMOKE_TIMEOUT" \
        "$HERMIT_BIN" "${HERMIT_RUN_ARGS[@]}" --verify -- \
        /bin/echo "$SMOKE_MARKER"
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
run_check "Build workspace" cargo build --workspace

# Cargo supports concurrent commands in one target directory. Run checks that
# do not execute Hermit guests alongside the ordered runtime and PMU gates.
start_check "Test workspace documentation" cargo test --workspace --doc
start_check "Clippy" cargo clippy --workspace --all-targets -- -D warnings
start_check "Rustfmt" cargo fmt --all -- --check
start_check "Documentation" cargo doc --workspace --no-deps

run_check "Hermit run smoke test" hermit_run_smoke
run_check "Hermit output determinism" hermit_determinism_check
run_check "Hermit verify-mode smoke test" hermit_verify_smoke
# Nextest runs most package unit and Cargo integration targets in parallel.
# Detcore's PMU tests depend on same-binary coordination; nextest would launch
# them as separate processes. Keep detcore and rustdoc tests as Cargo phases.
run_check "Test workspace and integrations" \
    "${NEXTEST_RUN[@]}" --workspace --exclude detcore \
    --exclude hermetic_infra_hermit_flaky-tests
run_check "Test detcore package" cargo test -p detcore
run_check "Fast concurrency stress suite" \
    "${NEXTEST_RUN[@]}" -p hermit --test stress_suite \
    -E 'test(=fast_chaos_matrix)'
# `hermit analyze` root-cause search over chaotic schedules (Buck analyze_* targets).
run_check "Hermit analyze scenarios" \
    cargo test -p hermit --test analyze -- --test-threads=1
run_check "Schedule search E2E (requires PMU)" \
    ./tests/util/hermit_analyze_e2e.sh
# rr's syscall edge-case programs (third-party/rr submodule) run under Hermit.
if [[ -f "$ROOT_DIR/third-party/rr/src/test/util.h" ]]; then
    run_check "rr syscall suite" \
        cargo test -p hermit --test rr_suite -- --test-threads=1
else
    run_check "rr syscall suite prerequisite" \
        test -f "$ROOT_DIR/third-party/rr/src/test/util.h"
fi

wait_for_background_checks
print_summary
((failures == 0))
