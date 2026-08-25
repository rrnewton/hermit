#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# run-node.sh — run one or more named DAG nodes from ci/dag/<lane>.json THROUGH
# the tracked dagrun, WITHOUT running their dependencies.
#
# WHY (single-engine invariant): the parallel GitHub fan-out (ci-portable.yml)
# shards the portable lane across many small jobs. Each shard must execute an
# exact subset of the DAG against a prebuilt tree / restored cache produced by an
# upstream build job. Historically this shim RE-IMPLEMENTED node execution in
# jq+bash (extract each node's `.cmd`, `bash -c` it) because the pinned runner
# predated its `run --only` node selector. That made GitHub Actions a SECOND
# execution engine that diverged from the runner: it ignored each node's
# jobs_flag, timeout, cpu_timeout, and cgroup boxing. This rewrite kills that
# divergence — every node now runs through the SAME `dagrun run`
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
#   ci/run-node.sh <lane> <group.job>[,<group.job>...] [-- <extra args>]
#     <lane>   portable | privileged  (selects ci/dag/<lane>.json)
#     nodes    one or more "group.job" tags, comma-separated. Passed verbatim to
#              `run --only`: the runner executes EXACTLY those steps (dependency
#              edges to steps OUTSIDE the selection are dropped — their outputs
#              are assumed already present from an upstream build/cache job),
#              while edges AMONG the selected steps are preserved so a selected
#              sub-graph still runs in the right order.
#     -- args  RUN ONE TEST INSIDE ONE NODE. Everything after `--` is appended,
#              shell-quoted, to the END of that node's tracked command line, and
#              the node runs from a scratch DAG holding only that edit. Requires
#              a SINGLE node tag, and refuses under $CI/$GITHUB_ACTIONS.
#
# ⚠️ WHY `--` EXISTS, AND WHY IT SHOUTS. Before it, this script took exactly two
# positional arguments and IGNORED any further ones. So the natural attempt at a
# targeted test —
#     ci/run-node.sh portable test.detcore_unit -E 'test(=cpuid_leaf_count)'
# — silently ran the WHOLE node, all 534 tests, and printed PASS. The filter was
# swallowed, and the operator read a full-node green as a one-test green. That is
# a mechanism producing a value that reads as information and carries none, so
# unrecognised trailing arguments are now a hard usage error, and the one form
# that IS supported announces the exact edited command before running it.
#
# ⚠️ AND AN EDITED NODE IS NOT THE NODE. The scratch DAG keeps the node's own
# boxing, jobs_flag, timeout and cpu_timeout — that is the whole point of going
# through dagrun rather than hand-copying the cmd — but the command it runs is no
# longer the tracked one, so its result is ITERATION EVIDENCE ONLY. It cannot
# stand in for the node in CI, and nothing here writes a receipt.
#
# The appended text lands at the end of the line VERBATIM. Knowing what that
# means for a given node is the caller's job: for a nextest node ending in
# `-- --skip X` the trailing position is already inside the test binary's own
# argument list, so a nextest-level `-E` filter must not be appended there.
#
# Environment:
#   DAGRUN_BIN   override the runner executable (mirrors run-dag.sh).
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
#   ci/run-node.sh portable test.detcore_unit -- -E 'test(=cpuid::tests::cpuid_leaf_count)'

set -uo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR" || exit 2

# shellcheck source=ci/configure-build-jobs.sh
source "$ROOT_DIR/ci/configure-build-jobs.sh" launcher || exit $?

usage() {
    echo "usage: ci/run-node.sh <lane> <group.job>[,<group.job>...] [-- <args appended to one node's cmd>]" >&2
}

lane=${1:-}
sel=${2:-}
if [[ -z $lane || -z $sel ]]; then
    usage
    exit 2
fi
shift 2

# Fail closed on anything else. Silently ignoring trailing arguments is what let
# a swallowed test filter read as a targeted green; see the header.
append=()
if (($# > 0)); then
    if [[ $1 != "--" ]]; then
        echo "run-node.sh: unexpected argument '$1'. Node-command arguments must follow a literal '--'." >&2
        usage
        exit 2
    fi
    shift
    if (($# == 0)); then
        echo "run-node.sh: '--' given with nothing after it." >&2
        exit 2
    fi
    append=("$@")
fi

dag="$ROOT_DIR/ci/dag/${lane}.json"
if [[ ! -f $dag ]]; then
    echo "run-node.sh: unknown lane '$lane' (no such file: $dag)" >&2
    exit 2
fi

# One test inside one node: run the node's own boxing and limits over an edited
# command. Refused in CI, refused for a multi-node selection, and never silent.
if ((${#append[@]} > 0)); then
    if [[ -n ${GITHUB_ACTIONS:-} || -n ${CI:-} ]]; then
        echo "run-node.sh: '--' node-command arguments are a local iteration aid and are refused in CI." >&2
        echo "            CI must run the tracked command; edit ci/dag/${lane}.json instead." >&2
        exit 2
    fi
    if [[ $sel == *,* ]]; then
        echo "run-node.sh: '--' requires exactly one node tag, got '$sel'." >&2
        exit 2
    fi
    quoted=$(printf ' %q' "${append[@]}")
    scratch_dir="$ROOT_DIR/ignored/ci/run-node"
    mkdir -p "$scratch_dir" || {
        echo "run-node.sh: could not create scratch dir: $scratch_dir" >&2
        exit 2
    }
    scratch_dag="$scratch_dir/${lane}.${sel}.edited.json"
    RUN_NODE_TAG="$sel" RUN_NODE_APPEND="$quoted" \
        python3 -c '
import json, os, sys

source, destination = sys.argv[1], sys.argv[2]
tag = os.environ["RUN_NODE_TAG"]
extra = os.environ["RUN_NODE_APPEND"]
dag = json.load(open(source))
def step_tag(step):
    return "{}.{}".format(step.get("group", ""), step.get("job", ""))

hits = [s for s in dag["steps"] if step_tag(s) == tag]
if len(hits) != 1:
    sys.exit(f"run-node.sh: {len(hits)} step(s) match tag {tag!r} in {source}")
hits[0]["cmd"] += extra
json.dump(dag, open(destination, "w"), indent=2)
print(hits[0]["cmd"])
' "$dag" "$scratch_dag" >"$scratch_dir/.cmd" || exit 2
    dag="$scratch_dag"
    echo "run-node.sh: ⚠️  EDITED NODE COMMAND — iteration evidence only, NOT the tracked node." >&2
    echo "run-node.sh: scratch DAG: $scratch_dag" >&2
    echo "run-node.sh: $sel now runs: $(cat "$scratch_dir/.cmd")" >&2
fi

# Locate the runner. Mirror ci/run-dag.sh's find_runner EXACTLY: an explicit
# override, then the TRACKED, source-invoked engine resolver
# (agent-utils/common/bin/dagrun), then the tracked, source-invoked
# Python entrypoint (agent-utils/py/bin), then a resolver already on PATH. NEVER
# auto-select the untracked prebuilt Rust binary (rs/bin): a compiled artifact
# can silently drift from its source (the historical cpu_timeout gap), and the
# CI execution path must stay tracked, deterministic, and self-describing.
find_runner() {
    if [[ -n ${DAGRUN_BIN:-} ]]; then
        printf '%s\n' "$DAGRUN_BIN"
        return 0
    fi
    local base="$ROOT_DIR/agent-utils"
    if [[ -x "$base/common/bin/dagrun" ]]; then
        printf '%s\n' "$base/common/bin/dagrun"
        return 0
    fi
    if [[ -x "$base/py/bin/dagrun" ]]; then
        printf '%s\n' "$base/py/bin/dagrun"
        return 0
    fi
    if command -v dagrun >/dev/null 2>&1; then
        command -v dagrun
        return 0
    fi
    return 1
}

runner=$(find_runner) || {
    echo "run-node.sh: dagrun not found." >&2
    echo "            Build it with: (cd agent-utils && ./setup) or set DAGRUN_BIN." >&2
    exit 2
}

jobs=${RUN_NODE_JOBS:-1}
perf_dir=${RUN_NODE_PERF_DIR:-"$ROOT_DIR/ignored/ci/perf/run-node/${lane}"}
mkdir -p "$perf_dir" || {
    echo "run-node.sh: could not create perf dir: $perf_dir" >&2
    exit 2
}

# Boxing policy. dagrun boxes fail-closed by default (two-level
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
exec "$runner" run --dag "$dag" --only "$sel" -j "$jobs" --perf-dir "$perf_dir" "${acf[@]}" -v
