#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# run-node.sh — run one or more named DAG nodes from ci/dag/<lane>.json THROUGH
# the tracked safe-ci-dag-runner, WITHOUT running their dependencies.
#
# WHY (single-engine invariant): the parallel GitHub fan-out (ci-portable.yml)
# shards the portable lane across many small jobs. Each shard must execute an
# exact subset of the DAG against a prebuilt tree / restored cache produced by an
# upstream build job. Historically this shim RE-IMPLEMENTED node execution in
# jq+bash (extract each node's `.cmd`, `bash -c` it) because the pinned runner
# predated its `run --only` node selector. That made GitHub Actions a SECOND
# execution engine that diverged from the runner: it ignored each node's
# jobs_flag, timeout, cpu_timeout, and cgroup boxing. This rewrite kills that
# divergence — every node now runs through the SAME `safe-ci-dag-runner run`
# entrypoint that scripts/validate.rs and run-dag.sh use, so ci/dag/<lane>.json is the
# single source of truth for both the command AND its resource policy.
#
# REQUIRES the pinned agent-utils runner to support `run --only` (added upstream
# in the same commit as the tracked common/bin engine resolver and
# --allow-cgroup-failure). At an older pin this script fails closed (argparse
# rejects --only), which is intentional: the run-node.sh rewrite and the
# agent-utils gitlink advance are a COUPLED change and must land together.
#
# Usage:
#   ci/run-node.sh <lane> <group.job>[,<group.job>...]
#     <lane>   portable | privileged  (selects ci/dag/<lane>.json)
#     nodes    one or more "group.job" tags, comma-separated. Passed verbatim to
#              `run --only`: the runner executes EXACTLY those steps (dependency
#              edges to steps OUTSIDE the selection are dropped — their outputs
#              are assumed already present from an upstream build/cache job),
#              while edges AMONG the selected steps are preserved so a selected
#              sub-graph still runs in the right order.
#
# Environment:
#   SAFE_CI_DAG_RUNNER   override the runner executable (mirrors run-dag.sh).
#   RUN_NODE_JOBS        outer concurrency across the selected nodes (default 1:
#                        one node at a time, preserving the historical serial
#                        shim semantics). Inner per-node parallelism (cargo /
#                        nextest -j) comes from each node's jobs_flag in the DAG,
#                        independent of this.
#   RUN_NODE_PERF_DIR    directory for per-step + whole-run resource-usage CSVs
#                        (default ignored/ci/perf/run-node/<lane>). CI uploads it
#                        as a per-shard performance artifact.
#
# Example:
#   ci/run-node.sh portable test.hermit_unit,test.detcore_unit

set -uo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR" || exit 2

# shellcheck source=ci/configure-build-jobs.sh
source "$ROOT_DIR/ci/configure-build-jobs.sh" launcher || exit $?

lane=${1:-}
sel=${2:-}
if [[ -z $lane || -z $sel ]]; then
    echo "usage: ci/run-node.sh <lane> <group.job>[,<group.job>...]" >&2
    exit 2
fi

dag="$ROOT_DIR/ci/dag/${lane}.json"
if [[ ! -f $dag ]]; then
    echo "run-node.sh: unknown lane '$lane' (no such file: $dag)" >&2
    exit 2
fi

# Locate the runner. Mirror ci/run-dag.sh's find_runner EXACTLY: an explicit
# override, then the TRACKED, source-invoked engine resolver
# (agent-utils/common/bin/safe-ci-dag-runner), then the tracked, source-invoked
# Python entrypoint (agent-utils/py/bin), then a resolver already on PATH. NEVER
# auto-select the untracked prebuilt Rust binary (rs/bin): a compiled artifact
# can silently drift from its source (the historical cpu_timeout gap), and the
# CI execution path must stay tracked, deterministic, and self-describing.
find_runner() {
    if [[ -n ${SAFE_CI_DAG_RUNNER:-} ]]; then
        printf '%s\n' "$SAFE_CI_DAG_RUNNER"
        return 0
    fi
    local base="$ROOT_DIR/agent-utils"
    if [[ -x "$base/common/bin/safe-ci-dag-runner" ]]; then
        printf '%s\n' "$base/common/bin/safe-ci-dag-runner"
        return 0
    fi
    if [[ -x "$base/py/bin/safe-ci-dag-runner" ]]; then
        printf '%s\n' "$base/py/bin/safe-ci-dag-runner"
        return 0
    fi
    if command -v safe-ci-dag-runner >/dev/null 2>&1; then
        command -v safe-ci-dag-runner
        return 0
    fi
    return 1
}

runner=$(find_runner) || {
    echo "run-node.sh: safe-ci-dag-runner not found." >&2
    echo "            Build it with: (cd agent-utils && ./setup) or set SAFE_CI_DAG_RUNNER." >&2
    exit 2
}

jobs=${RUN_NODE_JOBS:-1}
perf_dir=${RUN_NODE_PERF_DIR:-"$ROOT_DIR/ignored/ci/perf/run-node/${lane}"}
mkdir -p "$perf_dir" || {
    echo "run-node.sh: could not create perf dir: $perf_dir" >&2
    exit 2
}

# Boxing policy. safe-ci-dag-runner boxes fail-closed by default (two-level
# cgroup-v2 + a systemd --user scope, or it exits 3). Inside GitHub Actions the
# runner deliberately SKIPS the systemd --user scope (its skip_in_ci path keys on
# $GITHUB_ACTIONS / $CI), so the default no-opt-out path would exit 3 in ANY
# Actions context. run-node.sh is used ONLY by the hosted, EPHEMERAL ubuntu
# portable lane — where the throwaway VM IS the containment boundary — so we opt
# out of fail-closed boxing there with --allow-cgroup-failure. A local developer
# run (no $GITHUB_ACTIONS / $CI) still boxes fail-closed.
acf=()
if [[ -n ${GITHUB_ACTIONS:-} || -n ${CI:-} ]]; then
    acf=(--allow-cgroup-failure)
fi

echo "run-node.sh: lane=$lane runner=$runner nodes=$sel -j$jobs cargo-jobs=$CARGO_BUILD_JOBS reverie-dbt-budget=portable-build-child-only perf-dir=$perf_dir${acf+ (unboxed: ephemeral CI VM)}" >&2

# A strict-compat node is a COMPOSITE Rust driver: this Python runner supervises
# scripts/validate.rs, which in turn supervises the individual compatibility
# probes through the Rust runner.  At the old agent-utils pin an unboxed probe
# could leave a setsid child holding stdout/stderr open; the Rust runner detected
# its timeout and then blocked forever joining the pipe readers.  The outer
# Python runner had a second blind spot: its streaming reader calls flush while
# holding the scheduler lock, so Actions output backpressure can keep a detected
# 1800-second timeout from reaching terminal reporting.  Actions then kills the
# whole job at its 40-minute ceiling with neither runner's verdict preserved.
#
# The first fix still piped the runner through gawk.  A live four-shard run proved
# that insufficient: all four outer 1800-second steps remained alive beyond 32
# minutes.  Do not put ANY consumer between the runner and a regular file.  An
# independent GNU-timeout process supervises the runner, while a best-effort
# follower timestamps the growing regular file without being in the runner's
# output path.  Thus a blocked Actions log, timestamp follower, or inherited pipe
# cannot prevent the watchdog from becoming terminal.  The watchdog allowance is
# the DAG's real inner timeout plus 60 seconds for the runner's bounded teardown.
# The always() artifact step in ci-portable.yml uploads both logs.
if [[ -n ${GITHUB_ACTIONS:-} && $sel == test.strict_compat_* ]]; then
    safe_sel=${sel//[^a-zA-Z0-9_.-]/_}
    raw_log="$perf_dir/run-node-${safe_sel}.raw.log"
    phase_log="$perf_dir/run-node-${safe_sel}.timestamped.log"
    inner_timeout=$(jq -er --arg sel "$sel" '
        .steps[] | select((.group + "." + .job) == $sel) | .timeout
    ' "$dag") || {
        echo "run-node.sh: cannot resolve strict composite timeout for $sel" >&2
        exit 2
    }
    if [[ ! $inner_timeout =~ ^[1-9][0-9]*$ ]]; then
        echo "run-node.sh: invalid strict composite timeout for $sel: $inner_timeout" >&2
        exit 2
    fi
    watchdog_grace=60
    watchdog_timeout=$((inner_timeout + watchdog_grace))
    : >"$raw_log"
    : >"$phase_log"
    echo "run-node.sh: strict composite raw log: $raw_log" >&2
    echo "run-node.sh: strict composite timestamp log: $phase_log" >&2
    echo "run-node.sh: strict composite watchdog: ${watchdog_timeout}s (${inner_timeout}s DAG timeout + ${watchdog_grace}s bounded teardown)" >&2

    set +e
    timeout --signal=TERM --kill-after=30s "${watchdog_timeout}s" \
        "$runner" run --dag "$dag" --only "$sel" -j "$jobs" \
        --perf-dir "$perf_dir" "${acf[@]}" -v >"$raw_log" 2>&1 &
    runner_pid=$!
    tail --pid="$runner_pid" --sleep-interval=0.2 -n +1 -F "$raw_log" |
        TZ=UTC gawk -v out="$phase_log" '
            {
                stamp = strftime("%Y-%m-%dT%H:%M:%SZ", systime())
                print stamp, $0 >> out
                fflush(out)
            }
        ' &
    logger_pid=$!

    wait "$runner_pid"
    runner_rc=$?
    logger_deadline=$((SECONDS + 5))
    while kill -0 "$logger_pid" 2>/dev/null && (( SECONDS < logger_deadline )); do
        sleep 1
    done
    if kill -0 "$logger_pid" 2>/dev/null; then
        echo "run-node.sh: timestamp follower did not close after runner exit; terminating exact child pid=$logger_pid" >&2
        kill -TERM "$logger_pid"
        wait "$logger_pid"
        logger_rc=124
    else
        wait "$logger_pid"
        logger_rc=$?
    fi
    set -e

    grep -E '▶ START|✓ PASS|✗ FAIL|⊘ ABORT|TIMEOUT|WARNING|validate: durable log|validate PASS|validate FAIL|safe-ci-dag-runner:' \
        "$phase_log" | tail -n 400 >&2 || true
    if (( logger_rc != 0 )); then
        echo "run-node.sh: timestamp follower failed with rc=$logger_rc; refusing an unlogged verdict" >&2
        tail -n 200 "$raw_log" >&2
        exit "$logger_rc"
    fi
    if (( runner_rc == 124 )); then
        echo "run-node.sh: strict composite watchdog expired after ${watchdog_timeout}s (DAG timeout ${inner_timeout}s); runner output was bounded and retained" >&2
    fi
    if (( runner_rc != 0 )); then
        echo "run-node.sh: strict composite failed with rc=$runner_rc; final timestamped detail follows" >&2
        tail -n 200 "$phase_log" >&2
    fi
    exit "$runner_rc"
fi

exec "$runner" run --dag "$dag" --only "$sel" -j "$jobs" --perf-dir "$perf_dir" "${acf[@]}" -v
