#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Verify one published manifest-plan document against its exact input set.
#
# The document is a pure function of (a) the producer's own sources and locked
# dependency closure and (b) the manifest TOMLs it reads at runtime. Publishing
# the DOCUMENT rather than the producer BINARY is deliberate: the binary bakes
# CARGO_MANIFEST_DIR at compile time, so a transported binary would read its
# BUILD tree's manifests, not the consumer's. A document plus a per-file hash of
# the input set has no such ambiguity and can be checked by any consumer.
#
# Exit codes are distinct because the caller reacts differently to each:
#   0  verified   -> prints the bundle directory
#   2  invalid    -> malformed/corrupt/tampered; the CALLER MUST FAIL CLOSED
#   3  stale      -> inputs drifted from the published set; caller may rebuild
#   4  absent     -> nothing published; caller may rebuild
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

# The exact input set the published document is bound to. Directories are
# hashed recursively; `target` directories are pruned so a generated tree can
# never make every consumer look stale (which would silently restore the Cargo
# build-lock stall this whole mechanism exists to remove).
#
# ci/matrix-symmetry-baseline.json is here because main() calls
# validate_front_door() on EVERY format including harness-json: a drifted
# baseline makes the producer die, so a document published before that drift is
# not merely old, it is wrong. rust-toolchain.toml pins the compiler that built
# the producer. Cargo.lock covers the dependency closure; the producer has no
# path dependencies, so no other crate's sources can change its behavior.
# audit_manifest_plan_cargo_independence re-derives this list from the
# producer's own declared path constants and fails if one is missing.
readonly -a MANIFEST_PLAN_INPUT_DIRS=(tests/e2e/manifests ci/manifest-plan)
readonly -a MANIFEST_PLAN_INPUT_FILES=(Cargo.lock rust-toolchain.toml ci/matrix-symmetry-baseline.json)

function fail {
    local code=$1
    shift
    echo "verify-manifest-plan.sh: $*" >&2
    exit "$code"
}

# Hash one input without following a repository-controlled symlink outside the
# checkout. The type marker binds regular-file versus symlink identity; for a
# symlink the link text is the input, matching the producer's explicit
# file-or-symlink acceptance check without reading the target.
function emit_input_path {
    local relative=$1 path target resolved hash
    [[ $relative != /* && $relative != *".."* ]] ||
        fail 2 "manifest-plan input must be a repo-relative path without '..': $relative"
    path="$ROOT_DIR/$relative"
    if [[ -L $path ]]; then
        target=$(readlink -- "$path")
        [[ $target != /* ]] || fail 2 "manifest-plan input symlink is absolute: $relative -> $target"
        resolved=$(realpath -m -- "$(dirname -- "$path")/$target")
        [[ $resolved == "$ROOT_DIR"/* ]] ||
            fail 2 "manifest-plan input symlink escapes the checkout: $relative -> $target"
        hash=$(printf 'symlink\0%s' "$target" | sha256sum | cut -d' ' -f1)
    elif [[ -f $path ]]; then
        hash=$({ printf 'regular\0'; cat -- "$path"; } | sha256sum | cut -d' ' -f1)
    else
        fail 2 "manifest-plan input is missing or is not a file/symlink: $relative"
    fi
    printf '%s  %s\n' "$hash" "$relative"
}

# Deterministic `<sha256>  <path-relative-to-ROOT_DIR>` lines over the input set.
function emit_input_manifest {
    local dir file relative symlink
    local -a programs=()
    {
        for dir in "${MANIFEST_PLAN_INPUT_DIRS[@]}"; do
            [[ -d $ROOT_DIR/$dir ]] || fail 2 "manifest-plan input directory is missing: $dir"
            symlink=$(cd "$ROOT_DIR/$dir" && find . \( -type d -name target -prune \) -o -type l -print -quit)
            [[ -z $symlink ]] ||
                fail 2 "manifest-plan input directory contains a symlink: $dir/${symlink#./}"
            while IFS= read -r -d '' relative; do
                emit_input_path "$dir/$relative"
            done < <(cd "$ROOT_DIR/$dir" && find . \( -type d -name target -prune \) -o -type f -printf '%P\0')
        done
        for file in "${MANIFEST_PLAN_INPUT_FILES[@]}"; do
            emit_input_path "$file"
        done

        # The producer reads every manifest-declared program path to enforce
        # that it exists as a file or symlink. Bind those dynamic inputs too;
        # otherwise a published document could verify after a program vanished
        # even though rerunning the producer would refuse the same tree.
        mapfile -t programs < <(
            sed -nE 's/^[[:space:]]*program = "([^"]+)"[[:space:]]*$/\1/p' \
                "$ROOT_DIR"/tests/e2e/manifests/*.toml | LC_ALL=C sort -u
        )
        ((${#programs[@]} > 0)) || fail 2 "manifest-plan manifests declare no program inputs"
        for file in "${programs[@]}"; do
            [[ $file == tests/* ]] || fail 2 "manifest-plan program input is outside tests/: $file"
            emit_input_path "$file"
        done
    } | LC_ALL=C sort
}

# Content address binding the document to the inputs that produced it. Both
# halves are included so neither a swapped document nor a swapped input
# manifest can keep an identity that still names its own directory.
function manifest_plan_identity {
    local inputs_hash=$1 documents_hash=$2
    printf '%s\n%s\n' "$inputs_hash" "$documents_hash" | sha256sum | cut -d' ' -f1
}

if [[ ${1:-} == --emit-input-manifest ]]; then
    emit_input_manifest
    exit 0
fi

[[ $# == 1 ]] || fail 2 "usage: $0 POINTER | --emit-input-manifest"
pointer=$1
[[ -f $pointer && -s $pointer ]] || fail 4 "no manifest-plan document is published: $pointer"
[[ $(wc -l <"$pointer") == 1 ]] || fail 2 "manifest-plan pointer must contain exactly one line: $pointer"
IFS= read -r bundle <"$pointer"
[[ -n $bundle && $bundle == /* ]] || fail 2 "manifest-plan pointer must contain one absolute path: $pointer"
[[ ! -L $bundle ]] || fail 2 "published manifest-plan path is a symlink: $bundle"
# A pointer naming a path that does not exist is "nothing published HERE", not
# corruption: bundles carry absolute paths, so a pointer transported to a runner
# with a different workspace root must fall back to a rebuild rather than fail
# the job closed.
[[ -d $bundle ]] || fail 4 "no manifest-plan document at the published path: $bundle"

for required in harness.json harness.json.sha256 inputs.sha256; do
    [[ -f $bundle/$required && ! -L $bundle/$required && -s $bundle/$required ]] ||
        fail 2 "published manifest-plan bundle is incomplete: $bundle/$required"
done

expected_documents_hash=$(<"$bundle/harness.json.sha256")
actual_documents_hash=$(sha256sum "$bundle/harness.json" | cut -d' ' -f1)
[[ $actual_documents_hash == "$expected_documents_hash" ]] ||
    fail 2 "published manifest-plan document hash mismatch: expected $expected_documents_hash, got $actual_documents_hash"

inputs_hash=$(sha256sum "$bundle/inputs.sha256" | cut -d' ' -f1)
identity=$(manifest_plan_identity "$inputs_hash" "$actual_documents_hash")
[[ ${bundle##*/} == "$identity" ]] ||
    fail 2 "content-addressed manifest-plan identity mismatch: expected directory $identity, got ${bundle##*/}"

# STALE is checked LAST, so a tampered bundle is reported as invalid rather than
# being excused as merely out of date.
current=$(mktemp)
trap 'rm -f "$current"' EXIT
emit_input_manifest >"$current"
cmp -s "$bundle/inputs.sha256" "$current" ||
    fail 3 "manifest-plan inputs drifted from the published set: $bundle"

printf '%s\n' "$bundle"
