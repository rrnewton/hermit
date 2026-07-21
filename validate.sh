#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# This is the canonical local validation entrypoint. Keep comprehensive test
# discovery here so newly ported Cargo tests join the default validation path.

set -uo pipefail

readonly ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT_DIR"

declare -a check_names=()
declare -a check_results=()
declare -a check_durations=()
declare -a check_test_counts=()
failures=0
total_test_executions=0

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
        echo "PASS: $name"
    else
        status=$?
        check_results+=("FAIL (exit $status)")
        failures=$((failures + 1))
        echo "FAIL: $name (exit $status)"
    fi

    check_names+=("$name")
    check_durations+=("$((SECONDS - started_at))")
    check_test_counts+=("-")
}

function run_test_gate {
    local name=$1
    shift

    local started_at=$SECONDS
    local listing
    local ignored_listing
    local discovered
    local ignored
    local status
    local executed=partial

    banner "$name"
    printf "Command:"
    printf " %q" "$@"
    echo
    printf "Discovery command:"
    printf " %q" "$@"
    echo " -- --list"
    printf "Ignored-test command:"
    printf " %q" "$@"
    echo " -- --list --ignored"

    if listing=$("$@" -- --list); then
        discovered=$(
            printf "%s\n" "$listing" \
                | awk '/: test$/ { count++ } END { print count + 0 }'
        )
    else
        status=$?
        echo "FAIL: $name test discovery failed (exit $status)"
        check_names+=("$name")
        check_results+=("FAIL (discovery $status)")
        check_durations+=("$((SECONDS - started_at))")
        check_test_counts+=("0/?")
        failures=$((failures + 1))
        return
    fi

    if ignored_listing=$("$@" -- --list --ignored); then
        ignored=$(
            printf "%s\n" "$ignored_listing" \
                | awk '/: test$/ { count++ } END { print count + 0 }'
        )
    else
        status=$?
        echo "FAIL: $name ignored-test discovery failed (exit $status)"
        check_names+=("$name")
        check_results+=("FAIL (discovery $status)")
        check_durations+=("$((SECONDS - started_at))")
        check_test_counts+=("0/$discovered")
        failures=$((failures + 1))
        return
    fi

    if ((discovered == 0)); then
        echo "FAIL: $name discovered no tests"
        check_names+=("$name")
        check_results+=("FAIL (no tests)")
        check_durations+=("$((SECONDS - started_at))")
        check_test_counts+=("0/0")
        failures=$((failures + 1))
        return
    fi
    if ((ignored > discovered)); then
        echo "FAIL: $name discovered $ignored ignored tests out of $discovered total"
        check_names+=("$name")
        check_results+=("FAIL (bad count)")
        check_durations+=("$((SECONDS - started_at))")
        check_test_counts+=("0/$discovered")
        failures=$((failures + 1))
        return
    fi

    echo "Discovered $discovered tests ($ignored ignored)."
    if "$@"; then
        executed=$((discovered - ignored))
        total_test_executions=$((total_test_executions + executed))
        check_results+=("PASS")
        echo "PASS: $name"
    else
        status=$?
        check_results+=("FAIL (exit $status)")
        failures=$((failures + 1))
        echo "FAIL: $name (exit $status)"
    fi

    echo "Tests: $executed executed of $discovered discovered ($ignored ignored)."
    check_names+=("$name")
    check_durations+=("$((SECONDS - started_at))")
    check_test_counts+=("$executed/$discovered")
}

function print_summary {
    banner "Validation summary"

    local i
    printf "  %-42s %-18s %-14s %s\n" "Gate" "Result" "Tests run" "Time"
    for i in "${!check_names[@]}"; do
        printf "  %-42s %-18s %-14s %ss\n" \
            "${check_names[$i]}" \
            "${check_results[$i]}" \
            "${check_test_counts[$i]}" \
            "${check_durations[$i]}"
    done

    echo
    echo "Total test executions (including focused repeats): $total_test_executions"
    if ((failures == 0)); then
        echo "All ${#check_names[@]} validation checks passed."
    else
        echo "$failures of ${#check_names[@]} validation checks failed."
    fi
}

run_check "Build workspace" cargo build --workspace
# The workspace gate automatically discovers package, documentation, and new
# Cargo integration tests. The following focused gates intentionally repeat
# critical scenarios so their failures and timings remain independently visible.
run_test_gate "Workspace tests" cargo test --workspace
run_test_gate \
    "Hermit mode integration tests" \
    cargo test -p hermit --test hermit_modes
run_test_gate \
    "Record/replay smoke test" \
    cargo test -p hermit --test hermit_modes record_replay_matrix
run_test_gate \
    "Chaos/verify smoke test" \
    cargo test -p hermit --test hermit_modes hello_race_chaos_verify
run_check "Clippy" cargo clippy --workspace --all-targets -- -D warnings
run_check "Rustfmt" cargo fmt --all -- --check
run_check "Documentation" cargo doc --workspace --no-deps

print_summary
((failures == 0))
