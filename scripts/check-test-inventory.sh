#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Fail unless every file below tests/ has one valid inventory entry. An
# optional repository root lets merge-gate run this trusted script from main
# against a synthesized PR merge tree.
set -euo pipefail

ROOT_DIR=${1:-$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)}
INVENTORY="$ROOT_DIR/tests/e2e/manifests/inventory/test-files.json"

function die {
    echo "check-test-inventory.sh: $*" >&2
    exit 2
}

command -v jq >/dev/null 2>&1 || die "jq is required"
[[ -d $ROOT_DIR/tests ]] || die "missing tests directory below $ROOT_DIR"
[[ -f $INVENTORY ]] || die "missing test inventory: $INVENTORY"

jq -e '
    .schema == 2
    and (.files | type == "array" and length > 0)
    and (.files | all(
        type == "object"
        and ((keys | sort) == ["disposition", "path", "runner", "why"])
        and (.path | type == "string" and startswith("tests/") and (contains("..") | not))
        and (.disposition | type == "string" and length > 0)
        and (.runner | type == "string" and length > 0)
        and (.why | type == "string" and length > 0)
        and (. as $entry | ($entry.why | startswith($entry.path + " is owned by " + $entry.runner + ": ")))))
    and ((.files | map(.path) | unique | length) == (.files | length))
    and ([.files[] | select(.disposition != "manifest-test")
          | . as $entry
          | ($entry.why | ltrimstr($entry.path + " is owned by " + $entry.runner + ": "))]
         | length == (unique | length))
    and all(.files[] | select(.disposition != "manifest-test");
        (. as $entry
         | ($entry.why
            | ltrimstr($entry.path + " is owned by " + $entry.runner + ": ")
            | length >= 120)))
' "$INVENTORY" >/dev/null || die "test inventory schema violation"

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
find "$ROOT_DIR/tests" \( -type f -o -type l \) -printf 'tests/%P\n' |
    LC_ALL=C sort >"$scratch/expected"
jq -r '.files[].path' "$INVENTORY" | LC_ALL=C sort >"$scratch/actual"
if ! diff -u "$scratch/expected" "$scratch/actual"; then
    die "test inventory is stale; every file in tests/ must have an explicit disposition"
fi

count=$(wc -l <"$scratch/actual")
echo "check-test-inventory.sh: OK - $count test files have one valid inventory entry each"
