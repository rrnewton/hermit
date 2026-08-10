#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
set -euo pipefail

# Surface: PARTIAL-TRANSFER file I/O in a REAL program.
#
# The corpus exercised partial transfers only through purpose-built C fixtures.
# dd bs=1 across a pipe is the coreutils-native version of the same surface:
# every byte is its own read()/write() pair, so a backend that coalesces or
# splits transfers differently produces a different syscall stream while the
# byte total still matches. Small by design -- 4096 bytes is ~8k syscalls and
# no meaningful compute.
#
# dd's own stats line is suppressed with status=none: it reports a virtual-time
# derived rate, which the time-focused entries already cover, and leaving it in
# would put a second observable in this entry's oracle for no added surface.
case ${1:-} in
    --prepare) exit 0 ;;
    --run)
        work="$E2E_TMPDIR/dd-partial"
        rm -rf "$work"; mkdir -p "$work"
        src="$work/src.bin"
        # Deterministic, compressible-but-not-uniform payload.
        seq 1 512 | tr -d '\n' | head -c 4096 >"$src"
        printf 'SRC %s\n' "$(wc -c <"$src" | tr -d '[:space:]')"

        # Byte-at-a-time through a pipe: the reader cannot get a full block, so
        # every transfer is partial.
        piped=$(cat "$src" | dd bs=1 status=none | wc -c | tr -d '[:space:]')
        printf 'PIPED %s\n' "$piped"

        # Byte-at-a-time to a file, then compare content to prove no byte was
        # dropped or duplicated by the split.
        dd if="$src" of="$work/out.bin" bs=1 status=none
        printf 'COPIED %s\n' "$(wc -c <"$work/out.bin" | tr -d '[:space:]')"
        printf 'IDENTICAL %s\n' "$(cmp -s "$src" "$work/out.bin" && echo yes || echo no)"

        # A short odd-sized block count exercises the final partial block.
        odd=$(dd if="$src" bs=7 count=13 status=none | wc -c | tr -d '[:space:]')
        printf 'ODDBLOCK %s\n' "$odd"
        ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
