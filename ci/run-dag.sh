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
# Every standard profile selects a labelled subset of the same committed DAG.
# No profile rewrites commands, dependencies, or resource policy at launch.
#
# Usage:
#   ci/run-dag.sh <label> [runner-args...]
#     <label>           quick | portable | full | super | privileged
#                       (selects labelled steps from ci/dag/validate.json)
#     runner-args       forwarded verbatim to `dagrun run`
#                       (e.g. -j 8, --max-mem 32G, --perf-dir ./perf,
#                        -k/--keep-going, -v, -q)
#
# Examples:
#   ci/run-dag.sh portable --max-mem 32G
#   ci/run-dag.sh privileged -j 1 --perf-dir ./perf
#   agent-utils/py/bin/dagrun ascii --dag ci/dag/validate.json  # inspect the superset
#
# Environment:
#   DAGRUN_BIN     override the runner executable to use.
#   RUN_DAG_FILE_OVERRIDE  run this exact DAG file instead of ci/dag/validate.json.
#                          Used by scripts/validate.rs --selective to feed a subset DAG
#                          (a dependency-closed slice of the lane) while keeping
#                          the lane argument for runner labeling. The override
#                          must exist and be readable, or run-dag.sh fails closed.

set -uo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR" || exit 2

# shellcheck source=ci/configure-build-jobs.sh
source "$ROOT_DIR/ci/configure-build-jobs.sh" launcher || exit $?

if (($# < 1)); then
    echo "usage: ci/run-dag.sh <quick|portable|full|super|privileged> [runner-args...]" >&2
    exit 2
fi

lane=$1
shift

if [[ -n ${RUN_DAG_FILE_OVERRIDE:-} ]]; then
    dag="$RUN_DAG_FILE_OVERRIDE"
    if [[ ! -f $dag ]]; then
        echo "run-dag.sh: RUN_DAG_FILE_OVERRIDE set but not a file: $dag" >&2
        exit 2
    fi
    echo "run-dag.sh: using DAG override for lane '$lane': $dag" >&2
else
    dag="$ROOT_DIR/ci/dag/validate.json"
    if [[ ! $lane =~ ^(quick|portable|full|super|privileged)$ ]]; then
        echo "run-dag.sh: unknown validation label '$lane'" >&2
        echo "            known labels: quick, portable, full, super, privileged" >&2
        exit 2
    fi
fi

# Locate the runner. Prefer an explicit override, then the TRACKED, source-invoked
# engine resolver (agent-utils/common/bin/dagrun -> engine-resolver),
# then the tracked, source-invoked Python entrypoint. NEVER auto-select the
# untracked prebuilt Rust binary (rs/bin): a compiled artifact can silently drift
# from its source, which is exactly how a runner missing an enforcement guard (the
# historical cpu_timeout gap) can run while we believe we are boxed.
#
# The staleness axis is SOURCE-INVOKED vs PREBUILT-BINARY, not Rust vs Python. The
# resolver enforces that: it defaults to the source-invoked Python entrypoint,
# selects the Rust engine ONLY on explicit DAGRUN_ENGINE=rust (never a
# silent fallback), and LOGS the winning engine + its exact path on every run. So
# invoking it here keeps hermit's execution path deterministic, tracked, and
# self-describing in the logs. Rust is reached the same way through the resolver
# once it is invoked source-first (rust-script), not via a prebuilt-binary shortcut.
find_runner() {
    if [[ -n ${DAGRUN_BIN:-} ]]; then
        printf '%s\n' "$DAGRUN_BIN"
        return 0
    fi
    local base="$ROOT_DIR/agent-utils"
    # Tracked, source-invoked resolver: deterministic engine selection that logs
    # which engine won. Preferred over any prebuilt binary.
    if [[ -x "$base/common/bin/dagrun" ]]; then
        printf '%s\n' "$base/common/bin/dagrun"
        return 0
    fi
    # Fallback: the tracked, source-invoked Python entrypoint directly.
    if [[ -x "$base/py/bin/dagrun" ]]; then
        printf '%s\n' "$base/py/bin/dagrun"
        return 0
    fi
    # Last resort: a resolver/runner already on PATH.
    if command -v dagrun >/dev/null 2>&1; then
        command -v dagrun
        return 0
    fi
    return 1
}

runner=$(find_runner) || {
    echo "run-dag.sh: dagrun not found." >&2
    echo "            Build it with: (cd agent-utils && ./setup) or set DAGRUN_BIN." >&2
    exit 2
}

# A leading non-`run` verb (list/ascii/dot/json) is passed straight through; the
# common case is `run` with scheduling flags.
verb=run
if (($# > 0)) && [[ $1 == list || $1 == ascii || $1 == dot || $1 == json ]]; then
    verb=$1
    shift
fi

if [[ $verb != run && -z ${RUN_DAG_FILE_OVERRIDE:-} ]]; then
    echo "run-dag.sh: '$verb' cannot represent the '$lane' label selection." >&2
    echo "            Inspect the committed superset directly: $runner $verb --dag $dag" >&2
    exit 2
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
if [[ $verb == run && -z ${RUN_DAG_FILE_OVERRIDE:-} ]]; then
    export VALIDATE_RUN_STATE=${VALIDATE_RUN_STATE:-"$ROOT_DIR/target/validation/run-dag-${lane}-$$"}
    mkdir -p "$VALIDATE_RUN_STATE" || exit 2
    exec "$runner" "$verb" --dag "$dag" --labels "$lane" "$@"
fi
exec "$runner" "$verb" --dag "$dag" "$@"
