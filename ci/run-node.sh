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

selection_requires_validation_capabilities() {
    local selection=$1 tag
    local -a tags=()
    IFS=, read -r -a tags <<<"$selection"
    for tag in "${tags[@]}"; do
        case "$tag" in
            e2e.manifest_* | \
            test.applications_e2e | test.hermit_integration | \
            test.arbitrary_binaries | test.cli | test.liteinst_strict | \
            test.sabre_examples | test.hermit_modes | \
            test.app_strict_verify | test.command_strict_verify | \
            test.ignored_syscall_regressions | test.dbt_parity | \
            test.envelope_levels | test.strict_compat)
                return 0
                ;;
        esac
    done
    return 1
}

validation_capability_profile() {
    local selection=$1 tag saw_debug=0 saw_release=0
    local -a tags=()
    IFS=, read -r -a tags <<<"$selection"
    for tag in "${tags[@]}"; do
        case "$tag" in
            test.dbt_parity | test.sabre_examples | test.liteinst_strict | \
            test.strict_compat)
                saw_release=1
                ;;
            e2e.manifest_* | \
            test.applications_e2e | test.hermit_integration | \
            test.arbitrary_binaries | test.cli | test.hermit_modes | \
            test.app_strict_verify | test.command_strict_verify | \
            test.ignored_syscall_regressions | test.envelope_levels)
                saw_debug=1
                ;;
        esac
    done
    if ((saw_debug && saw_release)); then
        echo "run-node.sh: one shard mixed debug- and release-backed validation consumers" >&2
        return 1
    fi
    ((saw_release)) && printf 'release\n' || printf 'debug\n'
}

attested_validation_hermit() {
    local root=$1 selection=$2 profile binary attestation
    profile=$(validation_capability_profile "$selection") || return 1
    binary="$root/target/$profile/hermit"
    attestation="$root/target/ci/hermit-$profile.sha256"
    [[ -x $binary ]] || {
        echo "run-node.sh: selected $profile validation binary is missing or not executable: $binary" >&2
        return 1
    }
    [[ -f $attestation ]] || {
        echo "run-node.sh: selected $profile validation binary has no build attestation: $attestation" >&2
        return 1
    }
    (cd "$root" && sha256sum -c "target/ci/hermit-$profile.sha256" >/dev/null) || {
        echo "run-node.sh: selected $profile validation binary is stale relative to $attestation" >&2
        return 1
    }
    printf '%s\n' "$binary"
}

if [[ ${1:-} == --self-test ]]; then
    selection_requires_validation_capabilities test.sabre_examples || {
        echo "run-node.sh self-test: changed portable consumer was not guarded" >&2
        exit 1
    }
    selection_requires_validation_capabilities \
        build.workspace,check.validation_capabilities && {
        echo "run-node.sh self-test: build/check-only selection acquired runtime prerequisites" >&2
        exit 1
    }
    selection_requires_validation_capabilities \
        test.detcore_unit,e2e.manifest_system-utils || {
        echo "run-node.sh self-test: mixed selection lost its guarded manifest consumer" >&2
        exit 1
    }
    scratch=$(mktemp -d "${TMPDIR:-/tmp}/run-node-self-test.XXXXXX")
    trap 'rm -rf -- "$scratch"' EXIT
    mkdir -p "$scratch/target/debug" "$scratch/target/release" "$scratch/target/ci"
    printf 'debug\n' >"$scratch/target/debug/hermit"
    printf 'release\n' >"$scratch/target/release/hermit"
    chmod 755 "$scratch/target/debug/hermit" "$scratch/target/release/hermit"
    (cd "$scratch" && sha256sum target/debug/hermit >target/ci/hermit-debug.sha256)
    (cd "$scratch" && sha256sum target/release/hermit >target/ci/hermit-release.sha256)
    [[ $(attested_validation_hermit "$scratch" test.hermit_integration) == \
        "$scratch/target/debug/hermit" ]] || {
        echo "run-node.sh self-test: debug shard did not select its attested debug binary" >&2
        exit 1
    }
    [[ $(attested_validation_hermit "$scratch" test.liteinst_strict) == \
        "$scratch/target/release/hermit" ]] || {
        echo "run-node.sh self-test: release shard did not select its attested release binary" >&2
        exit 1
    }
    [[ $(attested_validation_hermit "$scratch" test.strict_compat) == \
        "$scratch/target/release/hermit" ]] || {
        echo "run-node.sh self-test: strict compatibility did not attest its release Hermit" >&2
        exit 1
    }
    printf 'stale\n' >>"$scratch/target/debug/hermit"
    if attested_validation_hermit "$scratch" test.hermit_integration >/dev/null 2>&1; then
        echo "run-node.sh self-test: stale debug binary passed its build attestation" >&2
        exit 1
    fi
    rm -f -- "$scratch/target/release/hermit"
    if attested_validation_hermit "$scratch" test.liteinst_strict >/dev/null 2>&1; then
        echo "run-node.sh self-test: missing release binary fell back to another profile" >&2
        exit 1
    fi
    if attested_validation_hermit \
        "$scratch" test.hermit_integration,test.liteinst_strict >/dev/null 2>&1; then
        echo "run-node.sh self-test: mixed debug/release shard chose one incomplete attestation" >&2
        exit 1
    fi
    echo "run-node.sh self-test: PASS (2 guarded, 1 build/check refusal; debug/release/strict-compat attestations + stale/missing/mixed refusals)"
    exit 0
fi

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

if [[ $lane == portable ]] && selection_requires_validation_capabilities "$sel"; then
    validation_hermit=$(attested_validation_hermit "$ROOT_DIR" "$sel") || exit 1
    "$ROOT_DIR/ci/check-validation-capabilities.sh" "$validation_hermit" || {
        echo "run-node.sh: portable validation requires PMU overflow delivery and ptrace CPUID interception" >&2
        exit 1
    }
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
exec "$runner" run --dag "$dag" --only "$sel" -j "$jobs" --perf-dir "$perf_dir" "${acf[@]}" -v
