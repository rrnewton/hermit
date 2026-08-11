#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

if (( $# != 3 )); then
    echo "Usage: $0 <cargo-profile> <stable-runtime-path> <runtime-target-root>" >&2
    exit 2
fi

root_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
liteinst_profile=$1
liteinst_stable_input=$2
reverie_pin=$(
    "$root_dir/ci/run-reverie-pin-check.sh" --repo "$root_dir" --print-pin
)
liteinst_target_dir=$(realpath -m -- "$3-${reverie_pin:0:8}")
liteinst_stage_dir=$(dirname -- "$liteinst_stable_input")
liteinst_stage_name=$(basename -- "$liteinst_stable_input")

if [[ -z $liteinst_stable_input || $liteinst_stage_name == . || $liteinst_stage_name == / ]]; then
    echo "Stable LiteInst runtime path must name a file: $liteinst_stable_input" >&2
    exit 2
fi

mkdir -p -- "$liteinst_stage_dir"
liteinst_stage_dir=$(realpath -e -- "$liteinst_stage_dir")
liteinst_stable_stage=$liteinst_stage_dir/$liteinst_stage_name
liteinst_temp_dir=$(
    mktemp -d --tmpdir="$liteinst_stage_dir" ".${liteinst_stage_name}.stage.XXXXXX"
)
liteinst_temp_stage=$liteinst_temp_dir/runtime.so
cleanup_liteinst_temp_stage() {
    if [[ -n ${liteinst_temp_stage:-} ]]; then
        rm -f -- "$liteinst_temp_stage"
    fi
    if [[ -n ${liteinst_temp_dir:-} ]]; then
        rmdir -- "$liteinst_temp_dir"
    fi
}
trap cleanup_liteinst_temp_stage EXIT

# OPTIONAL content-keyed artifact cache. OFF unless HERMIT_LITEINST_RUNTIME_CACHE
# names a directory, so the default path below is byte-for-byte what it was.
#
# WHY AN ARTIFACT CACHE AND NOT A SHARED TARGET DIR. `$liteinst_target_dir` above
# is already keyed by the Reverie pin, but it lives INSIDE the checkout, so every
# `validate-fresh-*` worktree misses it and pays the full build (~205s cold vs
# ~2s warm; cold full-profile runs median 470s against 304s warm over 515 runs).
# The tempting fix -- point that target dir at a shared location -- reintroduces
# exactly the hazard ci-hub/validate/start_unit.py:140-162 exists to prevent: a
# tree whose `git status` is clean while carrying gigabytes of ignored build
# output, where a stale gitignored cache has already flipped a `--self-test`
# verdict. A CONTENT key cannot do that: change any hashed input and the key
# changes, so a stale artifact is unreachable rather than merely unlikely.
#
# The key covers every input that can change the produced object: the Reverie
# revision, the Cargo profile, the exact toolchain identity, and the content of
# every tracked file under liteinst-runtime-build/. Anything not hashed here is
# a correctness bug, so the set is deliberately wide and the miss path is always
# safe. This mirrors reverie-dbt's `source_recipe_key()`, which hashes
# (vendor/dynamorio, build.rs, $CMAKE, $CMAKE_GENERATOR) for the same reason.
liteinst_cache_root=${HERMIT_LITEINST_RUNTIME_CACHE:-}
liteinst_cache_key=
liteinst_cache_file=
if [[ -n $liteinst_cache_root ]]; then
    liteinst_cache_key=$(
        {
            printf 'reverie-pin\0%s\0' "$reverie_pin"
            printf 'profile\0%s\0' "$liteinst_profile"
            printf 'toolchain\0%s\0' "$("${CARGO:-cargo}" --version 2>/dev/null; rustc -vV 2>/dev/null)"
            printf 'sources\0'
            # Hash CONTENT, not mtimes, and do it over the tracked set so an
            # untracked scratch file cannot silently change the key either way.
            git -C "$root_dir" ls-files -z -- liteinst-runtime-build \
                | sort -z \
                | while IFS= read -r -d '' rel; do
                    printf '%s\0' "$rel"
                    git -C "$root_dir" hash-object -- "$root_dir/$rel"
                done
        } | sha256sum | cut -d' ' -f1
    )
    if [[ ! $liteinst_cache_key =~ ^[0-9a-f]{64}$ ]]; then
        echo "LiteInst runtime cache: refusing a malformed content key; building instead" >&2
        liteinst_cache_key=
    else
        liteinst_cache_file=$liteinst_cache_root/liteinst-runtime-$liteinst_cache_key.so
    fi
fi

if [[ -n $liteinst_cache_file && -s $liteinst_cache_file && -f $liteinst_cache_file \
      && ! -L $liteinst_cache_file ]]; then
    echo "LiteInst runtime cache HIT key=sha256:$liteinst_cache_key"
    cp --reflink=auto -- "$liteinst_cache_file" "$liteinst_temp_stage"
else
    if [[ -n $liteinst_cache_key ]]; then
        echo "LiteInst runtime cache MISS key=sha256:$liteinst_cache_key"
    fi
    HERMIT_LITEINST_STAGE=$liteinst_temp_stage "${CARGO:-cargo}" build \
        --locked \
        --manifest-path liteinst-runtime-build/Cargo.toml \
        --profile "$liteinst_profile" \
        --target-dir "$liteinst_target_dir"
fi

if [[ ! -s $liteinst_temp_stage || ! -f $liteinst_temp_stage || -L $liteinst_temp_stage ]]; then
    echo "LiteInst runtime build did not stage a non-empty regular file: $liteinst_temp_stage" >&2
    exit 1
fi

# Publish AFTER the non-empty regular-file check above, so a failed build can
# never populate the cache. Write through a temp name in the same directory and
# rename, so a concurrent reader sees either no entry or a complete one.
if [[ -n $liteinst_cache_file && ! -s $liteinst_cache_file ]]; then
    mkdir -p -- "$liteinst_cache_root"
    liteinst_cache_temp=$(mktemp -- "$liteinst_cache_file.XXXXXX")
    cp --reflink=auto -- "$liteinst_temp_stage" "$liteinst_cache_temp"
    mv -fT -- "$liteinst_cache_temp" "$liteinst_cache_file"
fi

# The unique destination above forces Cargo to rerun the staging build script.
# It is adjacent to the stable path, so this rename is an atomic replacement.
mv -fT -- "$liteinst_temp_stage" "$liteinst_stable_stage"
liteinst_temp_stage=
rmdir -- "$liteinst_temp_dir"
liteinst_temp_dir=
