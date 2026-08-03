#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# run-dag.sh — run a Hermit CI validation lane as a safe-ci-dag-runner DAG.
#
# This entrypoint maps the hand-rolled serial/parallel gate structure in
# validate.sh onto the safe-ci-dag-runner scheduler, so each gate runs as an
# independently boxed node with explicit dependencies and resource limits (see
# ci/dag/README.md). The hosted GitHub Actions lane uses this path directly;
# validate.sh remains the source of truth for the individual gate commands.
#
# Usage:
#   ci/run-dag.sh <lane> [runner-args...]
#     <lane>            hosted | hardware  (selects ci/dag/<lane>.json)
#     runner-args       forwarded verbatim to `safe-ci-dag-runner run`
#                       (e.g. -j 8, --max-mem 32G, --perf-dir ./perf, --cgroups,
#                        -k/--keep-going, -v, -q)
#
# Examples:
#   ci/run-dag.sh hosted --max-mem 32G
#   ci/run-dag.sh hardware -j 1 --perf-dir ./perf
#   ci/run-dag.sh hosted ascii     # any non-`run` verb also works: ci/run-dag.sh hosted <verb>
#
# Environment:
#   SAFE_CI_DAG_RUNNER   override the runner executable to use.
#   CI_DAG_EXCLUDE_STEPS comma-separated group.job IDs to omit from this run.
#   CI_DAG_CGROUPS=0     disable automatic local cgroup-v2 containment.
#   CI_DAG_JOBS          default explicit job count when -j/--max-mem is absent.
#   CI_DAG_OUTER_MEMORY_MAX
#                        hard cap for one complete local DAG run (default 32G).
#   CI_DAG_AGGREGATE_MEMORY_MAX
#                        hard cap shared by all local DAG runs (default: the
#                        smaller of 64G and 75% of host RAM).

set -uo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR" || exit 2

if (($# < 1)); then
    echo "usage: ci/run-dag.sh <hosted|hardware> [runner-args...]" >&2
    exit 2
fi

lane=$1
shift

dag="$ROOT_DIR/ci/dag/${lane}.json"
if [[ ! -f $dag ]]; then
    echo "run-dag.sh: unknown lane '$lane' (no such file: $dag)" >&2
    echo "            known lanes: hosted, hardware" >&2
    exit 2
fi

filtered_dag=""
if [[ -n ${CI_DAG_EXCLUDE_STEPS:-} ]]; then
    if ! command -v jq >/dev/null 2>&1; then
        echo "run-dag.sh: CI_DAG_EXCLUDE_STEPS requires jq" >&2
        exit 2
    fi
    filtered_dag=$(mktemp "${TMPDIR:-/tmp}/hermit-${lane}-dag.XXXXXX.json") || exit 2
    if ! jq --arg csv "$CI_DAG_EXCLUDE_STEPS" '
        ($csv | split(",") | map(select(length > 0))) as $excluded
        | ([.steps[] | "\(.group).\(.job)"]) as $available
        | ($excluded - $available) as $missing
        | if ($missing | length) > 0 then
              error("unknown excluded DAG step(s): \($missing | join(", "))")
          else
              .steps |= map(select(
                  ("\(.group).\(.job)") as $id
                  | ($excluded | index($id) | not)
              ))
          end
    ' "$dag" >"$filtered_dag"; then
        rm -f "$filtered_dag"
        exit 2
    fi
    dag=$filtered_dag
    trap 'rm -f "$filtered_dag"' EXIT
fi

# Locate the runner. Prefer an explicit override, then the Python entrypoint,
# which is the only implementation with Linux cgroup boxing + perf logging in
# 0.1. The compiled Rust binary remains the fallback for visualization and
# environments where the Python entrypoint is unavailable.
find_runner() {
    if [[ -n ${SAFE_CI_DAG_RUNNER:-} ]]; then
        printf '%s\n' "$SAFE_CI_DAG_RUNNER"
        return 0
    fi
    local base="$ROOT_DIR/agent-utils"
    if [[ -x "$base/py/bin/safe-ci-dag-runner" ]]; then
        printf '%s\n' "$base/py/bin/safe-ci-dag-runner"
        return 0
    fi
    if [[ -x "$base/rs/bin/safe-ci-dag-runner" ]]; then
        printf '%s\n' "$base/rs/bin/safe-ci-dag-runner"
        return 0
    fi
    if command -v safe-ci-dag-runner >/dev/null 2>&1; then
        command -v safe-ci-dag-runner
        return 0
    fi
    return 1
}

runner=$(find_runner) || {
    echo "run-dag.sh: safe-ci-dag-runner not found." >&2
    echo "            Build it with: (cd agent-utils && ./setup) or set SAFE_CI_DAG_RUNNER." >&2
    exit 2
}

# A leading non-`run` verb (list/ascii/dot/json) is passed straight through; the
# common case is `run` with scheduling flags.
verb=run
if (($# > 0)) && [[ $1 == list || $1 == ascii || $1 == dot || $1 == json ]]; then
    verb=$1
    shift
fi

# The Rust implementation accepts --cgroups but is explicitly advisory-only in
# v0.1. Contained runs therefore require the checked-out Python implementation;
# SAFE_CI_DAG_RUNNER remains useful for visualization and explicit opt-out runs.
if [[ $verb == run && ${CI_DAG_CGROUPS:-1} != 0 ]]; then
    runner="$ROOT_DIR/agent-utils/py/bin/safe-ci-dag-runner"
    if [[ ! -x $runner ]]; then
        echo "run-dag.sh: cgroup-capable Python runner is unavailable: $runner" >&2
        echo "            initialize it with: git submodule update --init agent-utils" >&2
        exit 2
    fi
fi

# Keep scheduler memory budgeting active even when an already-contained caller
# explicitly disables this wrapper's cgroups.
if [[ $verb == run ]]; then
    explicit_concurrency=0
    for arg in "$@"; do
        case "$arg" in
            -j|--jobs|-j[0-9]*|--jobs=*) explicit_concurrency=1 ;;
            --max-mem|--max-mem=*) explicit_concurrency=1 ;;
        esac
    done
    if ((explicit_concurrency == 0)); then
        if [[ -n ${CI_DAG_JOBS:-} ]]; then
            if [[ ! $CI_DAG_JOBS =~ ^[1-9][0-9]*$ ]]; then
                echo "run-dag.sh: CI_DAG_JOBS must be a positive integer" >&2
                exit 2
            fi
            set -- "$@" -j "$CI_DAG_JOBS"
        else
            set -- "$@" --max-mem "${CI_DAG_OUTER_MEMORY_MAX:-32G}"
        fi
    fi
fi
original_args=("$lane" "$@")

# Local DAGs must not rely on advisory memory hints. Enter the runner's
# delegated two-level cgroup scope automatically when the host supports a
# systemd user session. Known disposable hosted workflows explicitly opt out;
# every other unavailable scope is a hard error. The in-scope pass appends
# --cgroups so each step receives its
# measured hard_mem_max_bytes limit and exact memory.peak accounting.
if [[ $verb == run && ${CI_DAG_CGROUPS:-1} != 0 ]]; then
    if [[ ${SAFE_CI_IN_SCOPE:-0} == 1 ]]; then
        scope_unit=${SAFE_CI_SCOPE_UNIT:-}
        current_cgroup=$(awk -F: '$1 == "0" {print $3; exit}' /proc/self/cgroup)
        if [[ -z $scope_unit || $scope_unit != safe-ci-*.scope \
            || $current_cgroup != */safe-ci.slice/"$scope_unit" \
            && $current_cgroup != */safe-ci.slice/"$scope_unit"/* ]]; then
            echo "run-dag.sh: invalid SAFE_CI_IN_SCOPE sentinel; refusing unverified containment" >&2
            exit 2
        fi
        scope_cgroup="/sys/fs/cgroup${current_cgroup%/supervisor}"
        expected_scope_bytes=$(numfmt --from=iec "${CI_DAG_OUTER_MEMORY_MAX:-32G}") || exit 2
        actual_scope_bytes=$(<"$scope_cgroup/memory.max")
        actual_swap_bytes=$(<"$scope_cgroup/memory.swap.max")
        aggregate_cgroup=${scope_cgroup%/*}
        actual_aggregate_bytes=$(<"$aggregate_cgroup/memory.max")
        actual_aggregate_swap_bytes=$(<"$aggregate_cgroup/memory.swap.max")
        expected_aggregate_bytes=${CI_DAG_EFFECTIVE_AGGREGATE_MEMORY_MAX:-}
        if [[ ! $actual_scope_bytes =~ ^[0-9]+$ \
            || ! $actual_aggregate_bytes =~ ^[0-9]+$ \
            || ! $expected_aggregate_bytes =~ ^[0-9]+$ \
            || $actual_swap_bytes != 0 \
            || $actual_aggregate_swap_bytes != 0 \
            || $actual_scope_bytes -gt $expected_scope_bytes \
            || $((expected_scope_bytes - actual_scope_bytes)) -ge $(getconf PAGESIZE) \
            || $actual_aggregate_bytes -gt $expected_aggregate_bytes \
            || $((expected_aggregate_bytes - actual_aggregate_bytes)) -ge $(getconf PAGESIZE) \
            || $actual_aggregate_bytes -lt $((expected_scope_bytes * 2)) ]]; then
            echo "run-dag.sh: cgroup limit audit failed; refusing unverified containment" >&2
            exit 2
        fi
        have_cgroups=0
        for arg in "$@"; do
            [[ $arg == --cgroups ]] && have_cgroups=1
        done
        ((have_cgroups == 1)) || set -- "$@" --cgroups
    elif systemd-run --user --scope --quiet true >/dev/null 2>&1; then
        scope_memory_spec=${CI_DAG_OUTER_MEMORY_MAX:-32G}
        scope_memory_bytes=$(numfmt --from=iec "$scope_memory_spec") || {
            echo "run-dag.sh: invalid CI_DAG_OUTER_MEMORY_MAX: $scope_memory_spec" >&2
            exit 2
        }
        if [[ -n ${CI_DAG_AGGREGATE_MEMORY_MAX:-} ]]; then
            aggregate_memory_bytes=$(numfmt --from=iec "$CI_DAG_AGGREGATE_MEMORY_MAX") || {
                echo "run-dag.sh: invalid CI_DAG_AGGREGATE_MEMORY_MAX: $CI_DAG_AGGREGATE_MEMORY_MAX" >&2
                exit 2
            }
        else
            host_memory_kib=$(awk '/^MemTotal:/ {print $2; exit}' /proc/meminfo)
            if [[ ! $host_memory_kib =~ ^[0-9]+$ ]]; then
                echo "run-dag.sh: cannot determine host memory for aggregate cap" >&2
                exit 2
            fi
            host_safe_bytes=$((host_memory_kib * 1024 * 3 / 4))
            aggregate_memory_bytes=$(numfmt --from=iec 64G)
            if ((host_safe_bytes < aggregate_memory_bytes)); then
                aggregate_memory_bytes=$host_safe_bytes
            fi
        fi
        minimum_aggregate_bytes=$((scope_memory_bytes * 2))
        if ((aggregate_memory_bytes < minimum_aggregate_bytes)); then
            echo "run-dag.sh: host cannot reserve two complete DAG scopes safely" >&2
            echo "            aggregate cap $aggregate_memory_bytes is below required $minimum_aggregate_bytes bytes" >&2
            echo "            lower CI_DAG_OUTER_MEMORY_MAX or use a host with more memory" >&2
            exit 2
        fi
        if ! systemctl --user start safe-ci.slice >/dev/null \
            || ! systemctl --user --runtime set-property safe-ci.slice \
                "MemoryMax=$aggregate_memory_bytes" MemorySwapMax=0 >/dev/null; then
            echo "run-dag.sh: failed to apply shared safe-ci.slice memory cap" >&2
            exit 2
        fi
        applied_aggregate_bytes=$(systemctl --user show safe-ci.slice \
            --property=MemoryMax --value)
        if [[ $applied_aggregate_bytes != "$aggregate_memory_bytes" ]]; then
            echo "run-dag.sh: shared memory cap mismatch: requested $aggregate_memory_bytes, got $applied_aggregate_bytes" >&2
            exit 2
        fi
        printf 'run-dag.sh: aggregate memory cap: %s across safe-ci.slice\n' \
            "$(numfmt --to=iec-i --suffix=B "$aggregate_memory_bytes")" >&2
        export CI_DAG_EFFECTIVE_AGGREGATE_MEMORY_MAX=$aggregate_memory_bytes
        py_root="$ROOT_DIR/agent-utils/py"
        PYTHONPATH="$py_root${PYTHONPATH:+:$PYTHONPATH}" exec python3 -c '
import os
import sys

from safe_ci_dag_runner.cgroup import reexec_in_scope

memory_max = int(sys.argv[1])
argv = sys.argv[2:]
if not reexec_in_scope(argv, memory_max=memory_max, skip_in_ci=False):
    raise SystemExit(2)
os.execvp(argv[0], argv)
' "$scope_memory_bytes" "$0" "${original_args[@]}"
    else
        echo "run-dag.sh: systemd user scopes unavailable; refusing advisory-only validation" >&2
        echo "            set CI_DAG_CGROUPS=0 only inside another enforced container or disposable VM" >&2
        exit 2
    fi
fi

echo "run-dag.sh: lane=$lane runner=$runner verb=$verb" >&2
if [[ -n $filtered_dag ]]; then
    "$runner" "$verb" --dag "$dag" "$@"
    exit $?
fi
exec "$runner" "$verb" --dag "$dag" "$@"
