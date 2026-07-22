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

declare -a check_names=()
declare -a check_results=()
declare -a check_durations=()
failures=0

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

COLOR_GREEN=""
COLOR_RED=""
COLOR_RESET=""
if [[ -t 1 && ${TERM:-dumb} != "dumb" && -z ${NO_COLOR:-} ]]; then
    COLOR_GREEN=$'\033[32m'
    COLOR_RED=$'\033[31m'
    COLOR_RESET=$'\033[0m'
fi
readonly COLOR_GREEN COLOR_RED COLOR_RESET

readonly HERMIT_BIN="$ROOT_DIR/target/debug/hermit"
readonly HERMIT_SMOKE_TIMEOUT="30s"
readonly SMOKE_MARKER="hermit-validation-smoke"
declare -ar HERMIT_RUN_ARGS=(
    run
    --base-env=minimal
    --no-virtualize-cpuid
    --preemption-timeout=disabled
)

function interrupted {
    echo
    echo "Validation interrupted."
    exit 130
}
trap interrupted INT TERM

function banner {
    echo
    echo "================================================================================"
    echo ">>> $*"
    echo "================================================================================"
}

function run_check {
    local name=$1
    shift

    local started_at=$SECONDS
    local status

    banner "$name"
    printf "Command:"
    printf " %q" "$@"
    echo

    if "$@"; then
        status=0
        check_results+=("PASS")
        printf "%bPASS%b: %s\n" "$COLOR_GREEN" "$COLOR_RESET" "$name"
    else
        status=$?
        check_results+=("FAIL (exit $status)")
        failures=$((failures + 1))
        printf "%bFAIL%b: %s (exit %s)\n" \
            "$COLOR_RED" "$COLOR_RESET" "$name" "$status"
    fi

    check_names+=("$name")
    check_durations+=("$((SECONDS - started_at))")
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
    banner "Validation summary"

    local i
    for i in "${!check_names[@]}"; do
        local color=$COLOR_RED
        if [[ ${check_results[$i]} == "PASS" ]]; then
            color=$COLOR_GREEN
        fi
        printf "  %-48s %b%-16s%b %ss\n" \
            "${check_names[$i]}" "$color" "${check_results[$i]}" \
            "$COLOR_RESET" "${check_durations[$i]}"
    done

    echo
    if ((failures == 0)); then
        echo "All ${#check_names[@]} validation checks passed."
    else
        echo "$failures of ${#check_names[@]} validation checks failed."
    fi
}

run_check "cargo-nextest available" ensure_cargo_nextest
run_check "Build workspace" cargo build --workspace
run_check "Hermit run smoke test" hermit_run_smoke
run_check "Hermit output determinism" hermit_determinism_check
run_check "Hermit verify-mode smoke test" hermit_verify_smoke
# Nextest runs package unit and Cargo integration targets in parallel. It does
# not run rustdoc tests, so keep those as a separate Cargo phase.
run_check "Test workspace and integrations" \
    "${NEXTEST_RUN[@]}" --workspace \
    --exclude hermetic_infra_hermit_flaky-tests
run_check "Test workspace documentation" cargo test --workspace --doc
run_check "Fast concurrency stress suite" \
    "${NEXTEST_RUN[@]}" -p hermit --test stress_suite \
    --run-ignored only -E 'test(=fast_chaos_matrix)'
run_check "Clippy" cargo clippy --workspace --all-targets -- -D warnings
run_check "Rustfmt" cargo fmt --all -- --check
run_check "Documentation" cargo doc --workspace --no-deps

print_summary
((failures == 0))
