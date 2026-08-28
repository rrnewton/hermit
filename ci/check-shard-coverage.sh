#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# check-shard-coverage.sh — fail-closed correspondence guard for the parallel
# portable fan-out. Asserts that ci/portable-shards.json assigns EVERY step in
# validate's constructed portable-only plan to exactly one job, with no overlap
# and no unknown step names:
#
#   union(preflight, builds, test shards, e2e, final)
#     == { steps returned by scripts/validate.rs portable-only --show-plan-json }
#
# The immutable E2E artifact is deliberately assigned to the integration shard
# beside test.applications_e2e. Keeping the producer and its protected consumer
# in one run-node selection preserves their declared DAG edge; assigning the
# producer to an unrelated bucket would satisfy set coverage while leaving the
# artifact off the consumer's execution path.
#
# Preflight has no earlier hosted job from which it can fetch an artifact or
# inherit a successful predecessor result. Its selected steps must therefore be
# closed over their dependencies in the constructed plan. This catches a map
# that assigns every step exactly once but separates a preflight command from
# the step that makes it runnable.
#
# The later check job may use either a predecessor in its own selection or one
# that the successful preflight job already supplied. Refuse any other omitted
# dependency so splitting the jobs cannot turn a command into a vacuous red or
# green.
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

shards="ci/portable-shards.json"
command -v jq >/dev/null 2>&1 || { echo "check-shard-coverage.sh: jq is required" >&2; exit 2; }
[[ -f $shards ]] || { echo "check-shard-coverage.sh: missing $shards" >&2; exit 2; }

# Ask the same plan constructor the runner uses. The command is inert, may run
# inside validate, and emits its JSON as the first stdout line.
plan_out=$(mktemp)
trap 'rm -f "$plan_out"' EXIT
./scripts/validate.rs portable-only --show-plan-json \
    --skip-inner-dirty-working-tree-and-rebase-freshness-checks >"$plan_out"
plan_json=$(sed -n '1p' "$plan_out")
jq -e '.profile == "portable-only" and .selection_mode == "full"' \
    <<<"$plan_json" >/dev/null || {
    echo "check-shard-coverage.sh: validate did not return the full portable-only plan" >&2
    exit 2
}
mapfile -t expected < <(jq -r '.dags[].steps[].tag' <<<"$plan_json" | sort -u)

# Every node assigned by the shard map, across all job buckets.
mapfile -t assigned < <(
    jq -r '
        (.preflight_nodes // [])
      + (.check_nodes // [])
      + (.build_debug_nodes // [])
      + (.build_dbt_nodes // [])
      + (.build_aux_nodes // [])
      + (.e2e_nodes // [])
      + (.final_nodes // [])
      + ([ (.debug_shards // [])[]   | .nodes[] ])
      + ([ (.release_shards // [])[] | .nodes[] ])
        | .[]
    ' "$shards" | sort
)

# Duplicate assignment (a node in two buckets) is a defect.
dupes=$(printf '%s\n' "${assigned[@]}" | uniq -d || true)
if [[ -n $dupes ]]; then
    echo "check-shard-coverage.sh: FAIL — node(s) assigned to more than one job:" >&2
    printf '  %s\n' $dupes >&2
    exit 1
fi

assigned_unique=$(printf '%s\n' "${assigned[@]}" | sort -u)
expected_list=$(printf '%s\n' "${expected[@]}")

missing=$(comm -23 <(printf '%s\n' "$expected_list") <(printf '%s\n' "$assigned_unique") || true)
extra=$(comm -13 <(printf '%s\n' "$expected_list") <(printf '%s\n' "$assigned_unique") || true)

status=0
if [[ -n $missing ]]; then
    echo "check-shard-coverage.sh: FAIL — portable nodes NOT assigned to any job:" >&2
    printf '  %s\n' $missing >&2
    status=1
fi
if [[ -n $extra ]]; then
    echo "check-shard-coverage.sh: FAIL — shard map names steps absent from the constructed portable-only plan:" >&2
    printf '  %s\n' $extra >&2
    status=1
fi

preflight_json=$(jq -c '.preflight_nodes // []' "$shards")
preflight_missing=$(jq -r --argjson selected "$preflight_json" '
    [
      .dags[].steps[]
      | select(.tag as $tag | $selected | index($tag))
      | .deps[]
      | select(. as $dependency | ($selected | index($dependency)) == null)
    ]
    | unique[]
' <<<"$plan_json")
if [[ -n $preflight_missing ]]; then
    echo "check-shard-coverage.sh: FAIL — preflight drops constructed predecessor(s) that no earlier job supplies:" >&2
    printf '  %s\n' $preflight_missing >&2
    status=1
fi

check_json=$(jq -c '.check_nodes // []' "$shards")
check_supplied_json=$(jq -c '(.preflight_nodes // []) + (.check_nodes // [])' "$shards")
check_missing=$(jq -r --argjson selected "$check_json" --argjson supplied "$check_supplied_json" '
    [
      .dags[].steps[]
      | select(.tag as $tag | $selected | index($tag))
      | .deps[]
      | select(. as $dependency | ($supplied | index($dependency)) == null)
    ]
    | unique[]
' <<<"$plan_json")
if [[ -n $check_missing ]]; then
    echo "check-shard-coverage.sh: FAIL — check job drops constructed predecessor(s) not supplied by preflight:" >&2
    printf '  %s\n' $check_missing >&2
    status=1
fi

if ! jq -e '
    [.debug_shards[] | select(.slug == "integration") | .nodes] as $integration
    | ($integration | length) == 1
      and ($integration[0] | index("build.e2e_artifact") != null)
      and ($integration[0] | index("test.applications_e2e") != null)
' "$shards" >/dev/null; then
    echo "check-shard-coverage.sh: FAIL — integration job must run build.e2e_artifact with test.applications_e2e" >&2
    status=1
fi

if ((status == 0)); then
    n=$(printf '%s\n' "$assigned_unique" | grep -c . || true)
    echo "check-shard-coverage.sh: OK — $n constructed portable-only steps each assigned to exactly one hosted job."
fi
exit "$status"
