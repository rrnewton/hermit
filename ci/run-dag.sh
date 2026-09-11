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
# portable and privileged CI plans. Each gate is an independently boxed node
# with explicit dependencies and resource limits (see ci/dag/README.md).
# Execution asks scripts/validate.rs for the constructed DAG so its typed result
# declarations and portable corpus-derived strict-compatibility expansion are
# shared by local, hosted, and validate callers.
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
#   scripts/validate.rs --write-constructed-dag, then runs the tracked Rust
#   scheduler at agent-utils/rs/bin/dagrun. Runtime runner and DAG overrides are
#   rejected so CI cannot silently switch to a different scheduler or raw graph.

set -uo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR" || exit 2

# shellcheck source=ci/configure-build-jobs.sh
source "$ROOT_DIR/ci/configure-build-jobs.sh" launcher || exit $?

if (($# < 1)); then
    echo "usage: ci/run-dag.sh <portable|privileged> [runner-args...]" >&2
    exit 2
fi

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

runner="$ROOT_DIR/agent-utils/rs/bin/dagrun"
if [[ ! -e "$runner" ]]; then
    echo "run-dag.sh: required dagrun runner is missing: $runner" >&2
    echo "            Build or check out the tracked Rust runner at agent-utils/rs/bin/dagrun." >&2
    exit 127
fi
if [[ ! -f "$runner" || ! -x "$runner" ]]; then
    echo "run-dag.sh: required dagrun runner is not an executable file: $runner" >&2
    echo "            Build or repair the tracked Rust runner at agent-utils/rs/bin/dagrun." >&2
    exit 126
fi

# Structured result ownership is attached during validate plan construction for
# both lanes. Portable strict compatibility is also generated from the canonical
# corpus there. Raw lane execution must consume that same constructed graph:
# executing portable.json directly would reach the fail-closed test.strict_compat
# marker instead of the 189 compat.* steps, while executing either raw graph
# would bypass the structured-result declarations.
# This exports data only; the single dagrun invocation below remains the only
# scheduler.
generated_dir=
cleanup_generated_dir() {
    if [[ -n ${generated_dir:-} ]]; then
        rm -rf -- "$generated_dir"
    fi
}
mkdir -p "$ROOT_DIR/target/validation" || exit 2
generated_dir=$(mktemp -d "$ROOT_DIR/target/validation/run-dag.XXXXXX") || exit 2
trap cleanup_generated_dir EXIT
dag="$generated_dir/$lane.json"
level="${lane}-only"
if [[ $lane == privileged ]]; then
    level=--privileged-only
fi
if ! ./scripts/validate.rs "$level" --write-constructed-dag "$dag" >/dev/null; then
    echo "run-dag.sh: validate could not construct the $lane DAG" >&2
    exit 2
fi
exec {dag_fd}<"$dag" || {
    echo "run-dag.sh: could not open constructed $lane DAG: $dag" >&2
    exit 2
}
if ! cleanup_generated_dir; then
    echo "run-dag.sh: could not remove generated DAG directory: $generated_dir" >&2
    exit 2
fi
if ! exec <&"$dag_fd"; then
    echo "run-dag.sh: could not attach constructed $lane DAG to stdin" >&2
    exit 2
fi
if ! exec {dag_fd}<&-; then
    echo "run-dag.sh: could not close constructed $lane DAG descriptor" >&2
    exit 2
fi
trap - EXIT

# A leading non-`run` verb (list/ascii/dot/json) is passed straight through; the
# common case is `run` with scheduling flags.
verb=run
if (($# > 0)) && [[ $1 == list || $1 == ascii || $1 == dot || $1 == json ]]; then
    verb=$1
    shift
fi

echo "run-dag.sh: lane=$lane runner=$runner verb=$verb cargo-jobs=$CARGO_BUILD_JOBS reverie-dbt-budget=portable-build-child-only" >&2
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
exec "$runner" "$verb" --dag - "$@"
