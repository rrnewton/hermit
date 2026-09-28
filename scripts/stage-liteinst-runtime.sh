#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

export CARGO_NET_OFFLINE=true
unset HERMIT_LITEINST_LEGACY_HOST

if (( $# != 3 )); then
    echo "Usage: $0 <cargo-profile> <stable-runtime-path> <runtime-target-root>" >&2
    exit 2
fi

root_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
export HERMIT_LITEINST_HERMIT_ROOT=$root_dir
: "${HERMIT_LITEINST_SOURCE_RECORD:?verified source record is required}"
: "${HERMIT_LITEINST_REVERIE_ROOT:?resolved Reverie root is required}"
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
liteinst_stable_marker=${liteinst_stable_stage}.provenance.json
liteinst_temp_dir=$(
    mktemp -d --tmpdir="$liteinst_stage_dir" ".${liteinst_stage_name}.stage.XXXXXX"
)
liteinst_temp_stage=$liteinst_temp_dir/runtime.so
liteinst_temp_marker=${liteinst_temp_dir}/runtime.so.provenance.json
cleanup_liteinst_temp_stage() {
    if [[ -n ${liteinst_temp_stage:-} ]]; then
        rm -f -- "$liteinst_temp_stage"
    fi
    if [[ -n ${liteinst_temp_marker:-} ]]; then
        rm -f -- "$liteinst_temp_marker"
    fi
    if [[ -n ${liteinst_temp_dir:-} ]]; then
        rmdir -- "$liteinst_temp_dir"
    fi
}
trap cleanup_liteinst_temp_stage EXIT

config_args=()
if [[ -n ${HERMIT_LITEINST_CARGO_CONFIG:-} ]]; then
    config_args=(--config "$HERMIT_LITEINST_CARGO_CONFIG")
fi
if [[ ${HERMIT_LITEINST_DIAGNOSTIC:-0} == 1 ]]; then
    case "$liteinst_stage_dir/" in
        "$root_dir/"*|"$(realpath -e -- "$HERMIT_LITEINST_REVERIE_ROOT")/"*)
            echo 'diagnostic artifacts must remain outside product trees' >&2
            exit 1
            ;;
    esac
fi
HERMIT_LITEINST_STAGE=$liteinst_temp_stage "${CARGO:-cargo}" build "${config_args[@]}" \
    --locked --offline \
    --manifest-path "${HERMIT_LITEINST_BUILD_MANIFEST:-$root_dir/liteinst-runtime-build/Cargo.toml}" \
    --profile "$liteinst_profile" \
    --target-dir "$liteinst_target_dir"

if [[ ! -s $liteinst_temp_stage || ! -f $liteinst_temp_stage || -L $liteinst_temp_stage ]]; then
    echo "LiteInst runtime build did not stage a non-empty regular file: $liteinst_temp_stage" >&2
    exit 1
fi

if [[ ! -s $liteinst_temp_marker || ! -f $liteinst_temp_marker || -L $liteinst_temp_marker ]]; then
    echo 'validated Detcore provenance was not staged' >&2
    exit 1
fi

# The unique destinations above force Cargo to rerun the staging build script.
# They are adjacent to the stable paths, so each rename is atomic. Move the DSO
# first: while the two renames are in flight, a new DSO with an old or absent
# marker is refused. Moving the marker first could make an old DSO look current.
mv -fT -- "$liteinst_temp_stage" "$liteinst_stable_stage"
liteinst_temp_stage=
mv -fT -- "$liteinst_temp_marker" "$liteinst_stable_marker"
liteinst_temp_marker=
rmdir -- "$liteinst_temp_dir"
liteinst_temp_dir=
