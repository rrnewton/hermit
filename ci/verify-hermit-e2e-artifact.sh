#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Verify and resolve one source-bound content-addressed Hermit E2E artifact.
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
MANIFEST_SCHEMA=1

function fail {
    echo "verify-hermit-e2e-artifact.sh: $*" >&2
    exit 2
}

function require_clean_source_state {
    local status gitlink_line

    [[ $(git -C "$ROOT_DIR" rev-parse --is-inside-work-tree 2>/dev/null) == true ]] ||
        fail "Hermit source is not a Git worktree: $ROOT_DIR"
    [[ $(git -C "$ROOT_DIR/agent-utils" rev-parse --is-inside-work-tree 2>/dev/null) == true ]] ||
        fail "agent-utils source is not an initialized Git worktree: $ROOT_DIR/agent-utils"

    SOURCE_HERMIT_HEAD=$(git -C "$ROOT_DIR" rev-parse --verify HEAD)
    SOURCE_HERMIT_TREE=$(git -C "$ROOT_DIR" show -s --format=%T HEAD)
    SOURCE_AGENT_UTILS_HEAD=$(git -C "$ROOT_DIR/agent-utils" rev-parse --verify HEAD)
    gitlink_line=$(git -C "$ROOT_DIR" ls-tree HEAD -- agent-utils)
    [[ $gitlink_line =~ ^160000[[:space:]]commit[[:space:]]([0-9a-f]+)[[:space:]]agent-utils$ ]] ||
        fail "Hermit HEAD has no exact agent-utils gitlink"
    SOURCE_AGENT_UTILS_GITLINK=${BASH_REMATCH[1]}
    [[ $SOURCE_AGENT_UTILS_GITLINK == "$SOURCE_AGENT_UTILS_HEAD" ]] ||
        fail "agent-utils gitlink/HEAD mismatch: gitlink=$SOURCE_AGENT_UTILS_GITLINK head=$SOURCE_AGENT_UTILS_HEAD"
    SOURCE_EXPECTED_REPORTED_SHA=$(git -C "$ROOT_DIR" rev-parse --short=12 HEAD)

    status=$(git -C "$ROOT_DIR" status --porcelain=v1 --untracked-files=all --ignore-submodules=none)
    [[ -z $status ]] || fail "Hermit source worktree is not clean: $ROOT_DIR"
    status=$(git -C "$ROOT_DIR/agent-utils" status --porcelain=v1 --untracked-files=all)
    [[ -z $status ]] || fail "agent-utils source worktree is not clean: $ROOT_DIR/agent-utils"
}

function source_state {
    printf '%s\n%s\n%s\n%s\n%s\n' \
        "$SOURCE_HERMIT_HEAD" \
        "$SOURCE_HERMIT_TREE" \
        "$SOURCE_AGENT_UTILS_GITLINK" \
        "$SOURCE_AGENT_UTILS_HEAD" \
        "$SOURCE_EXPECTED_REPORTED_SHA"
}

function validate_version_json {
    local version_file=$1 expected_sha=$2 reported_sha

    jq -e -s '
        length == 1
        and (.[0] |
            type == "object"
            and (keys | sort) == ["build_date", "features", "git_sha", "schema", "version"]
            and .schema == 1
            and (.version | type == "string" and length > 0)
            and (.build_date == null or (.build_date | type == "string" and length > 0))
            and (.git_sha | type == "string" and length > 0)
            and (.features |
                type == "object"
                and (keys | sort) == ["dbt", "e9patch", "sabre"]
                and (.dbt | type == "boolean")
                and (.e9patch | type == "boolean")
                and (.sabre | type == "boolean")))
    ' "$version_file" >/dev/null ||
        fail "manifest Hermit version JSON is invalid or does not report expected source revision $expected_sha"
    reported_sha=$(jq -er -s '.[0].git_sha' "$version_file")
    [[ $reported_sha != unknown && $reported_sha != *-dirty ]] ||
        fail "manifest Hermit version JSON reports an inadmissible source revision: $reported_sha"
    [[ $reported_sha == "$expected_sha" ]] ||
        fail "manifest Hermit version/source revision mismatch: binary=$reported_sha source=$expected_sha"
}

function valid_payload_path {
    local path=$1
    [[ $path =~ ^[A-Za-z0-9._+@%:,=/\-]+$ && $path != /* && $path != . && $path != .. && $path != *//* && $path != */../* && $path != ../* && $path != */.. && $path != */./* && $path != ./* ]]
}

function require_complete_resources {
    local install=$1 path
    [[ -d $install/rsrcs ]] || fail "resource bundle has no rsrcs directory: $install"
    for path in libdetcore_dbt.so libdetcore_sabre.so libreverie_dbt_client.so libreverie_liteinst.so; do
        [[ -f $install/rsrcs/$path && ! -L $install/rsrcs/$path && -s $install/rsrcs/$path ]] ||
            fail "resource bundle is missing or empty: $install/rsrcs/$path"
    done
    for path in dynamorio/bin64/drrun sabre e9patch e9tool; do
        [[ -f $install/rsrcs/$path && ! -L $install/rsrcs/$path && -s $install/rsrcs/$path && -x $install/rsrcs/$path ]] ||
            fail "resource bundle executable is missing, empty, or non-executable: $install/rsrcs/$path"
    done
}

[[ $# == 1 ]] || fail "usage: $0 BUNDLE-OR-POINTER"
command -v git >/dev/null || fail "git is required"
command -v jq >/dev/null || fail "jq is required"
command -v sha256sum >/dev/null || fail "sha256sum is required"

input=$1
if [[ -d $input && ! -L $input ]]; then
    bundle=$(cd "$input" && pwd -P)
else
    [[ -f $input && ! -L $input && -s $input ]] ||
        fail "artifact pointer is missing, empty, or not a regular file: $input"
    mapfile -t pointer_lines <"$input"
    [[ ${#pointer_lines[@]} == 1 && -n ${pointer_lines[0]} && ${pointer_lines[0]} == /* ]] ||
        fail "artifact pointer must contain exactly one absolute path: $input"
    bundle=${pointer_lines[0]}
fi

[[ -d $bundle && ! -L $bundle ]] ||
    fail "published artifact directory is missing or not a regular directory: $bundle"
bundle=$(cd "$bundle" && pwd -P)
manifest="$bundle/manifest.json"
[[ -f $manifest && ! -L $manifest && -s $manifest ]] ||
    fail "published artifact has no regular manifest: $bundle"
[[ $(stat -c '%a' -- "$manifest") == 644 ]] ||
    fail "published artifact manifest mode mismatch: $manifest"
manifest_hash=$(sha256sum -- "$manifest" | cut -d' ' -f1)
[[ ${bundle##*/} == "$manifest_hash" ]] ||
    fail "content-addressed artifact manifest mismatch: expected directory $manifest_hash, got ${bundle##*/}"

jq -e --argjson schema "$MANIFEST_SCHEMA" '
    type == "object"
    and (keys | sort) == ["files", "hermit_version_json", "kind", "schema", "source"]
    and .schema == $schema
    and (.kind == "complete" or .kind == "binary-only")
    and (.hermit_version_json | type == "string" and length > 0 and endswith("\n"))
    and (.source |
        type == "object"
        and (keys | sort) == ["agent_utils", "hermit"]
        and (.hermit |
            type == "object"
            and (keys | sort) == ["head", "tree"]
            and (.head | type == "string" and test("^[0-9a-f]{40,64}$"))
            and (.tree | type == "string" and test("^[0-9a-f]{40,64}$")))
        and (.agent_utils |
            type == "object"
            and (keys | sort) == ["gitlink", "head"]
            and (.gitlink | type == "string" and test("^[0-9a-f]{40,64}$"))
            and (.head | type == "string" and test("^[0-9a-f]{40,64}$"))))
    and (.files |
        type == "array"
        and length > 0
        and all(.[];
            type == "object"
            and (keys | sort) == ["mode", "path", "sha256", "size"]
            and (.path | type == "string" and length > 0)
            and (.mode | type == "string" and test("^[0-7]{3,4}$"))
            and (.size | type == "number" and isfinite and floor == . and . >= 0)
            and (.sha256 | type == "string" and test("^[0-9a-f]{64}$")))
        and . == (sort_by(.path))
        and ([.[].path] | unique | length) == length)
    and ([.files[] | select(.path == "hermit")] | length) == 1
    and (.files[] | select(.path == "hermit") | .mode == "755" and .size > 0)
    and (if .kind == "binary-only"
         then (.files | length == 1)
         else (.files | length > 1 and all(.[]; .path == "hermit" or (.path | startswith("install/"))))
         end)
' "$manifest" >/dev/null || fail "published artifact manifest does not match schema $MANIFEST_SCHEMA: $manifest"

kind=$(jq -er '.kind' "$manifest")
require_clean_source_state
initial_source_state=$(source_state)
[[ $(jq -er '.source.hermit.head' "$manifest") == "$SOURCE_HERMIT_HEAD" ]] ||
    fail "artifact Hermit HEAD does not match live source"
[[ $(jq -er '.source.hermit.tree' "$manifest") == "$SOURCE_HERMIT_TREE" ]] ||
    fail "artifact Hermit tree does not match live source"
[[ $(jq -er '.source.agent_utils.gitlink' "$manifest") == "$SOURCE_AGENT_UTILS_GITLINK" ]] ||
    fail "artifact agent-utils gitlink does not match live source"
[[ $(jq -er '.source.agent_utils.head' "$manifest") == "$SOURCE_AGENT_UTILS_HEAD" ]] ||
    fail "artifact agent-utils HEAD does not match live source"

expected_paths=$(mktemp)
actual_paths=$(mktemp)
actual_paths_nul=$(mktemp)
manifest_version=$(mktemp)
live_version=$(mktemp)
function cleanup {
    rm -f "$expected_paths" "$actual_paths" "$actual_paths_nul" "$manifest_version" "$live_version"
}
trap cleanup EXIT

function verify_payload_tree {
    local path file expected_mode expected_size expected_hash actual_mode actual_size actual_hash

    [[ -z $(find "$bundle" -type l -print -quit) ]] ||
        fail "published artifact contains a symlink: $bundle"
    [[ -z $(find "$bundle" ! -type d ! -type f -print -quit) ]] ||
        fail "published artifact contains an unsupported filesystem object: $bundle"
    (
        cd "$bundle"
        find . -type f -printf '%P\0' | LC_ALL=C sort -z
    ) >"$actual_paths_nul" || fail "cannot enumerate published artifact: $bundle"
    while IFS= read -r -d '' path; do
        valid_payload_path "$path" || fail "published artifact contains an invalid path: $path"
        printf '%s\n' "$path"
    done <"$actual_paths_nul" >"$actual_paths"
    {
        printf '%s\n' manifest.json
        jq -r '.files[].path' "$manifest"
    } | LC_ALL=C sort >"$expected_paths"
    cmp -s "$expected_paths" "$actual_paths" ||
        fail "published artifact file set does not match its manifest: $bundle"

    while IFS=$'\t' read -r path expected_mode expected_size expected_hash; do
        valid_payload_path "$path" || fail "manifest contains an invalid payload path: $path"
        file="$bundle/$path"
        [[ -f $file && ! -L $file ]] || fail "manifest payload is missing or not a regular file: $file"
        actual_mode=$(stat -c '%a' -- "$file")
        actual_size=$(stat -c '%s' -- "$file")
        actual_hash=$(sha256sum -- "$file" | cut -d' ' -f1)
        [[ $actual_mode == "$expected_mode" ]] ||
            fail "published artifact mode mismatch for $path: expected $expected_mode, got $actual_mode"
        [[ $actual_size == "$expected_size" ]] ||
            fail "published artifact size mismatch for $path: expected $expected_size, got $actual_size"
        [[ $actual_hash == "$expected_hash" ]] ||
            fail "published artifact SHA-256 mismatch for $path: expected $expected_hash, got $actual_hash"
    done < <(jq -r '.files[] | [.path, .mode, (.size | tostring), .sha256] | @tsv' "$manifest")
}

verify_payload_tree

[[ -f $bundle/hermit && ! -L $bundle/hermit && -s $bundle/hermit && -x $bundle/hermit ]] ||
    fail "published Hermit is missing, empty, or non-executable: $bundle/hermit"
if [[ $kind == complete ]]; then
    require_complete_resources "$bundle/install"
else
    [[ ! -e $bundle/install ]] ||
        fail "binary-only artifact unexpectedly contains an install resource bundle: $bundle"
fi

jq -j '.hermit_version_json' "$manifest" >"$manifest_version"
validate_version_json "$manifest_version" "$SOURCE_EXPECTED_REPORTED_SHA"
"$bundle/hermit" version --json >"$live_version" ||
    fail "published Hermit did not emit live version JSON: $bundle/hermit"
validate_version_json "$live_version" "$SOURCE_EXPECTED_REPORTED_SHA"
cmp -s "$manifest_version" "$live_version" ||
    fail "published Hermit live version JSON does not match its manifest"

# The executable is untrusted until the live probe completes. Recheck the whole
# tree so a self-modifying or side-effecting probe cannot leave accepted bytes.
verify_payload_tree
require_clean_source_state
final_source_state=$(source_state)
[[ $final_source_state == "$initial_source_state" ]] ||
    fail "live source revision changed during artifact verification"
printf '%s\n' "$bundle"
