#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
HARNESS="$ROOT_DIR/tests/e2e/manifests/manifest-harness.rs"
VALIDATE="$ROOT_DIR/validate.sh"

for command in jq diff sort grep; do
    command -v "$command" >/dev/null || {
        echo "manifest-ci-alignment: missing required command: $command" >&2
        exit 2
    }
done

tmpdir=$(mktemp -d)
trap 'rm -rf -- "$tmpdir"' EXIT

plan="$tmpdir/plan.json"
"$HARNESS" plan --format json >"$plan"
jq -e 'type == "array" and length > 0' "$plan" >/dev/null

cells=$(jq 'length' "$plan")
unique_cells=$(jq '[.[] | [.test, .mode, (.backend // "")]] | unique | length' "$plan")
if [[ $cells != "$unique_cells" ]]; then
    echo "manifest-ci-alignment: duplicate (test, mode, backend) plan cells: $cells total, $unique_cells unique" >&2
    exit 1
fi

for expected in \
    'manifest-harness.rs validate' \
    'manifest-harness.rs build --all --lane portable' \
    'manifest-harness.rs build --all --lane privileged' \
    'manifest-harness.rs run --all --lane portable' \
    'manifest-harness.rs run --all --lane privileged'; do
    grep -Fq "$expected" "$VALIDATE" || {
        echo "manifest-ci-alignment: validate.sh is missing manifest discovery command: $expected" >&2
        exit 1
    }
done

for workflow in \
    ci-dag.yml \
    ci-portable.yml \
    ci-privileged.yml \
    validation-levels.yml; do
    path="$ROOT_DIR/.github/workflows/$workflow"
    consumers=$(grep -Ec '^[[:space:]]+run: .*(ci/run-dag\.sh|\./validate\.sh)' "$path" || true)
    installers=$(grep -Fc 'run: ci/install-rust-script.sh' "$path" || true)
    audits=$(grep -Fc 'run: ci/check-manifest-ci-alignment.sh' "$path" || true)
    if ((consumers == 0 || consumers != installers || consumers != audits)); then
        echo "manifest-ci-alignment: $workflow consumers/installers/audits = $consumers/$installers/$audits" >&2
        exit 1
    fi
done

for lane in portable privileged; do
    dag="$tmpdir/$lane.dag.json"
    manifest_buckets="$tmpdir/$lane.manifest-buckets"
    dag_buckets="$tmpdir/$lane.dag-buckets"
    dag_buckets_raw="$tmpdir/$lane.dag-buckets-raw"

    jq -r --arg lane "$lane" \
        '[.[] | select(.lane == $lane) | .bucket] | unique[]' \
        "$plan" | sort >"$manifest_buckets"

    "$ROOT_DIR/ci/render-manifest-dag.sh" "$lane" "$dag"

    ids="$tmpdir/$lane.ids"
    jq -r '.steps[] | .group + "." + .job' "$dag" >"$ids"
    if [[ $(wc -l <"$ids") != $(sort -u "$ids" | wc -l) ]]; then
        echo "manifest-ci-alignment: duplicate $lane DAG node ids" >&2
        exit 1
    fi
    while IFS=$'\t' read -r node dependency; do
        grep -Fxq "$dependency" "$ids" || {
            echo "manifest-ci-alignment: $lane DAG node $node has missing dependency $dependency" >&2
            exit 1
        }
    done < <(jq -r '.steps[] | (.group + "." + .job) as $node | (.deps // [])[] | [$node, .] | @tsv' "$dag")

    jq -e --arg lane "$lane" '
        if $lane == "portable" then .resource_caps.hermit_guest == 5
        else .resource_caps.kvm == 1 end
    ' "$dag" >/dev/null || {
        echo "manifest-ci-alignment: $lane DAG resource cap is inconsistent" >&2
        exit 1
    }

    jq -r --arg lane "$lane" '
        .steps[]
        | select(.group == "e2e")
        | (.cmd // "")
        | try capture("--bucket (?<bucket>[^ ]+) --lane (?<lane>[^ ]+)") catch empty
        | select(.lane == $lane)
        | .bucket
    ' "$dag" >"$dag_buckets_raw"
    sort -u "$dag_buckets_raw" >"$dag_buckets"

    if [[ $(wc -l <"$dag_buckets_raw") != $(wc -l <"$dag_buckets") ]]; then
        echo "manifest-ci-alignment: duplicate $lane DAG bucket nodes" >&2
        exit 1
    fi
    if ! diff -u "$manifest_buckets" "$dag_buckets"; then
        echo "manifest-ci-alignment: $lane DAG buckets do not match manifest buckets" >&2
        exit 1
    fi

    jq -e --arg lane "$lane" '
        [.steps[]
         | select(.group == "build" and .job == "manifest_guests")
         | select(.cmd | contains("build --all --lane " + $lane))]
        | length == 1
    ' "$dag" >/dev/null || {
        echo "manifest-ci-alignment: $lane DAG needs exactly one manifest guest build node" >&2
        exit 1
    }

    while IFS= read -r bucket; do
        jq -e --arg lane "$lane" --arg bucket "$bucket" '
            [.steps[]
             | select(.group == "e2e")
             | select((.cmd // "") | contains("--bucket " + $bucket + " --lane " + $lane))
             | select((.cmd | contains("--prebuilt")) and
                      (.cmd | contains("--results")) and
                      (.cmd | contains("--junit")) and
                      ((.deps // []) | index("build.manifest_guests") != null) and
                      (if $lane == "portable" then
                           .hint.resources.hermit_guest == 1
                       else .hint.resources.kvm == 1 end))]
            | length == 1
        ' "$dag" >/dev/null || {
            echo "manifest-ci-alignment: incomplete $lane DAG node for bucket $bucket" >&2
            exit 1
        }
    done <"$manifest_buckets"
done

tests=$(jq '[.[].test] | unique | length' "$plan")
portable_tests=$(jq '[.[] | select(.lane == "portable") | .test] | unique | length' "$plan")
portable_cells=$(jq '[.[] | select(.lane == "portable")] | length' "$plan")
privileged_tests=$(jq '[.[] | select(.lane == "privileged") | .test] | unique | length' "$plan")
privileged_cells=$(jq '[.[] | select(.lane == "privileged")] | length' "$plan")

printf 'PASS manifest/CI alignment: tests=%s cells=%s portable=%s/%s privileged=%s/%s (tests/cells)\n' \
    "$tests" "$cells" "$portable_tests" "$portable_cells" \
    "$privileged_tests" "$privileged_cells"

if [[ -n ${GITHUB_STEP_SUMMARY:-} ]]; then
    {
        echo '### Manifest/CI alignment'
        echo
        echo '| Lane | Tests | Plan cells | DAG buckets |'
        echo '| --- | ---: | ---: | ---: |'
        printf '| portable | %s | %s | %s |\n' \
            "$portable_tests" "$portable_cells" "$(wc -l <"$tmpdir/portable.manifest-buckets")"
        printf '| privileged | %s | %s | %s |\n' \
            "$privileged_tests" "$privileged_cells" "$(wc -l <"$tmpdir/privileged.manifest-buckets")"
        printf '| **total** | **%s** | **%s** | **%s** |\n' \
            "$tests" "$cells" "$(( $(wc -l <"$tmpdir/portable.manifest-buckets") + $(wc -l <"$tmpdir/privileged.manifest-buckets") ))"
    } >>"$GITHUB_STEP_SUMMARY"
fi
