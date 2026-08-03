#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# run-node.sh — execute one or more named DAG nodes UNDER safe-ci-dag-runner,
# WITHOUT running their (already-satisfied) dependencies.
#
# WHY: the parallel GitHub fan-out (ci-portable.yml) shards the portable lane
# across many jobs. Each hosted job is its own ephemeral, isolated VM — that VM
# boundary IS the containment box: a runaway step can only harm its own throwaway
# machine, and GitHub's per-job timeout + VM teardown kill it cleanly. GitHub
# Actions stays the CROSS-MACHINE scheduler; this wrapper makes the runner own
# execution WITHIN each shard so that ALL portable CI compute runs under
# safe-ci-dag-runner — gaining per-node profiling, per-node wall-clock timeouts,
# and setsid-proof teardown — instead of raw `bash -c`.
#
# HOW: `safe-ci-dag-runner run --only <a,b,...>` runs exactly the named nodes and
# DROPS dependency edges to steps outside the selection (their outputs are
# assumed present from an upstream build job or a restored artifact) — the
# runner-native equivalent of the retired jq shim, and dependency-correct for any
# intra-selection edges. Node commands stay defined ONLY in ci/dag/<lane>.json
# (the runner reads them straight from there), so no cargo lines are hand-copied
# and nothing drifts.
#
# BOXING NOTE: the runner's cgroup boxing is ON by default but self-skips under
# GITHUB_ACTIONS (no per-user systemd scope on hosted runners, and the per-job VM
# already provides isolation). On a shared, long-lived machine (self-hosted
# privileged CI, or validate.sh on a dev box) the same runner DOES box each node.
# See ci/dag/README.md and the safe-ci-dag-runner cgroup module.
#
# Usage:
#   ci/run-node.sh <lane> <group.job>[,<group.job>...]
#     <lane>   portable | privileged  (selects ci/dag/<lane>.json)
#     nodes    one or more "group.job" keys, comma-separated. Dependencies
#              OUTSIDE the selection are assumed already built.
#
# Environment:
#   SAFE_CI_DAG_RUNNER          override the runner executable to use.
#   SAFE_CI_DAG_RUNNER_PROFILE_DIR
#                               where per-node profiles are written
#                               (default: $RUNNER_TEMP/hermit-ci-perf, else
#                                $ROOT_DIR/.ci-perf). Uploaded by the workflow so
#                               per-node timing/resource history flows to ci-hub.
#   SAFE_CI_DAG_RUNNER_SHARD_JOBS
#                               within-shard concurrency (default: 1, matching
#                               the historical serial-within-shard behavior; the
#                               cross-machine fan-out is the real parallelism).
#
# Example:
#   ci/run-node.sh portable test.hermit_unit,test.detcore_unit
set -uo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR" || exit 2

lane=${1:-}
sel=${2:-}
if [[ -z $lane || -z $sel ]]; then
    echo "usage: ci/run-node.sh <lane> <group.job>[,<group.job>...]" >&2
    exit 2
fi

dag="$ROOT_DIR/ci/dag/${lane}.json"
if [[ ! -f $dag ]]; then
    echo "run-node.sh: unknown lane '$lane' (no such file: $dag)" >&2
    echo "            known lanes: portable, privileged" >&2
    exit 2
fi

# Locate the runner: explicit override, then the compiled Rust binary (fast), then
# the Python entrypoint (build-free; the shipped symlink fixes its own sys.path).
# Mirrors ci/run-dag.sh::find_runner so both entrypoints resolve identically.
find_runner() {
    if [[ -n ${SAFE_CI_DAG_RUNNER:-} ]]; then
        printf '%s\n' "$SAFE_CI_DAG_RUNNER"
        return 0
    fi
    local base="$ROOT_DIR/agent-utils"
    if [[ -x "$base/rs/bin/safe-ci-dag-runner" ]]; then
        printf '%s\n' "$base/rs/bin/safe-ci-dag-runner"
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

# Per-node profiles land in a stable per-run directory the workflow uploads, so
# per-node timing/resource history flows to the ci-hub store.
perf_dir=${SAFE_CI_DAG_RUNNER_PROFILE_DIR:-${RUNNER_TEMP:+$RUNNER_TEMP/hermit-ci-perf}}
perf_dir=${perf_dir:-$ROOT_DIR/.ci-perf}
mkdir -p "$perf_dir" 2>/dev/null || true

# Within-shard concurrency defaults to serial (the fan-out across VMs is the real
# parallelism, and shards were sized assuming serial resource use); overridable.
shard_jobs=${SAFE_CI_DAG_RUNNER_SHARD_JOBS:-1}

# Boxing acknowledgement. The runner treats cgroup boxing as mandatory and FAILS CLOSED
# (exit 3) rather than run advisory-only. On a hosted GitHub runner there is no per-user
# systemd scope, so the runner self-skips the cgroup re-exec; the ephemeral per-job VM is the
# real isolation box (a runaway can only harm its own throwaway machine, and GitHub's per-job
# timeout + VM teardown kill it cleanly). We therefore acknowledge UNBOXED-within-VM explicitly
# on hosted CI — profiling, per-node wall-clock timeouts, and setsid-proof teardown still apply.
# On a shared, long-lived machine (self-hosted / dev box) we do NOT pass this, so the runner
# boxes each node with cgroups (and fails closed if that is misconfigured — the correct signal).
box_args=()
if [[ -n ${GITHUB_ACTIONS:-} || -n ${CI:-} ]]; then
    box_args+=(--allow-cgroup-failure)
fi

echo "run-node.sh: lane=$lane runner=$runner nodes=$sel perf-dir=$perf_dir -j$shard_jobs ${box_args[*]}" >&2
exec "$runner" run --dag "$dag" --only "$sel" \
    -j "$shard_jobs" --perf-dir "$perf_dir" "${box_args[@]}" -v
