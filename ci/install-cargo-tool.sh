#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Install a standalone Cargo tool without inheriting the repository's build
# policy.  Workflow-level RUSTFLAGS, compiler wrappers, target selection, and
# native linker flags are inputs to Hermit builds; applying them to an unrelated
# tool can make provisioning depend on Hermit's libraries or target layout.
#
# Keep Cargo home and network/registry variables intact: they select the install
# destination and provide the proxy/credentials needed to fetch the tool.

set -euo pipefail

if (( $# == 0 )); then
    echo "usage: ci/install-cargo-tool.sh TOOL [cargo-install-options...]" >&2
    exit 2
fi

declare -a scrubbed=()
while IFS='=' read -r name _; do
    case "$name" in
        RUSTFLAGS | RUSTDOCFLAGS | CARGO_ENCODED_RUSTFLAGS | \
        RUSTC | RUSTDOC | RUSTC_WRAPPER | RUSTC_WORKSPACE_WRAPPER | \
        CARGO_BUILD_* | CARGO_TARGET_* | CARGO_PROFILE_* | \
        CARGO_TARGET_DIR | CARGO_INCREMENTAL | CARGO_CACHE_RUSTC_INFO | \
        CC | CC_* | HOST_CC | TARGET_CC | \
        CXX | CXX_* | HOST_CXX | TARGET_CXX | \
        AR | AR_* | HOST_AR | TARGET_AR | \
        RANLIB | RANLIB_* | HOST_RANLIB | TARGET_RANLIB | \
        CFLAGS | CFLAGS_* | CXXFLAGS | CXXFLAGS_* | CPPFLAGS | CPPFLAGS_* | \
        LDFLAGS | LDFLAGS_*)
            scrubbed+=("$name")
            ;;
    esac
done < <(env)

declare -a clean_env=()
for name in "${scrubbed[@]}"; do
    clean_env+=(-u "$name")
done

if (( ${#scrubbed[@]} > 0 )); then
    printf 'install-cargo-tool: scrubbed project build environment: %s\n' "${scrubbed[*]}" >&2
else
    echo "install-cargo-tool: project build environment already clean" >&2
fi

exec env "${clean_env[@]}" cargo install "$@"
