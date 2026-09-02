#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# check-shard-coverage.sh — fail-closed correspondence guard for the parallel
# portable fan-out. Asserts that ci/portable-shards.json assigns EVERY step in
# validate's committed portable label selection to exactly one job, with no overlap
# and no unknown step names:
#
#   union(preflight, builds, test shards, e2e, final)
#     == { steps selected by the portable label from ci/dag/validate.json }
#
# The immutable E2E artifact and the LiteInst producer are deliberately assigned
# to one completed-build job after the debug and release producers. Keeping that
# internal edge preserves the constructed ordering while later test jobs fetch
# the resulting artifact instead of rerunning its command.
#
# Every hosted group must also preserve each constructed predecessor either in
# the same selected group or in an earlier job whose artifacts/results it uses.
# Exact set coverage alone cannot catch an edge that was reversed or dropped.
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

shards="ci/portable-shards.json"
workflow=".github/workflows/ci-portable.yml"
command -v jq >/dev/null 2>&1 || { echo "check-shard-coverage.sh: jq is required" >&2; exit 2; }
[[ -f $shards ]] || { echo "check-shard-coverage.sh: missing $shards" >&2; exit 2; }
[[ -f $workflow ]] || { echo "check-shard-coverage.sh: missing $workflow" >&2; exit 2; }

# Ask the same plan constructor the runner uses. The command is inert, may run
# inside validate, and emits its JSON as the first stdout line.
plan_out=$(mktemp)
trap 'rm -f "$plan_out"' EXIT
./scripts/validate.rs portable-only --show-plan-json \
    --skip-inner-dirty-working-tree-and-rebase-freshness-checks >"$plan_out"
plan_json=$(sed -n '1p' "$plan_out")
jq -e '.profile == "portable" and .selection_mode == "label"' \
    <<<"$plan_json" >/dev/null || {
    echo "check-shard-coverage.sh: validate did not return the committed portable label selection" >&2
    exit 2
}
mapfile -t expected < <(jq -r '.dags[].steps[].tag' <<<"$plan_json" | sort -u)
mapfile -t strict_compat_expansion < <(
    jq -r '
        .dags[].steps[].tag
        | select(. == "compatprep.fixtures" or startswith("compat."))
    ' <<<"$plan_json" | sort -u
)
if ((${#strict_compat_expansion[@]} == 0)); then
    echo "check-shard-coverage.sh: constructed plan has no direct strict compatibility nodes" >&2
    exit 2
fi

# Every selection alias assigned by the shard map, across all job buckets.
mapfile -t assigned_aliases < <(
    jq -r '
        (.preflight_nodes // [])
      + (.check_nodes // [])
      + (.build_debug_nodes // [])
      + (.build_dbt_nodes // [])
      + (.build_aux_nodes // [])
      + (.strict_compat_nodes // [])
      + (.e2e_nodes // [])
      + (.final_nodes // [])
      + ([ (.debug_shards // [])[]   | .nodes[] ])
      + ([ (.release_shards // [])[] | .nodes[] ])
        | .[]
    ' "$shards" | sort
)
strict_alias_count=$(printf '%s\n' "${assigned_aliases[@]}" |
    grep -Fxc 'test.strict_compat' || true)
if [[ $strict_alias_count -ne 1 ]]; then
    echo "check-shard-coverage.sh: FAIL — shard map assigns test.strict_compat $strict_alias_count times; expected exactly one stable alias" >&2
    exit 1
fi
mapfile -t assigned < <(
    {
        printf '%s\n' "${assigned_aliases[@]}" | grep -Fvx 'test.strict_compat'
        printf '%s\n' "${strict_compat_expansion[@]}"
    } | sort
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
    echo "check-shard-coverage.sh: FAIL — shard map names steps absent from the committed portable label selection:" >&2
    printf '  %s\n' $extra >&2
    status=1
fi

dependency_misses() {
    local selected_json=$1
    local supplied_json=$2
    local source_plan=${3:-$plan_json}
    jq -r --argjson selected "$selected_json" --argjson supplied "$supplied_json" '
    [
      .dags[].steps[]
      | select(.tag as $tag | $selected | index($tag))
      | .deps[]
      | select(. as $dependency | ($supplied | index($dependency)) == null)
    ]
    | unique[]
' <<<"$source_plan"
}

# Pin both directions of the dependency guard with a synthetic hosted group.
# The live plan below caught a real omission after build.workspace became a
# predecessor of a check assigned to the pre-build checks job. A guard that only
# happens to reject today's map can silently decay when its jq selection changes;
# this fixture requires the missing edge to be named and the supplied edge to
# clear without relying on any current node identity.
dependency_fixture='{"dags":[{"steps":[{"tag":"check.fixture","deps":["build.fixture"]}]}]}'
fixture_missing=$(dependency_misses '["check.fixture"]' '["check.fixture"]' "$dependency_fixture")
if [[ $fixture_missing != build.fixture ]]; then
    echo "check-shard-coverage.sh: FAIL — dependency guard did not name a planted missing predecessor" >&2
    status=1
fi
fixture_clear=$(dependency_misses \
    '["check.fixture"]' '["check.fixture","build.fixture"]' "$dependency_fixture")
if [[ -n $fixture_clear ]]; then
    echo "check-shard-coverage.sh: FAIL — dependency guard rejected a planted supplied predecessor" >&2
    status=1
fi

workflow_step_body() {
    local step_name=$1 workflow_text=$2
    awk -v marker="      - name: $step_name" '
        $0 == marker { in_step = 1; next }
        in_step && /^      - name:/ { exit }
        in_step { print }
    ' <<<"$workflow_text"
}

debug_artifact_contract() {
    local workflow_text=$1 pack_step unpack_step
    local archive_member='            target/debug/verification-report \'
    pack_step=$(workflow_step_body "Pack debug prebuilt tree" "$workflow_text")
    unpack_step=$(workflow_step_body "Unpack debug tree" "$workflow_text")
    grep -Fqx '          test -x target/debug/verification-report' <<<"$pack_step" &&
        grep -Fqx "$archive_member" <<<"$pack_step" &&
        grep -Fqx '          test -x target/debug/verification-report' <<<"$unpack_step"
}

# check.backend_parity_suites runs target/debug/verification-report after the
# debug tree crosses a job boundary. Guard all three parts of that contract:
# producer existence, archive membership, and executable consumer assertion.
# The mutation bracket proves the guard rejects the original omission instead
# of passing merely because the binary is mentioned somewhere in the workflow.
workflow_text=$(<"$workflow")
if ! debug_artifact_contract "$workflow_text"; then
    echo "check-shard-coverage.sh: FAIL — debug artifact must transport executable target/debug/verification-report" >&2
    status=1
fi
omitted_artifact=${workflow_text/$'            target/debug/verification-report \\\n'/}
if [[ $omitted_artifact == "$workflow_text" ]]; then
    echo "check-shard-coverage.sh: FAIL — artifact omission fixture did not remove verification-report" >&2
    status=1
elif debug_artifact_contract "$omitted_artifact"; then
    echo "check-shard-coverage.sh: FAIL — artifact guard accepted a planted missing verification-report member" >&2
    status=1
fi

check_dependencies() {
    local label=$1 selected_json=$2 supplied_json=$3 missing
    missing=$(dependency_misses "$selected_json" "$supplied_json")
    if [[ -n $missing ]]; then
        echo "check-shard-coverage.sh: FAIL — $label drops constructed predecessor(s) that no earlier job supplies:" >&2
        printf '  %s\n' $missing >&2
        status=1
    fi
}

preflight_json=$(jq -c '.preflight_nodes // []' "$shards")
check_json=$(jq -c '.check_nodes // []' "$shards")
build_debug_json=$(jq -c '.build_debug_nodes // []' "$shards")
build_dbt_json=$(jq -c '.build_dbt_nodes // []' "$shards")
build_aux_json=$(jq -c '.build_aux_nodes // []' "$shards")
strict_compat_json=$(printf '%s\n' "${strict_compat_expansion[@]}" |
    jq -Rsc 'split("\n") | map(select(length > 0))')
through_preflight=$(jq -cn --argjson preflight "$preflight_json" '$preflight')
through_checks=$(jq -cn --argjson preflight "$preflight_json" --argjson checks "$check_json" '$preflight + $checks')
through_debug=$(jq -cn --argjson preflight "$preflight_json" --argjson debug "$build_debug_json" '$preflight + $debug')
through_release=$(jq -cn --argjson preflight "$preflight_json" --argjson release "$build_dbt_json" '$preflight + $release')
through_builds=$(jq -cn \
    --argjson preflight "$preflight_json" \
    --argjson debug "$build_debug_json" \
    --argjson release "$build_dbt_json" \
    --argjson aux "$build_aux_json" \
    '$preflight + $debug + $release + $aux')

check_dependencies "preflight" "$preflight_json" "$through_preflight"
check_dependencies "check job" "$check_json" "$through_checks"
check_dependencies "debug build job" "$build_debug_json" "$through_debug"
check_dependencies "release build job" "$build_dbt_json" "$through_release"
check_dependencies "completed build job" "$build_aux_json" "$through_builds"

debug_test_json=$(jq -c '[.debug_shards[].nodes[]]' "$shards")
strict_compat_supplied=$(jq -cn \
    --argjson prior "$through_builds" \
    --argjson tests "$debug_test_json" \
    --argjson selected "$strict_compat_json" \
    '$prior + $tests + $selected')
check_dependencies "strict compatibility job" "$strict_compat_json" "$strict_compat_supplied"

while IFS= read -r shard; do
    slug=$(jq -r '.slug' <<<"$shard")
    nodes=$(jq -c '.nodes' <<<"$shard")
    supplied=$(jq -cn --argjson prior "$through_builds" --argjson selected "$nodes" '$prior + $selected')
    check_dependencies "debug shard $slug" "$nodes" "$supplied"
done < <(jq -c '.debug_shards[]' "$shards")

while IFS= read -r shard; do
    slug=$(jq -r '.slug' <<<"$shard")
    nodes=$(jq -c '.nodes' <<<"$shard")
    supplied=$(jq -cn --argjson prior "$through_builds" --argjson selected "$nodes" '$prior + $selected')
    check_dependencies "release shard $slug" "$nodes" "$supplied"
done < <(jq -c '.release_shards[]' "$shards")

while IFS= read -r node; do
    selected=$(jq -cn --arg node "$node" '[$node]')
    supplied=$(jq -cn --argjson prior "$through_builds" --argjson selected "$selected" '$prior + $selected')
    check_dependencies "E2E job $node" "$selected" "$supplied"
done < <(jq -r '.e2e_nodes[]' "$shards")

final_json=$(jq -c '.final_nodes // []' "$shards")
all_supplied_json=$(printf '%s\n' "${assigned[@]}" | jq -Rsc 'split("\n") | map(select(length > 0))')
check_dependencies "final job" "$final_json" "$all_supplied_json"

if ! jq -e '
    (.build_aux_nodes // []) as $completed_build
    | ($completed_build | index("build.e2e_artifact") != null)
      and ($completed_build | index("build.liteinst_runtime_release") != null)
' "$shards" >/dev/null; then
    echo "check-shard-coverage.sh: FAIL — completed build job must preserve build.e2e_artifact -> build.liteinst_runtime_release" >&2
    status=1
fi

if ((status == 0)); then
    n=$(printf '%s\n' "$assigned_unique" | grep -c . || true)
    cell_count=$(jq '[.cells[] | select(.lane == "portable")] | length' ci/expected-e2e-plan.json)
    ((cell_count > 0)) || {
        echo "check-shard-coverage.sh: FAIL — constructed portable cell population is empty" >&2
        exit 1
    }
    if [[ -n ${GITHUB_OUTPUT:-} ]]; then
        printf 'constructed_step_count=%s\n' "$n" >>"$GITHUB_OUTPUT"
        printf 'selected_cell_count=%s\n' "$cell_count" >>"$GITHUB_OUTPUT"
    fi
    echo "check-shard-coverage.sh: OK — $n constructed portable-only steps each assigned to exactly one hosted job; $cell_count selected portable cells."
fi
exit "$status"
