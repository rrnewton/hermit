#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Compile and run the relaxation-flag classification guard with the installed
# Rust toolchain rather than relying on its developer-friendly rust-script
# shebang. GitHub's portable images provide rustc but intentionally do not
# install rust-script, so the DAG lane compiles the same checker source on a
# pristine image. Mirrors ci/run-reverie-pin-check.sh deliberately: one wrapper
# shape for every rust-script check keeps the CI surface uniform.
#
# `--self-test` runs the checker's own unit tests. The DAG node runs BOTH, tests
# first, for the same reason the Reverie pin node does: a checker whose own
# tests are broken cannot be trusted to render a verdict on the tree.

set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

mode='run'
if [[ ${1:-} == --self-test ]]; then
    mode='test'
    shift
fi

mkdir -p target/ci
compile_dir=$(mktemp -d "$ROOT_DIR/target/ci/check-relaxation-flags.XXXXXX")
checker="$compile_dir/checker"
trap 'rm -rf -- "$compile_dir"' EXIT
if [[ $mode == test ]]; then
    if (($# != 0)); then
        echo "usage: ci/run-relaxation-flag-check.sh --self-test" >&2
        exit 2
    fi
    RUSTUP_TOOLCHAIN=stable rustc --edition=2021 --test \
        scripts/check-relaxation-flags-classified.rs -o "$checker"
else
    RUSTUP_TOOLCHAIN=stable rustc --edition=2021 \
        scripts/check-relaxation-flags-classified.rs -o "$checker"
fi

"$checker" "$@"
