#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
set -euo pipefail

case ${1:-} in
    --prepare)
        test -x /usr/bin/find
        ;;
    --run)
        # Under the hygiene lockdown E2E_TMPDIR is absent, so this fixed tree
        # lives in the initially empty guest cwd (/test). The legacy override
        # keeps manual and pre-lockdown runs isolated as before.
        work="${E2E_TMPDIR:-.}/hermit-find-tree-metadata"
        tree="$work/tree"
        rm -rf -- "$work"
        mkdir -p -- "$tree/alpha" "$tree/beta-layer/deeper"
        trap 'rm -rf -- "$work"' EXIT

        # Four leaves at depths 1, 2, and 3, with deliberately varied name
        # lengths and payload sizes. The exact expected listing makes this a
        # live traversal/metadata oracle rather than a command-exit smoke test.
        : >"$tree/root-empty"
        printf 'abc\n' >"$tree/alpha/short.txt"
        printf '123456\n' >"$tree/beta-layer/medium.bin"
        printf '0123456789abcdef\n' \
            >"$tree/beta-layer/deeper/fixed-width-name.dat"

        listing=$(
            /usr/bin/find "$tree" -mindepth 1 -maxdepth 3 -type f \
                -printf '%P %s\n' | LC_ALL=C sort
        )
        expected=$(printf '%s\n' \
            'alpha/short.txt 4' \
            'beta-layer/deeper/fixed-width-name.dat 17' \
            'beta-layer/medium.bin 7' \
            'root-empty 0')
        if [[ $listing != "$expected" ]]; then
            printf 'find listing mismatch\nexpected:\n%s\nactual:\n%s\n' \
                "$expected" "$listing" >&2
            exit 1
        fi
        printf 'FIND files=4 max_depth=3\n%s\n' "$listing"
        ;;
    *)
        echo "usage: $0 --prepare|--run" >&2
        exit 2
        ;;
esac
