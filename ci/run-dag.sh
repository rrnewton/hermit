#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# run-dag.sh — run a Hermit CI validation lane as a dagrun DAG.
#
# This entrypoint is the shared local/GitHub execution path for the centralized
# validation profiles. It asks scripts/validate.rs for the selected graph from
# the committed validation DAG, retains that exact input and its run state, and
# hands the graph to the tracked Rust dagrun runner on stdin.
#
# Usage:
#   ci/run-dag.sh <label> [runner-args...]
#     <label>           quick | portable | full | super | privileged
#     runner-args       allowlisted non-selection controls forwarded to `dagrun run`
#                       (e.g. -j 8, --max-mem 32G, --perf-dir ./perf,
#                        -k/--keep-going, -v, -q)
#                       graph, label, selected-step, command, stress, and
#                       resource-policy overrides are refused
#
# Examples:
#   ci/run-dag.sh portable --max-mem 32G
#   ci/run-dag.sh privileged -j 1 --perf-dir ./perf
#   ci/run-dag.sh portable json
#
# Runner:
#   run-dag.sh always constructs the selected graph with
#   scripts/validate.rs --write-constructed-dag into a unique retained
#   target/validation/run-dag.* directory, then execs the tracked Rust runner at
#   agent-utils/rs/bin/dagrun with --dag -. Runtime runner, graph, and label
#   overrides are refused.

set -uo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR" || exit 2

print_help() {
    cat <<'EOF'
run-dag.sh — run a Hermit CI validation profile as a dagrun DAG.

Usage:
  ci/run-dag.sh <label> [runner-args...]
    <label>            quick | portable | full | super | privileged
    runner-args       either a leading inspection verb, then that verb's flags,
                      or allowlisted run-mode controls for the default `run` verb

Runner verbs:
  run                 default; executes the DAG
  list | ascii | dot | json
                      consumed as the dagrun verb before --dag -

Examples:
  ci/run-dag.sh portable --max-mem 32G
  ci/run-dag.sh privileged -j 1 --perf-dir ./perf
  ci/run-dag.sh portable json

Runner and DAG:
  run-dag.sh constructs the selected graph with
  scripts/validate.rs --write-constructed-dag into a unique retained
  target/validation/run-dag.* directory. It then execs the tracked Rust
  scheduler at agent-utils/rs/bin/dagrun with --dag - and feeds the constructed
  DAG on stdin. Runtime overrides that would select a different scheduler,
  graph, or label are refused: DAGRUN_BIN, DAGRUN_ENGINE,
  RUN_DAG_FILE_OVERRIDE, RUN_DAG_LABEL_OVERRIDE, and forwarded selection flags
  are not supported.

Retention:
  After a generated DAG directory has been allocated, it is intentionally
  retained for construction and runner outcomes. Pre-allocation refusals have
  no retained path. When a retained path is printed, inspect that exact path
  after failures. Validation checkout lifecycle cleanup may remove it later only
  after no validation run is active; for persistent local runs, remove it
  manually after confirming no active process still needs it.

Containment:
  dagrun's normal run mode uses its configured boxing policy. Explicit unboxed
  flags such as --unsafe-no-cgroups, or --allow-cgroup-failure fallback, are not
  containment guarantees.

Environment:
  CI_DAG_BUILD_JOBS   optional build-job count consumed by
                      ci/configure-build-jobs.sh for execution. Invalid values
                      are refused for execution, but -h/--help is available
                      before configuration validation.

Options:
  -h, --help       Print this help
EOF
}

is_help_arg() {
    [[ ${1:-} == -h || ${1:-} == --help ]]
}

is_supported_label() {
    [[ ${1:-} =~ ^(quick|portable|full|super|privileged)$ ]]
}

is_inspection_verb() {
    [[ ${1:-} == list || ${1:-} == ascii || ${1:-} == dot || ${1:-} == json ]]
}

if (($# == 1)) && is_help_arg "$1"; then
    print_help
    exit 0
fi

if (($# == 2)) && is_supported_label "$1" && is_help_arg "$2"; then
    print_help
    exit 0
fi

if (($# == 3)) && is_supported_label "$1" && is_inspection_verb "$2" && is_help_arg "$3"; then
    print_help
    exit 0
fi

if (($# < 1)); then
    echo "usage: ci/run-dag.sh <quick|portable|full|super|privileged> [runner-args...]" >&2
    exit 2
fi

# shellcheck source=ci/configure-build-jobs.sh
source "$ROOT_DIR/ci/configure-build-jobs.sh" launcher || exit $?

lane=$1
shift

for name in DAGRUN_BIN DAGRUN_ENGINE RUN_DAG_FILE_OVERRIDE RUN_DAG_LABEL_OVERRIDE; do
    if [[ -n ${!name:-} ]]; then
        echo "run-dag.sh: $name is not supported for CI DAG execution." >&2
        echo "            Use ci/run-dag.sh <quick|portable|full|super|privileged>; it constructs the selected DAG and runs agent-utils/rs/bin/dagrun." >&2
        exit 2
    fi
done

if ! is_supported_label "$lane"; then
    echo "run-dag.sh: unknown validation label '$lane'" >&2
    echo "            known labels: quick, portable, full, super, privileged" >&2
    exit 2
fi
case $lane in
    quick) validate_selector=quick ;;
    portable) validate_selector=--hosted-portable-only ;;
    full) validate_selector=full ;;
    super) validate_selector=super ;;
    privileged) validate_selector=--hosted-privileged-only ;;
esac

# The wrapper, not its caller, owns both graph identity and label selection.
# dagrun accepts repeated options with the final value winning, so merely
# placing these options first would let a trailing caller argument replace the
# committed DAG or select a different profile. Forward only runner controls
# that cannot change graph contents or the selected node population.
validate_runner_args() {
    local arg
    while (($# > 0)); do
        arg=$1
        shift
        case "$arg" in
            --dag|--dag=*|--labels|--labels=*|--selected|--selected=*|\
            --ignore-selected-deps|--ignore-selected-deps=*|--args|--args=*|\
            --stress|--stress=*|--resource-caps-path|--resource-caps-path=*|\
            --small-default-cap|--small-default-cap=*)
                echo "run-dag.sh: refusing caller graph/selection override '$arg'; this entry point owns --dag and --labels" >&2
                return 2
                ;;
            -s|-j|--max-steps|--max-cpus|--jobs|--cores|--cpuset|--pin|\
            --max-mem|--perf-dir|--profile-timeseries|--planner|--profile-sync|\
            --profile-sync-direction|--run-timeout|--cpu-timeout-multiplier)
                if (($# == 0)); then
                    echo "run-dag.sh: runner option '$arg' requires a value" >&2
                    return 2
                fi
                shift
                ;;
            --max-steps=*|--max-cpus=*|--jobs=*|--cores=*|--cpuset=*|--pin=*|\
            --max-mem=*|--perf-dir=*|--profile-timeseries=*|--planner=*|\
            --profile-sync=*|--profile-sync-direction=*|--run-timeout=*|\
            --cpu-timeout-multiplier=*|-s?*|-j?*)
                ;;
            --admission)
                # dagrun's admission wait is optional. It consumes the next
                # token only when that token is a nonnegative value rather
                # than another flag; dagrun remains responsible for validating
                # the number and its range.
                if (($# > 0)) && [[ $1 != -* ]]; then
                    shift
                fi
                ;;
            --admission=*|--no-profile|--profile|--show-plan|\
            --no-profile-feedback|--profile-memory-feedback|-k|--keep-going|\
            --no-color|--allow-cgroup-failure|--unsafe-no-cgroups|\
            --allow-unwise-nest-dagruns|-v|-q|--quiet)
                ;;
            *)
                echo "run-dag.sh: unsupported runner argument '$arg'; pass only documented non-selection run controls" >&2
                return 2
                ;;
        esac
    done
}

# A leading non-`run` verb (list/ascii/dot/json) is passed straight through; the
# common case is `run` with scheduling flags.
verb=run
if (($# > 0)) && [[ $1 == list || $1 == ascii || $1 == dot || $1 == json ]]; then
    verb=$1
    shift
fi

validate_runner_args "$@" || exit $?

runner="$ROOT_DIR/agent-utils/rs/bin/dagrun"
echo "run-dag.sh: label=$lane runner=$runner verb=$verb cargo-jobs=$CARGO_BUILD_JOBS reverie-dbt-budget=portable-build-child-only" >&2
if [[ ! -e $runner ]]; then
    echo "run-dag.sh: fixed Rust dagrun runner is missing: $runner" >&2
    echo "            Build or repair the tracked agent-utils checkout so agent-utils/rs/bin/dagrun exists." >&2
    exit 127
fi
if [[ ! -f $runner || ! -x $runner ]]; then
    echo "run-dag.sh: fixed Rust dagrun runner is not an executable file: $runner" >&2
    echo "            Build or repair the tracked agent-utils checkout so agent-utils/rs/bin/dagrun is executable." >&2
    exit 126
fi

if [[ -n ${VALIDATE_RUN_STATE:-} ]]; then
    echo "run-dag.sh: refusing inherited VALIDATE_RUN_STATE; every top-level run owns a unique retained state directory" >&2
    exit 2
fi

export HERMIT_REAL_RUST_SCRIPT
HERMIT_REAL_RUST_SCRIPT=$(command -v rust-script) || {
    echo "run-dag.sh: rust-script is required" >&2
    exit 2
}
HERMIT_REAL_RUST_SCRIPT=$(realpath -- "$HERMIT_REAL_RUST_SCRIPT") || exit 2
prebuilt_rust_script=$(realpath -m -- "$ROOT_DIR/ci/rust-script-bin/rust-script") || exit 2
if [[ $HERMIT_REAL_RUST_SCRIPT == "$prebuilt_rust_script" ]]; then
    echo "run-dag.sh: construction requires the real rust-script executable before the prebuilt shim in PATH" >&2
    exit 2
fi
rust_script_bootstrap_path=$(dirname -- "$HERMIT_REAL_RUST_SCRIPT"):$PATH

dag_parent="$ROOT_DIR/target/validation"
mkdir -p "$dag_parent" || exit 2
dag_dir=$(mktemp -d "$dag_parent/run-dag.XXXXXXXXXX") || exit 2
dag_file="$dag_dir/$lane.json"

env -u HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED \
    -u HERMIT_RUST_SCRIPT_ARTIFACT_ROOT \
    PATH="$rust_script_bootstrap_path" \
    "$ROOT_DIR/scripts/validate.rs" \
    "$validate_selector" --write-constructed-dag "$dag_file" 1>&2
rc=$?
if ((rc != 0)); then
    echo "run-dag.sh: DAG construction failed; retained generated DAG directory: $dag_dir" >&2
    echo "            Inspect that path, then remove it only after confirming no validation run is active." >&2
    exit "$rc"
fi

if [[ $verb == run ]]; then
    export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$ROOT_DIR/target/ci/rust-scripts"
    export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1
    export PATH="$ROOT_DIR/ci/rust-script-bin:$PATH"
fi

echo "run-dag.sh: retained generated DAG directory: $dag_dir" >&2
echo "            Validation checkout lifecycle cleanup may remove it after no validation run is active." >&2

if [[ $verb == run ]]; then
    VALIDATE_RUN_STATE="$dag_dir/run-state"
    export VALIDATE_RUN_STATE
    export E2E_RESULT_ROOT=${E2E_RESULT_ROOT:-"$VALIDATE_RUN_STATE/results"}
    export E2E_BUILD_ROOT=${E2E_BUILD_ROOT:-"$VALIDATE_RUN_STATE/build"}
    mkdir -p "$VALIDATE_RUN_STATE" "$E2E_RESULT_ROOT" "$E2E_BUILD_ROOT" || exit 2
fi

if ! exec {dag_fd}<"$dag_file"; then
    echo "run-dag.sh: constructed DAG is not readable: $dag_file" >&2
    echo "            Retained generated DAG directory: $dag_dir" >&2
    echo "            Inspect that path, then remove it only after confirming no validation run is active." >&2
    exit 2
fi
if ! exec <&"$dag_fd"; then
    echo "run-dag.sh: could not attach constructed DAG to runner stdin: $dag_file" >&2
    echo "            Retained generated DAG directory: $dag_dir" >&2
    echo "            Inspect that path, then remove it only after confirming no validation run is active." >&2
    exit 2
fi
if ! exec {dag_fd}<&-; then
    echo "run-dag.sh: could not close constructed DAG descriptor after stdin handoff: $dag_file" >&2
    echo "            Retained generated DAG directory: $dag_dir" >&2
    echo "            Inspect that path, then remove it only after confirming no validation run is active." >&2
    exit 2
fi
exec "$runner" "$verb" --dag - "$@"
