#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Snapshot mutable Cargo outputs into one source-bound content-addressed artifact.
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
VERIFY="$ROOT_DIR/ci/verify-hermit-e2e-artifact.sh"
MANIFEST_SCHEMA=1

function fail {
    echo "publish-hermit-e2e-artifact.sh: $*" >&2
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
        fail "Hermit version JSON is invalid or does not report expected source revision $expected_sha"
    reported_sha=$(jq -er -s '.[0].git_sha' "$version_file")
    [[ $reported_sha != unknown && $reported_sha != *-dirty ]] ||
        fail "Hermit binary reports an inadmissible source revision: $reported_sha"
    [[ $reported_sha == "$expected_sha" ]] ||
        fail "Hermit binary/source revision mismatch: binary=$reported_sha source=$expected_sha"
}

function valid_payload_path {
    local path=$1
    [[ $path =~ ^[A-Za-z0-9._+@%:,=/\-]+$ && $path != /* && $path != . && $path != .. && $path != *//* && $path != */../* && $path != ../* && $path != */.. && $path != */./* && $path != ./* ]]
}

function file_record {
    local path=$1 file=$2 mode size hash
    valid_payload_path "$path" || fail "artifact path is not portable and unambiguous: $path"
    [[ -f $file && ! -L $file ]] || fail "artifact payload is not a regular file: $file"
    mode=$(stat -c '%a' -- "$file")
    size=$(stat -c '%s' -- "$file")
    hash=$(sha256sum -- "$file" | cut -d' ' -f1)
    jq -cn \
        --arg path "$path" \
        --arg mode "$mode" \
        --argjson size "$size" \
        --arg sha256 "$hash" \
        '{path: $path, mode: $mode, size: $size, sha256: $sha256}'
}

function tree_records {
    local root=$1 prefix=$2 output=$3 list relative
    list=$(mktemp)
    TREE_RECORD_TEMPS+=("$list")
    (
        cd "$root"
        find -L . -type f -printf '%P\0' | LC_ALL=C sort -z
    ) >"$list" || fail "cannot enumerate resource bundle: $root"
    while IFS= read -r -d '' relative; do
        file_record "$prefix/$relative" "$root/$relative"
    done <"$list" >"$output"
}

function require_complete_resources {
    local install=$1 path
    [[ -d $install/rsrcs ]] || fail "resource bundle has no rsrcs directory: $install"
    for path in libdetcore_dbt.so libdetcore_sabre.so libreverie_dbt_client.so libreverie_liteinst.so; do
        [[ -f $install/rsrcs/$path && -s $install/rsrcs/$path ]] ||
            fail "resource bundle is missing or empty: $install/rsrcs/$path"
    done
    for path in dynamorio/bin64/drrun sabre e9patch e9tool; do
        [[ -f $install/rsrcs/$path && -s $install/rsrcs/$path && -x $install/rsrcs/$path ]] ||
            fail "resource bundle executable is missing, empty, or non-executable: $install/rsrcs/$path"
    done
}

[[ $# == 3 || $# == 4 ]] ||
    fail "usage: $0 SOURCE-BINARY BUNDLE-ROOT POINTER [SOURCE-INSTALL-DIR]"
source_binary=$1
bundle_root=$2
pointer=$3
source_install=${4:-}
kind=binary-only
[[ -z $source_install ]] || kind=complete

command -v git >/dev/null || fail "git is required"
command -v jq >/dev/null || fail "jq is required"
command -v sha256sum >/dev/null || fail "sha256sum is required"
[[ -f $source_binary && ! -L $source_binary && -s $source_binary && -x $source_binary ]] ||
    fail "source Hermit is missing, empty, or non-executable: $source_binary"
if [[ $kind == complete ]]; then
    [[ -d $source_install && ! -L $source_install ]] ||
        fail "source install bundle is missing or is a symlink: $source_install"
fi

require_clean_source_state
initial_source_state=$(source_state)

mkdir -p "$bundle_root" "$(dirname "$pointer")"
bundle_root=$(cd "$bundle_root" && pwd -P)
pointer_dir=$(cd "$(dirname "$pointer")" && pwd -P)
pointer="$pointer_dir/$(basename "$pointer")"
stage="$bundle_root/.tmp-$$"
pointer_tmp="$pointer.tmp-$$"
before_records=$(mktemp)
after_records=$(mktemp)
file_records=$(mktemp)
version_before=$(mktemp)
version_after=$(mktemp)
TREE_RECORD_TEMPS=()
function cleanup {
    rm -rf "$stage"
    rm -f "$pointer_tmp" "$before_records" "$after_records" "$file_records" \
        "$version_before" "$version_after" "${TREE_RECORD_TEMPS[@]}"
}
trap cleanup EXIT
[[ ! -e $stage ]] || fail "staging path already exists: $stage"
mkdir -p "$stage"

"$source_binary" version --json >"$version_before" ||
    fail "source Hermit did not emit version JSON: $source_binary"
validate_version_json "$version_before" "$SOURCE_EXPECTED_REPORTED_SHA"
binary_hash_before=$(sha256sum -- "$source_binary" | cut -d' ' -f1)
install -m 755 "$source_binary" "$stage/hermit"
binary_hash_after=$(sha256sum -- "$source_binary" | cut -d' ' -f1)
published_binary_hash=$(sha256sum -- "$stage/hermit" | cut -d' ' -f1)
[[ -f $source_binary && ! -L $source_binary && -s $source_binary && -x $source_binary ]] ||
    fail "source Hermit changed type, size, or mode during publication: $source_binary"
[[ $binary_hash_before == "$binary_hash_after" && $binary_hash_before == "$published_binary_hash" ]] ||
    fail "source Hermit changed bytes during publication: before=$binary_hash_before after=$binary_hash_after copy=$published_binary_hash"
"$stage/hermit" version --json >"$version_after" ||
    fail "published Hermit did not emit version JSON: $stage/hermit"
cmp -s "$version_before" "$version_after" ||
    fail "source and published Hermit version JSON differ"
validate_version_json "$version_after" "$SOURCE_EXPECTED_REPORTED_SHA"
source_binary_hash_final=$(sha256sum -- "$source_binary" | cut -d' ' -f1)
published_binary_hash_final=$(sha256sum -- "$stage/hermit" | cut -d' ' -f1)
[[ $source_binary_hash_final == "$binary_hash_before" && $published_binary_hash_final == "$binary_hash_before" ]] ||
    fail "Hermit changed bytes while reporting its version: source=$source_binary_hash_final published=$published_binary_hash_final expected=$binary_hash_before"
file_record hermit "$stage/hermit" >"$file_records"

if [[ $kind == complete ]]; then
    require_complete_resources "$source_install"
    tree_records "$source_install" install "$before_records"
    [[ -s $before_records ]] || fail "source install bundle contains no regular files: $source_install"
    mkdir -p "$stage/install"
    cp -aL "$source_install/." "$stage/install/"
    tree_records "$source_install" install "$after_records"
    cmp -s "$before_records" "$after_records" ||
        fail "source install bundle changed during publication: $source_install"
    tree_records "$stage/install" install "$stage/resource-records.jsonl"
    cmp -s "$before_records" "$stage/resource-records.jsonl" ||
        fail "published resource metadata or bytes do not match source bundle: $source_install"
    require_complete_resources "$stage/install"
    [[ -z $(find "$stage/install" -type l -print -quit) ]] ||
        fail "published resource bundle retained a symlink instead of an immutable copy: $stage/install"
    [[ -z $(find "$stage/install" ! -type d ! -type f -print -quit) ]] ||
        fail "published resource bundle contains an unsupported filesystem object: $stage/install"
    cat "$stage/resource-records.jsonl" >>"$file_records"
    rm -f "$stage/resource-records.jsonl"
fi

require_clean_source_state
final_source_state=$(source_state)
[[ $final_source_state == "$initial_source_state" ]] ||
    fail "source revision changed during publication"
cmp -s "$version_before" "$version_after" || fail "Hermit version JSON changed during publication"

jq -S -n \
    --argjson schema "$MANIFEST_SCHEMA" \
    --arg kind "$kind" \
    --arg hermit_head "$SOURCE_HERMIT_HEAD" \
    --arg hermit_tree "$SOURCE_HERMIT_TREE" \
    --arg agent_utils_gitlink "$SOURCE_AGENT_UTILS_GITLINK" \
    --arg agent_utils_head "$SOURCE_AGENT_UTILS_HEAD" \
    --rawfile hermit_version_json "$version_before" \
    --slurpfile files "$file_records" \
    '{
        schema: $schema,
        kind: $kind,
        source: {
            hermit: {head: $hermit_head, tree: $hermit_tree},
            agent_utils: {gitlink: $agent_utils_gitlink, head: $agent_utils_head}
        },
        hermit_version_json: $hermit_version_json,
        files: $files
    }' >"$stage/manifest.json"
chmod 644 "$stage/manifest.json"

identity=$(sha256sum -- "$stage/manifest.json" | cut -d' ' -f1)
published="$bundle_root/$identity"
if [[ -e $published ]]; then
    "$VERIFY" "$published" >/dev/null
    rm -rf "$stage"
else
    mv "$stage" "$published"
    "$VERIFY" "$published" >/dev/null
fi
printf '%s\n' "$published" >"$pointer_tmp"
mv -f "$pointer_tmp" "$pointer"
resolved=$("$VERIFY" "$pointer")
[[ $resolved == "$published" ]] || fail "published pointer resolved to $resolved, expected $published"
printf 'published Hermit E2E artifact kind=%s identity=%s path=%s\n' "$kind" "$identity" "$published"
