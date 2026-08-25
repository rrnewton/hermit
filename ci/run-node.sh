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
#   RUN_NODE_PRINT_ONLY  with `--`, write the scratch DAG, print the edited node
#                        command on stdout and exit 0 WITHOUT running it. This
#                        exists so ci/run-node-args-test.sh can assert the exact
#                        edited command without needing that node's build
#                        artifacts; it says on stderr that nothing was executed,
#                        because a runner that silently runs nothing is the very
#                        failure this `--` support was added to remove.
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

# ⚠️ CARRY THE LANE'S CPU BUDGET, WHICH THIS ENTRYPOINT WAS SILENTLY DROPPING.
#
# Shipped lane nodes declare `cpu_timeout` on 0 of 56 (portable) and 0 of 9
# (privileged), so the budget comes entirely from the caller. scripts/validate.rs
# supplies one — `scripts/lib/validate_plan.rs` sets the DAG-level default to
# LANE_DEFAULT_CPU_TIMEOUT_S and calls it "the one DELIBERATE divergence". This
# script never did, so the runner's own fallback applied instead:
# DEFAULT_SMALL_CPU_TIMEOUT = 10 s (agent-utils/py/dagrun/model.py:47).
#
# The 10 s is a FALLBACK FOR A STEP THAT DECLARES NOTHING, not a limit anybody
# chose for these nodes, and it only bites HERE:
#   * `scripts/validate.rs --show-plan` prints cpu_s = 7200 for every lane node;
#   * in CI this script passes --allow-cgroup-failure, and with no cgroup
#     `cpu.stat` is unreadable so the CPU guard DOES NOT RUN AT ALL
#     (agent-utils/py/dagrun/scheduler.py, cpu_guard docstring);
#   * a local run boxes fail-closed, so the guard runs — against 10 s.
# Measured 2026-08-25: `e2e.metadata` FAIL "12s, CPU-TIMEOUT >10s cpu" while
# `target/debug/test-harness validate` alone exits 0 with "PASS: 13 YAML
# manifests, 305 required cells"; `check.check_outcome_consumers` likewise at
# 11.3 s CPU with both its scripts exiting 0. A targeted runner that reddens work
# the lane passes is worse than no targeted runner.
#
# The DAG document format REFUSES `default_step_cpu_timeout` at the top level on
# purpose (agent-utils/py/dagrun/io.py, UNCARRIED_CONFIG_KEYS: it is caller
# policy, and a key the parser ignores reads exactly like one that took effect).
# So apply it the way the format DOES carry — per step, on the steps that declare
# nothing, leaving any declared budget alone, exactly as `effective_cpu_timeout`
# would. The number is READ FROM validate_plan.rs rather than copied, so this
# entrypoint cannot drift from the lane it is supposed to mirror; if that
# constant is renamed or removed, this fails closed instead of using a stale one.
lane_cpu_timeout=$(sed -n 's/^const LANE_DEFAULT_CPU_TIMEOUT_S: i64 = \([0-9]\{1,\}\);$/\1/p' \
    "$ROOT_DIR/scripts/lib/validate_plan.rs")
if [[ ! $lane_cpu_timeout =~ ^[0-9]+$ ]]; then
    echo "run-node.sh: could not read LANE_DEFAULT_CPU_TIMEOUT_S from scripts/lib/validate_plan.rs." >&2
    echo "            That constant is the lane's per-node CPU budget; without it this entrypoint" >&2
    echo "            would silently fall back to the runner's 10 s default and redden nodes the" >&2
    echo "            lane passes. Refusing rather than guessing." >&2
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
fi

quoted=""
if ((${#append[@]} > 0)); then
    quoted=$(printf ' %q' "${append[@]}")
fi

scratch_dir="$ROOT_DIR/ignored/ci/run-node"
mkdir -p "$scratch_dir" || {
    echo "run-node.sh: could not create scratch dir: $scratch_dir" >&2
    exit 2
}
scratch_dag="$scratch_dir/${lane}.${sel}.effective.json"
RUN_NODE_TAG="$sel" RUN_NODE_APPEND="$quoted" RUN_NODE_CPU_TIMEOUT="$lane_cpu_timeout" \
    python3 -c '
import json, os, sys

source, destination = sys.argv[1], sys.argv[2]
tag = os.environ["RUN_NODE_TAG"]
extra = os.environ["RUN_NODE_APPEND"]
budget = int(os.environ["RUN_NODE_CPU_TIMEOUT"])
dag = json.load(open(source))

def step_tag(step):
    return "{}.{}".format(step.get("group", ""), step.get("job", ""))

# A step that declares its own cpu_timeout keeps it, exactly as
# effective_cpu_timeout would; only the undeclared ones take the lane default.
stamped = 0
for step in dag["steps"]:
    if not step.get("cpu_timeout"):
        step["cpu_timeout"] = budget
        stamped += 1

edited = ""
if extra:
    hits = [s for s in dag["steps"] if step_tag(s) == tag]
    if len(hits) != 1:
        sys.exit(f"run-node.sh: {len(hits)} step(s) match tag {tag!r} in {source}")
    hits[0]["cmd"] += extra
    edited = hits[0]["cmd"]

json.dump(dag, open(destination, "w"), indent=2)
print(stamped)
print(edited)
' "$dag" "$scratch_dag" >"$scratch_dir/.state" || exit 2
dag="$scratch_dag"
stamped=$(sed -n 1p "$scratch_dir/.state")
edited_cmd=$(sed -n 2p "$scratch_dir/.state")
echo "run-node.sh: carried the lane CPU budget onto $stamped undeclared step(s): ${lane_cpu_timeout}s (LANE_DEFAULT_CPU_TIMEOUT_S)" >&2

if [[ -n $quoted ]]; then
    echo "run-node.sh: ⚠️  EDITED NODE COMMAND — iteration evidence only, NOT the tracked node." >&2
    echo "run-node.sh: scratch DAG: $scratch_dag" >&2
    echo "run-node.sh: $sel now runs: $edited_cmd" >&2
    if [[ -n ${RUN_NODE_PRINT_ONLY:-} ]]; then
        echo "run-node.sh: RUN_NODE_PRINT_ONLY set — the edited command above was NOT executed." >&2
        printf '%s\n' "$edited_cmd"
        exit 0
    fi
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
