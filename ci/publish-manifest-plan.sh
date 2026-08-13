#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Publish the manifest-plan document as one content-addressed, input-bound bundle.
#
# Consumers must not shell out to `cargo run` for metadata: `cargo run` takes
# Cargo's exclusive build-directory lock, and the e2e manifest consumers run in
# the same wave as the DAG's Cargo fan-out, so that lock silently serialized a
# ~6s node behind whatever compile owned it. This node produces the document
# once, beside the build that produced the binary, so every later consumer can
# read it without Cargo.
#
# The input manifest is emitted by ci/verify-manifest-plan.sh so the publisher
# and the verifier cannot disagree about what "the input set" is.
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
VERIFY="$ROOT_DIR/ci/verify-manifest-plan.sh"

function fail {
    echo "publish-manifest-plan.sh: $*" >&2
    exit 2
}

[[ $# == 3 ]] || fail "usage: $0 PRODUCER-BINARY BUNDLE-ROOT POINTER"
producer=$1
bundle_root=$2
pointer=$3

[[ -f $producer && -s $producer && -x $producer ]] ||
    fail "manifest-plan producer is missing, empty, or non-executable: $producer"
mkdir -p "$bundle_root" "$(dirname "$pointer")"
bundle_root=$(cd "$bundle_root" && pwd -P)
pointer="$(cd "$(dirname "$pointer")" && pwd -P)/$(basename "$pointer")"

stage="$bundle_root/.tmp-$$"
pointer_tmp="$pointer.tmp-$$"
before_inputs=$(mktemp)
after_inputs=$(mktemp)
function cleanup {
    rm -rf "$stage"
    rm -f "$pointer_tmp" "$before_inputs" "$after_inputs"
}
trap cleanup EXIT
[[ ! -e $stage ]] || fail "staging path already exists: $stage"
mkdir -p "$stage"

# Bracket the production with the input hashes so a concurrent edit cannot land
# a document that claims an input set it was not built from.
"$VERIFY" --emit-input-manifest >"$before_inputs"
[[ -s $before_inputs ]] || fail "manifest-plan input set resolved to no files"
"$producer" --format harness-json >"$stage/harness.json"
"$VERIFY" --emit-input-manifest >"$after_inputs"
cmp -s "$before_inputs" "$after_inputs" ||
    fail "manifest-plan inputs changed during publication; re-run this node"
[[ -s $stage/harness.json ]] || fail "manifest-plan producer emitted an empty document: $producer"
jq -e 'type == "array" and length > 0' "$stage/harness.json" >/dev/null ||
    fail "manifest-plan producer did not emit a non-empty document array: $producer"

cp -- "$before_inputs" "$stage/inputs.sha256"
documents_hash=$(sha256sum "$stage/harness.json" | cut -d' ' -f1)
printf '%s\n' "$documents_hash" >"$stage/harness.json.sha256"
inputs_hash=$(sha256sum "$stage/inputs.sha256" | cut -d' ' -f1)
identity=$(printf '%s\n%s\n' "$inputs_hash" "$documents_hash" | sha256sum | cut -d' ' -f1)

published="$bundle_root/$identity"
if [[ -e $published ]]; then
    rm -rf "$stage"
else
    mv "$stage" "$published"
fi
printf '%s\n' "$published" >"$pointer_tmp"
mv -f "$pointer_tmp" "$pointer"

resolved=$("$VERIFY" "$pointer")
[[ $resolved == "$published" ]] || fail "published pointer resolved to $resolved, expected $published"
printf 'published manifest-plan document identity=%s inputs=%s files=%s path=%s\n' \
    "$identity" "$inputs_hash" "$(wc -l <"$published/inputs.sha256")" "$published"
