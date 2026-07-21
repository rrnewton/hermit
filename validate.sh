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
}

function print_summary {
    banner "Validation summary"

    local i
    for i in "${!check_names[@]}"; do
        printf "  %-48s %-16s %ss\n" \
            "${check_names[$i]}" \
            "${check_results[$i]}" \
            "${check_durations[$i]}"
    done

    echo
    if ((failures == 0)); then
        echo "All ${#check_names[@]} validation checks passed."
    else
        echo "$failures of ${#check_names[@]} validation checks failed."
    fi
}

run_check "Build workspace" cargo build --workspace
# Workspace tests include package unit, documentation, and Cargo integration
# targets such as hermit-cli/tests/hermit_modes.rs.
run_check "Test workspace and integrations" \
    cargo test --workspace --exclude hermetic_infra_hermit_flaky-tests
run_check "Clippy" cargo clippy --workspace --all-targets -- -D warnings
run_check "Rustfmt" cargo fmt --all -- --check
run_check "Documentation" cargo doc --workspace --no-deps

print_summary
((failures == 0))
