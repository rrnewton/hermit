#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

# The artifact producer selects its own target paths and, for private builds,
# its verified compiler and linker inputs. Do not let ambient compiler or
# dynamic-loader controls replace those inputs in this process or descendants.
while IFS= read -r variable; do
    case $variable in
        LD_*|DYLD_*|CARGO_BUILD_*|CARGO_PROFILE_*|CARGO_TARGET_*|\
        CC_*|CXX_*|CFLAGS_*|CXXFLAGS_*|AR_*|RANLIB_*|PKG_CONFIG_*|\
        *_CC|*_CXX|*_CFLAGS|*_CXXFLAGS|*_AR|*_RANLIB|AR|CC|CFLAGS|CXX|CXXFLAGS|\
        COMPILER_PATH|CPATH|C_INCLUDE_PATH|CPLUS_INCLUDE_PATH|GCC_EXEC_PREFIX|\
        LIBRARY_PATH|RANLIB|RUSTC|RUSTDOC|RUSTC_WRAPPER|RUSTC_WORKSPACE_WRAPPER|\
        RUSTC_BOOTSTRAP|RUSTFLAGS|RUSTDOCFLAGS|CARGO_ENCODED_RUSTFLAGS|\
        RUSTUP_TOOLCHAIN|RUSTUP_OVERRIDE_HOST_TRIPLE|LDFLAGS|CPP|CPPFLAGS|AS|LD|\
        NM|OBJCOPY|OBJDUMP|STRIP|PKG_CONFIG|QEMU_LD_PREFIX|GLIBC_TUNABLES|\
        BASH_ENV|ENV)
            unset "$variable"
            ;;
    esac
done < <(compgen -e)

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
liteinst_target_root=$3
liteinst_stage_dir=$(dirname -- "$liteinst_stable_input")
liteinst_stage_name=$(basename -- "$liteinst_stable_input")

liteinst_runtime_kind=${HERMIT_LITEINST_RUNTIME_KIND:-preload}
case $liteinst_runtime_kind in
    preload)
        liteinst_expected_name=libhermit_liteinst_detcore.so
        ;;
    private-crt)
        liteinst_expected_name=hermit_liteinst_detcore_private.elf
        ;;
    *)
        echo "Unsupported LiteInst runtime build kind: ${HERMIT_LITEINST_RUNTIME_KIND}" >&2
        exit 2
        ;;
esac

if [[ -z $liteinst_stable_input || $liteinst_stage_name == . || $liteinst_stage_name == / ]]; then
    echo "Stable LiteInst runtime path must name a file: $liteinst_stable_input" >&2
    exit 2
fi
if [[ $liteinst_stage_name != "$liteinst_expected_name" ]]; then
    echo "LiteInst runtime destination must be named $liteinst_expected_name: $liteinst_stable_input" >&2
    exit 2
fi

mkdir -p -- "$liteinst_stage_dir"
liteinst_stage_dir=$(realpath -e -- "$liteinst_stage_dir")
liteinst_stable_stage=$liteinst_stage_dir/$liteinst_stage_name
liteinst_stable_provenance=${liteinst_stable_stage}.provenance.json
liteinst_temp_dir=$(
    mktemp -d --tmpdir="$liteinst_stage_dir" ".${liteinst_stage_name}.stage.XXXXXX"
)
liteinst_temp_stage=$liteinst_temp_dir/$liteinst_expected_name
liteinst_temp_provenance=${liteinst_temp_stage}.provenance.json
cleanup_liteinst_temp_stage() {
    if [[ -n ${liteinst_temp_stage:-} ]]; then
        rm -f -- "$liteinst_temp_stage"
    fi
    if [[ -n ${liteinst_temp_provenance:-} ]]; then
        rm -f -- "$liteinst_temp_provenance"
    fi
    if [[ -n ${liteinst_temp_dir:-} ]]; then
        rmdir -- "$liteinst_temp_dir"
    fi
    if [[ -n ${liteinst_temp_source_record:-} ]]; then
        rm -f -- "$liteinst_temp_source_record"
    fi
}
trap cleanup_liteinst_temp_stage EXIT

liteinst_hermit_root=${HERMIT_LITEINST_HERMIT_ROOT:-$root_dir}
liteinst_reverie_root=${HERMIT_LITEINST_REVERIE_ROOT:-}
if [[ -z $liteinst_reverie_root ]]; then
    liteinst_metadata_arguments=(
        metadata
        --offline
        --locked
        --format-version=1
        --manifest-path
        "$root_dir/liteinst-runtime-build/detcore-runtime/Cargo.toml"
    )
    if [[ -n ${HERMIT_LITEINST_CARGO_CONFIG:-} ]]; then
        liteinst_metadata_arguments+=(--config "$HERMIT_LITEINST_CARGO_CONFIG")
    fi
    liteinst_metadata=$("${CARGO:-cargo}" "${liteinst_metadata_arguments[@]}")
    mapfile -t liteinst_reverie_manifests < <(
        jq -r '.packages[] | select(.name == "reverie-liteinst-runtime") | .manifest_path' \
            <<<"$liteinst_metadata"
    )
    if (( ${#liteinst_reverie_manifests[@]} != 1 )); then
        echo "Cargo metadata did not identify exactly one Reverie LiteInst runtime" >&2
        exit 1
    fi
    liteinst_reverie_root=$(
        git -C "$(dirname -- "${liteinst_reverie_manifests[0]}")" rev-parse --show-toplevel
    )
fi

# Cargo target state is reusable only for the exact source trees and runtime
# kind that produced it. The source record performs the authoritative check;
# this key keeps a same-pin source change from reaching a stale build-script
# fingerprint or stale default source-record pathname in the first place.
source_tree_key() {
    local source_root=$1 untracked
    {
        git -C "$source_root" rev-parse HEAD
        git -C "$source_root" status --porcelain=v1 --untracked-files=all
        git -C "$source_root" diff --no-ext-diff --binary HEAD --
        while IFS= read -r -d '' untracked; do
            printf '%s\0' "$untracked"
            if [[ -L $source_root/$untracked ]]; then
                readlink -- "$source_root/$untracked"
            elif [[ -f $source_root/$untracked ]]; then
                sha256sum -- "$source_root/$untracked"
            else
                printf 'non-regular\n'
            fi
        done < <(
            git -C "$source_root" ls-files --others --exclude-standard --deduplicate -z
        )
    } | sha256sum | cut -d ' ' -f 1
}

liteinst_hermit_key=$(source_tree_key "$liteinst_hermit_root")
liteinst_reverie_key=$(source_tree_key "$liteinst_reverie_root")
if [[ -n ${HERMIT_LITEINST_CARGO_CONFIG:-} ]]; then
    liteinst_config_key=$(sha256sum -- "$HERMIT_LITEINST_CARGO_CONFIG" | cut -d ' ' -f 1)
else
    liteinst_config_key=none
fi
liteinst_cache_key=$(
    printf '%s\n%s\n%s\n%s\n' \
        "$liteinst_runtime_kind" "$liteinst_hermit_key" "$liteinst_reverie_key" \
        "$liteinst_config_key" | sha256sum | cut -d ' ' -f 1
)
liteinst_target_dir=$(
    realpath -m -- "$liteinst_target_root-${reverie_pin:0:8}-$liteinst_runtime_kind-${liteinst_cache_key:0:16}"
)
mkdir -p -- "$liteinst_target_dir"
liteinst_source_record=$liteinst_target_dir/source-record.json
liteinst_published_source_record=${HERMIT_LITEINST_SOURCE_RECORD:-${liteinst_stable_stage}.source-record.json}
liteinst_source_record_dir=$(dirname -- "$liteinst_published_source_record")
mkdir -p -- "$liteinst_source_record_dir"
liteinst_source_record_dir=$(realpath -e -- "$liteinst_source_record_dir")
liteinst_published_source_record=$liteinst_source_record_dir/$(basename -- "$liteinst_published_source_record")

liteinst_build_arguments=(
    build
    --offline
    --locked
    --manifest-path "$root_dir/liteinst-runtime-build/Cargo.toml"
    --profile "$liteinst_profile"
    --target-dir "$liteinst_target_dir"
)
if [[ -n ${HERMIT_LITEINST_CARGO_CONFIG:-} ]]; then
    liteinst_build_arguments+=(--config "$HERMIT_LITEINST_CARGO_CONFIG")
fi
HERMIT_LITEINST_STAGE=$liteinst_temp_stage \
HERMIT_LITEINST_SOURCE_RECORD=$liteinst_source_record \
HERMIT_LITEINST_HERMIT_ROOT=$liteinst_hermit_root \
HERMIT_LITEINST_REVERIE_ROOT=$liteinst_reverie_root \
"${CARGO:-cargo}" "${liteinst_build_arguments[@]}"

if [[ ! -s $liteinst_temp_stage || ! -f $liteinst_temp_stage || -L $liteinst_temp_stage ]]; then
    echo "LiteInst runtime build did not stage a non-empty regular file: $liteinst_temp_stage" >&2
    exit 1
fi
if [[ ! -s $liteinst_temp_provenance || ! -f $liteinst_temp_provenance || -L $liteinst_temp_provenance ]]; then
    echo "LiteInst runtime build did not stage provenance: $liteinst_temp_provenance" >&2
    exit 1
fi
if [[ ! -s $liteinst_source_record || ! -f $liteinst_source_record || -L $liteinst_source_record ]]; then
    echo "LiteInst runtime build did not produce a source record: $liteinst_source_record" >&2
    exit 1
fi
liteinst_temp_source_record=$(
    mktemp --tmpdir="$liteinst_source_record_dir" ".$(basename -- "$liteinst_published_source_record").stage.XXXXXX"
)
cp -- "$liteinst_source_record" "$liteinst_temp_source_record"

# The unique destinations above force Cargo to rerun the staging build script.
# They are adjacent to the stable paths, so each rename is atomic. Move the DSO
# first: while the two renames are in flight, a new DSO with old or absent
# provenance is refused. Moving provenance first could make an old DSO look
# current.
mv -fT -- "$liteinst_temp_stage" "$liteinst_stable_stage"
liteinst_temp_stage=
mv -fT -- "$liteinst_temp_provenance" "$liteinst_stable_provenance"
liteinst_temp_provenance=
rmdir -- "$liteinst_temp_dir"
liteinst_temp_dir=
mv -fT -- "$liteinst_temp_source_record" "$liteinst_published_source_record"
liteinst_temp_source_record=
