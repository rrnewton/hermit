#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# End-to-end coreutils pipeline determinism fixture.
#
# A multi-stage coreutils pipeline whose output is driven by coreutils' own
# randomness: `shuf` and `sort -R` read OS entropy (getrandom / /dev/urandom),
# so the shuffled ordering -- and every downstream stage that preserves it --
# varies every run natively. Under Hermit --strict that entropy is determinized,
# so the entire pipeline, the file-I/O roundtrip, and the final SHA-256 digest
# are bitwise reproducible. `nl` numbers the shuffled lines before any sort so
# the digest depends on shuf's order rather than the canonical value order.

set -euo pipefail

case ${1:-} in
    --prepare)
        for command in seq shuf sort uniq nl awk tr cut paste head sha256sum wc; do
            command -v "$command" >/dev/null
        done
        ;;
    --run)
        root=${E2E_TMPDIR:-/tmp}/hermit-coreutils-shuf-pipeline
        rm -rf -- "$root"
        mkdir -p -- "$root"

        # Stage 1: shuf randomizes the sequence (entropy source), then nl
        # numbers the shuffled lines so their position -- and therefore the
        # final digest -- reflects shuf's order. A later sort cannot erase it.
        shuffled=$root/shuffled.txt
        seq 1 300 | shuf | nl -ba -w1 -s: >"$shuffled"

        # Multi-stage coreutils pipeline over the shuffled, numbered data:
        # reformat with awk, retab with tr, project a column with cut, dedupe
        # positions with sort|uniq, and flatten with paste.
        pipeline=$(
            awk -F: '{ print $1 " " $2 }' "$shuffled" |
                tr ' ' '\t' |
                cut -f1,2 |
                sort -t$'\t' -k1,1n |
                uniq |
                cut -f2 |
                paste -sd, -
        )

        # First ten shuffled values, order-sensitive.
        first10=$(head -10 "$shuffled" | cut -d: -f2 | paste -sd, -)

        # sort -R: coreutils random-sort, a second independent entropy consumer.
        rand_sort=$(seq 1 40 | sort -R | paste -sd, -)

        # File I/O roundtrip: persist the pipeline material and read it back.
        out=$root/pipeline.txt
        printf '%s\n%s\n%s\n' "$pipeline" "$first10" "$rand_sort" >"$out"
        readback=$(cat "$out")
        bytes=$(printf '%s' "$readback" | wc -c)
        digest=$(sha256sum "$out" | cut -d' ' -f1)
        lines=$(wc -l <"$shuffled")

        printf 'COREUTILS lines=%s first10=%s rand_sort=%s bytes=%s sha256=%s\n' \
            "$lines" "$first10" "$rand_sort" "$bytes" "$digest"
        ;;
    *)
        echo "usage: $0 --prepare|--run" >&2
        exit 2
        ;;
esac
