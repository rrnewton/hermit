#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Brackets validate.sh's truncated-build-artifact pre-flight BOTH WAYS, because a
# purge that deletes nothing and a purge that deletes everything both "pass" a
# one-sided test:
#
#   NEGATIVE (must be REMOVED): 0-byte and header-truncated .o/.a/.so -- the
#     corruption an OOM-killed neighbour leaves behind, which make/cmake then
#     trust forever because they key freshness on timestamp, not content.
#   POSITIVE (must be PRESERVED): well-formed .o/.a/.so, and unrelated files.
#     This is the guard that stops the purge from degenerating into a cold-cache
#     "clean everything", which costs +232s and fails more.
#
# Also asserts the returned COUNT, so a purge that silently removes extra files
# cannot pass by removing the right ones too.

set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

# Source only the two functions under test, without running validate.sh itself.
eval "$(awk '/^function object_header_is_valid \{/,/^\}/' "$ROOT_DIR/validate.sh")"
eval "$(awk '/^function purge_zero_byte_objects \{/,/^\}/' "$ROOT_DIR/validate.sh")"

failures=0
tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT

function check {
    local desc=$1 expected=$2 actual=$3
    if [[ $expected == "$actual" ]]; then
        printf 'ok   %s\n' "$desc"
    else
        printf 'FAIL %s (expected %q, got %q)\n' "$desc" "$expected" "$actual"
        failures=$((failures + 1))
    fi
}

# ---- fixtures ---------------------------------------------------------------
mkdir -p "$tmp/tree/nested"

# NEGATIVE: corrupt artifacts that must be removed.
: >"$tmp/tree/empty.o"                                   # 0-byte object
: >"$tmp/tree/nested/empty.a"                            # 0-byte archive
: >"$tmp/tree/nested/empty.so"                           # 0-byte shared object
printf '\x7f' >"$tmp/tree/truncated.o"                   # died after 1 byte of ELF magic
printf '!<ar' >"$tmp/tree/nested/truncated.a"            # died mid ar magic
printf '\x7fELX' >"$tmp/tree/wrong-magic.so"             # nonzero, not ELF
printf '\x7fELF' >"$tmp/tree/libfoo.so.1"                # versioned .so IS covered

# POSITIVE: healthy artifacts that must survive.
printf '\x7fELF\x02\x01\x01\x00 rest of a real object' >"$tmp/tree/good.o"
printf '!<arch>\n/               0  ' >"$tmp/tree/nested/good.a"
printf '\x7fELF\x02\x01\x01\x00 rest of a real dso' >"$tmp/tree/nested/good.so"
printf 'not an object at all' >"$tmp/tree/notes.txt"     # unrelated extension
: >"$tmp/tree/empty.txt"                                 # 0-byte but NOT an artifact

# libfoo.so.1 has valid magic, so it must be preserved -- it is here to prove the
# *.so.* glob is reached, not to be deleted.
removed=$(purge_zero_byte_objects "$tmp/tree")

# ---- negative side: corrupt artifacts gone ----------------------------------
for f in empty.o truncated.o wrong-magic.so nested/empty.a nested/empty.so nested/truncated.a; do
    check "removed corrupt $f" "absent" "$([[ -e $tmp/tree/$f ]] && echo present || echo absent)"
done

# ---- positive side: healthy artifacts and non-artifacts preserved -----------
for f in good.o libfoo.so.1 nested/good.a nested/good.so notes.txt empty.txt; do
    check "preserved $f" "present" "$([[ -e $tmp/tree/$f ]] && echo present || echo absent)"
done

# ---- the count is part of the contract (validate.sh logs it + ledgers it) ---
check "removed count" "6" "$removed"

# ---- a tree with nothing wrong must be a no-op ------------------------------
mkdir -p "$tmp/clean"
printf '\x7fELF\x02\x01\x01\x00 healthy' >"$tmp/clean/only-good.o"
check "clean tree removes nothing" "0" "$(purge_zero_byte_objects "$tmp/clean")"
check "clean tree keeps its object" "present" \
    "$([[ -e $tmp/clean/only-good.o ]] && echo present || echo absent)"

# ---- a missing tree is tolerated, as validate.sh relies on ------------------
check "absent root returns 0" "0" "$(purge_zero_byte_objects "$tmp/does-not-exist")"

if ((failures > 0)); then
    printf '\n%d check(s) failed\n' "$failures" >&2
    exit 1
fi
printf '\nall checks passed\n'
