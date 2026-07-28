#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)

if (($# != 2)); then
    echo "usage: ci/render-manifest-dag.sh <portable|privileged> <output.json>" >&2
    exit 2
fi

lane=$1
output=$2
case "$lane" in
    portable|privileged) ;;
    *)
        echo "render-manifest-dag: unknown lane: $lane" >&2
        exit 2 ;;
esac

base="$ROOT_DIR/ci/dag/$lane.json"
harness="$ROOT_DIR/tests/e2e/manifests/manifest-harness.rs"
tmpdir=$(mktemp -d)
trap 'rm -rf -- "$tmpdir"' EXIT

"$harness" dag --lane "$lane" --format json >"$tmpdir/manifest.json"
mkdir -p "$(dirname -- "$output")"

jq --arg lane "$lane" --slurpfile manifest "$tmpdir/manifest.json" '
    def is_manifest_step:
        (.group == "e2e" and (.job == "metadata" or .job == "manifest_validate")) or
        (.group == "build" and .job == "manifest_guests") or
        ((.cmd // "") | contains("tests/e2e/manifests/manifest-harness.rs run --bucket"));

    [$manifest[0].steps[]
     | select((.group == "build" and .job == "workspace") | not)
     | if $lane == "privileged" then
           .deps = ((.deps // [])
                    | map(if . == "build.workspace" then
                              "build.privileged_tests"
                          else . end))
       else . end] as $manifest_steps
    | .steps = ([.steps[] | select(is_manifest_step | not)] + $manifest_steps)
' "$base" >"$tmpdir/rendered.json"

mv "$tmpdir/rendered.json" "$output"
