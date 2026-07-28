#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

version=${RUST_SCRIPT_VERSION:-0.36.0}
expected="rust-script $version"

if command -v rust-script >/dev/null 2>&1 &&
    [[ $(rust-script --version) == "$expected" ]]; then
    echo "$expected is already installed"
    exit 0
fi

install=(cargo install rust-script --locked --version "$version")
if command -v rust-script >/dev/null 2>&1; then
    install+=(--force)
fi

if command -v with-proxy >/dev/null 2>&1; then
    with-proxy "${install[@]}"
else
    "${install[@]}"
fi

actual=$(rust-script --version)
if [[ $actual != "$expected" ]]; then
    echo "install-rust-script: expected '$expected', got '$actual'" >&2
    exit 1
fi
echo "$actual installed"
