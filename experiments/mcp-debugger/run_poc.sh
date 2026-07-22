#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Run the MCP-debugger proof of concept end to end. Assumes ./setup.sh has
# already fetched/built the MCP server and venv.
#
# Exit codes:  0 = PASS,  1 = ran but FAILED,  2 = SKIP (missing prerequisite,
# e.g. no PMU / gdb / built server). SKIP keeps CI green on hosts that cannot
# run hermit's gdbserver, matching hermit's hardware-sensitivity conventions.
set -uo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
BUILD="$HERE/.build"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"

# Locate the hermit binary: env override, then release, then debug build.
HERMIT="${HERMIT_BIN:-}"
if [[ -z "$HERMIT" ]]; then
    for cand in "$REPO_ROOT/target/release/hermit" "$REPO_ROOT/target/debug/hermit"; do
        [[ -x "$cand" ]] && HERMIT="$cand" && break
    done
fi

skip() { echo ":: SKIP: $*"; exit 2; }

command -v gdb        >/dev/null 2>&1 || skip "gdb not on PATH"
[[ -n "$HERMIT" && -x "$HERMIT" ]]    || skip "hermit binary not found (build it: cargo build -p hermit)"
[[ -x "$BUILD/mcp_server_gdb/target/release/mcp-server-gdb" ]] || skip "mcp-server-gdb not built (run setup.sh)"
[[ -x "$BUILD/demo" ]]               || skip "demo not built (run setup.sh)"
[[ -x "$BUILD/venv/bin/python" ]]    || skip "python venv not found (run setup.sh)"

echo ":: hermit         = $HERMIT"
echo ":: mcp-server-gdb = $BUILD/mcp_server_gdb/target/release/mcp-server-gdb"

"$BUILD/venv/bin/python" "$HERE/mcp_gdb_agent.py" \
    --hermit "$HERMIT" \
    --mcp-gdb "$BUILD/mcp_server_gdb/target/release/mcp-server-gdb" \
    --demo "$BUILD/demo" \
    --demo-src "$HERE/demo.c"
rc=$?

case "$rc" in
    0) echo ":: RESULT: PASS" ;;
    2) echo ":: RESULT: SKIP" ;;
    *) echo ":: RESULT: FAIL (rc=$rc)" ;;
esac
exit "$rc"
