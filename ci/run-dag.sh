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
# portable and privileged CI plans. Execution asks scripts/validate.rs for the
# constructed DAG so its typed result declarations and portable corpus-derived
# strict-compatibility expansion are shared by local, hosted, and validate
# callers.
#
# Usage:
#   ci/run-dag.sh <lane> [runner-args...]
#     <lane>            portable | privileged  (selects ci/dag/<lane>.json)
#     runner-args       forwarded verbatim to `dagrun run`
#                       (e.g. -j 8, --max-mem 32G, --perf-dir ./perf,
#                        -k/--keep-going, -v, -q)
#
# Examples:
#   ci/run-dag.sh portable --max-mem 32G
#   ci/run-dag.sh privileged -j 1 --perf-dir ./perf
#   ci/run-dag.sh portable ascii   # any non-`run` verb also works
#
# Runner:
#   run-dag.sh always constructs the selected validation DAG with
#   scripts/validate.rs --write-constructed-dag into a unique retained
#   target/validation/run-dag.* directory, then execs the tracked Rust scheduler
#   at agent-utils/rs/bin/dagrun with the DAG on stdin. Runtime runner and DAG
#   overrides are rejected so CI cannot silently switch to a different scheduler
#   or raw graph.

set -uo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR" || exit 2

print_help() {
    cat <<'EOF'
run-dag.sh — run a Hermit CI validation lane as a dagrun DAG.

Usage:
  ci/run-dag.sh <lane> [runner-args...]
    <lane>            portable | privileged
    runner-args       either a leading inspection verb, then that verb's flags,
                      or run-mode flags for the default `run` verb

Runner verbs:
  run                 default; executes the DAG
  list | ascii | dot | json
                      consumed as the dagrun verb before --dag -

Run-mode examples:
  -j 8, --max-mem 32G, --perf-dir ./perf, -k/--keep-going, -v, -q

Examples:
  ci/run-dag.sh portable --max-mem 32G
  ci/run-dag.sh privileged -j 1 --perf-dir ./perf
  ci/run-dag.sh portable list
  ci/run-dag.sh portable json

Runner and DAG:
  run-dag.sh constructs the selected validation DAG with
  scripts/validate.rs --write-constructed-dag into a unique retained
  target/validation/run-dag.* directory. It then execs the tracked Rust
  scheduler at agent-utils/rs/bin/dagrun with --dag - and feeds the constructed
  DAG on stdin. Runtime overrides that would select a different scheduler or
  raw graph are refused: DAGRUN_BIN, DAGRUN_ENGINE, RUN_DAG_FILE_OVERRIDE, and
  forwarded --dag are not supported.

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

is_supported_lane() {
    [[ ${1:-} == portable || ${1:-} == privileged ]]
}

is_inspection_verb() {
    [[ ${1:-} == list || ${1:-} == ascii || ${1:-} == dot || ${1:-} == json ]]
}

if (($# == 1)) && is_help_arg "$1"; then
    print_help
    exit 0
fi

if (($# == 2)) && is_supported_lane "$1" && is_help_arg "$2"; then
    print_help
    exit 0
fi

if (($# == 3)) && is_supported_lane "$1" && is_inspection_verb "$2" && is_help_arg "$3"; then
    print_help
    exit 0
fi

if (($# < 1)); then
    echo "usage: ci/run-dag.sh <portable|privileged> [runner-args...]" >&2
    exit 2
fi

# shellcheck source=ci/configure-build-jobs.sh
source "$ROOT_DIR/ci/configure-build-jobs.sh" launcher || exit $?

lane=$1
shift

for name in DAGRUN_BIN DAGRUN_ENGINE RUN_DAG_FILE_OVERRIDE; do
    if [[ -n ${!name:-} ]]; then
        echo "run-dag.sh: $name is no longer supported for CI DAG execution." >&2
        echo "            Use ci/run-dag.sh <portable|privileged>; it constructs the DAG and runs agent-utils/rs/bin/dagrun." >&2
        exit 2
    fi
done

for arg in "$@"; do
    case "$arg" in
        --dag|--dag=*)
            echo "run-dag.sh: forwarded --dag is not supported." >&2
            echo "            ci/run-dag.sh constructs the portable or privileged DAG with scripts/validate.rs --write-constructed-dag." >&2
            exit 2
            ;;
    esac
done

case "$lane" in
    portable|privileged) ;;
    *)
        echo "run-dag.sh: unknown lane '$lane'." >&2
        echo "            known lanes: portable, privileged" >&2
        exit 2
        ;;
esac

case "$lane" in
    portable) validate_level=portable-only ;;
    privileged) validate_level=--privileged-only ;;
esac

# Structured result ownership is attached during validate plan construction for
# both lanes. Portable strict compatibility is also generated from the canonical
# corpus there. Raw lane execution must consume that same constructed graph:
# executing portable.json directly would reach the fail-closed test.strict_compat
# marker instead of the 189 compat.* steps, while executing either raw graph
# would bypass the structured-result declarations.
# The generated DAG directory is intentionally retained for every runner
# outcome. Validation checkout lifecycle cleanup may remove it later only after
# no validation run is active. For persistent local runs, remove the printed
# path manually after confirming no active process still needs it.

# A leading non-`run` verb (list/ascii/dot/json) is passed straight through; the
# common case is `run` with scheduling flags.
verb=run
if (($# > 0)) && [[ $1 == list || $1 == ascii || $1 == dot || $1 == json ]]; then
    verb=$1
    shift
fi

runner="$ROOT_DIR/agent-utils/rs/bin/dagrun"
echo "run-dag.sh: lane=$lane runner=$runner verb=$verb cargo-jobs=$CARGO_BUILD_JOBS reverie-dbt-budget=portable-build-child-only" >&2
if [[ ! -e "$runner" ]]; then
    echo "run-dag.sh: fixed Rust dagrun runner is missing: $runner" >&2
    echo "            Build or repair the tracked agent-utils checkout so agent-utils/rs/bin/dagrun exists." >&2
    exit 127
fi
if [[ ! -f "$runner" || ! -x "$runner" ]]; then
    echo "run-dag.sh: fixed Rust dagrun runner is not an executable file: $runner" >&2
    echo "            Build or repair the tracked agent-utils checkout so agent-utils/rs/bin/dagrun is executable." >&2
    exit 126
fi

if [[ $verb == run ]]; then
    export HERMIT_REAL_RUST_SCRIPT
    HERMIT_REAL_RUST_SCRIPT=$(command -v rust-script) || {
        echo "run-dag.sh: rust-script is required" >&2
        exit 2
    }
    export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$ROOT_DIR/target/ci/rust-scripts"
    export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1
    export PATH="$ROOT_DIR/ci/rust-script-bin:$PATH"
fi

dag_parent="$ROOT_DIR/target/validation"
mkdir -p "$dag_parent" || exit 2
dag_dir=$(mktemp -d "$dag_parent/run-dag.XXXXXXXXXX") || exit 2
dag_file="$dag_dir/$lane.json"

"$ROOT_DIR/scripts/validate.rs" "$validate_level" --write-constructed-dag "$dag_file" 1>&2
rc=$?
if ((rc != 0)); then
    echo "run-dag.sh: DAG construction failed; retained generated DAG directory: $dag_dir" >&2
    echo "            Inspect that path, then remove it only after confirming no validation run is active." >&2
    exit "$rc"
fi

echo "run-dag.sh: retained generated DAG directory: $dag_dir" >&2
echo "            Validation checkout lifecycle cleanup may remove it after no validation run is active." >&2

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
