#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Every literal compatibility ratchet must carry an immediately preceding,
# machine-checkable provenance block. A known origin records what was measured,
# its date and full SHA, the method, and why the value is the right ratchet. If
# history cannot establish an origin, the block must say "origin unknown" and
# "re-derive before trusting" instead of inventing a justification.

set -euo pipefail

target=${1:-validate.sh}
if [[ ! -f $target ]]; then
    printf 'ratchet-provenance lint: not a file: %s\n' "$target" >&2
    exit 2
fi

awk '
    function clear_block() {
        block = ""
    }

    function lint_ratchet(line, line_number,    fields, measured, parts, n, i,
                           missing, name, known_origin, sha) {
        name = line
        sub(/^[[:space:]]*readonly[[:space:]]+/, "", name)
        sub(/=.*/, "", name)

        missing = ""
        if (block !~ /# Ratchet provenance:/) missing = missing " header"
        if (block !~ /#   What:/) missing = missing " What"
        if (block !~ /#   Measured:/) missing = missing " Measured"
        if (block !~ /#   Method:/) missing = missing " Method"
        if (block !~ /#   Why:/) missing = missing " Why"
        if (missing != "") {
            printf "%s:%d: %s lacks ratchet provenance field(s):%s\n", \
                FILENAME, line_number, name, missing > "/dev/stderr"
            failures++
            return
        }

        n = split(block, fields, "\n")
        measured = ""
        for (i = 1; i <= n; i++) {
            if (fields[i] ~ /^#   Measured:/) {
                measured = fields[i]
                sub(/^#   Measured:[[:space:]]*/, "", measured)
                break
            }
        }

        if (measured ~ /origin unknown/) {
            if (block !~ /re-derive before trusting/) {
                printf "%s:%d: %s has unknown origin without re-derive warning\n", \
                    FILENAME, line_number, name > "/dev/stderr"
                failures++
            }
            return
        }

        split(measured, parts, /[[:space:];]+/)
        sha = parts[3]
        sub(/[^0-9a-f].*$/, "", sha)
        known_origin = parts[1] ~ \
            /^[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]$/ \
            && parts[2] == "at" \
            && length(sha) == 40 \
            && sha ~ /^[0-9a-f]+$/
        if (!known_origin) {
            printf "%s:%d: %s Measured field needs YYYY-MM-DD at FULL_SHA or explicit origin unknown\n", \
                FILENAME, line_number, name > "/dev/stderr"
            failures++
        }
    }

    /^[[:space:]]*#/ {
        block = block $0 "\n"
        next
    }

    /^[[:space:]]*readonly[[:space:]]+[A-Z0-9_]+_(EXPECTED|TOTAL)=[0-9]+[[:space:]]*$/ {
        checked++
        lint_ratchet($0, FNR)
        clear_block()
        next
    }

    {
        clear_block()
    }

    END {
        if (failures > 0) {
            printf "ratchet-provenance lint: %d failure(s)\n", failures > "/dev/stderr"
            exit 1
        }
        printf "ratchet-provenance lint: %d asserted ratchet(s) have provenance\n", checked
    }
' "$target"
