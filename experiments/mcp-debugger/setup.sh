#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# One-time setup for the MCP-debugger proof of concept. Fetches and builds the
# off-the-shelf GDB MCP server, creates a Python venv with the MCP SDK, and
# compiles the demo guest. Everything lands under ./.build (gitignored).
#
# Network access on Meta devservers goes through the proxy; prefix with
# `with-proxy` if your shell does not already export HTTPS_PROXY:
#     with-proxy ./setup.sh
set -euo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
BUILD="$HERE/.build"
mkdir -p "$BUILD"

# Pinned upstreams so the POC is reproducible.
GDB_MCP_REPO="https://github.com/pansila/mcp_server_gdb.git"
GDB_MCP_REV="78021078c291ee6aac6dc1893bb09a654c0b19f1"
LLDB_MCP_REPO="https://github.com/stass/lldb-mcp.git"
LLDB_MCP_REV="a610f2d0d3835739c41762352442ba2a13958b38"

echo ":: building demo guest"
cc -g -O0 -o "$BUILD/demo" "$HERE/demo.c"

echo ":: fetching + building pansila/mcp_server_gdb @ ${GDB_MCP_REV:0:12}"
GDB_DIR="$BUILD/mcp_server_gdb"
if [[ ! -d "$GDB_DIR/.git" ]]; then
    git clone "$GDB_MCP_REPO" "$GDB_DIR"
fi
git -C "$GDB_DIR" fetch --quiet origin
git -C "$GDB_DIR" checkout --quiet "$GDB_MCP_REV"
# .build lives inside hermit's cargo workspace; mark the clone as its own
# workspace root so cargo does not try to treat it as a hermit member.
if ! grep -q '^\[workspace\]' "$GDB_DIR/Cargo.toml"; then
    printf '\n[workspace]\n' >> "$GDB_DIR/Cargo.toml"
fi
( cd "$GDB_DIR" && cargo build --release )

echo ":: creating python venv with the MCP SDK"
if [[ ! -d "$BUILD/venv" ]]; then
    python3 -m venv "$BUILD/venv"
fi
"$BUILD/venv/bin/pip" install --quiet --upgrade pip
"$BUILD/venv/bin/pip" install --quiet mcp

# Optional LLDB path (see README "LLDB variant"). Only useful once the reverie
# gdbstub LLDB-handshake fix (rrnewton/reverie PR #21) is in the reverie that
# hermit builds against. Cloned here for convenience; not exercised by run_poc.sh.
if [[ "${WITH_LLDB:-0}" == "1" ]]; then
    echo ":: fetching stass/lldb-mcp @ ${LLDB_MCP_REV:0:12}"
    LLDB_DIR="$BUILD/lldb-mcp"
    if [[ ! -d "$LLDB_DIR/.git" ]]; then
        git clone "$LLDB_MCP_REPO" "$LLDB_DIR"
    fi
    git -C "$LLDB_DIR" checkout --quiet "$LLDB_MCP_REV"
fi

echo ":: setup complete"
echo "   demo:           $BUILD/demo"
echo "   mcp-server-gdb: $GDB_DIR/target/release/mcp-server-gdb"
echo "   venv python:    $BUILD/venv/bin/python"
