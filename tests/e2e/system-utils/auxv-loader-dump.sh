#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
set -euo pipefail

# Surface: the AUXILIARY VECTOR, which no corpus entry read.
#
# LD_SHOW_AUXV makes glibc's dynamic loader walk and print the auxv it was
# handed by execve. That is real loader code on the real startup path, not a
# --version probe, and it is the only entry that observes AT_RANDOM (the 16
# bytes glibc seeds stack-protector and malloc from), AT_SYSINFO_EHDR (the vdso
# base), AT_HWCAP/AT_HWCAP2, AT_SECURE, AT_CLKTCK and AT_PAGESZ.
#
# Deliberately UNNORMALIZED. AT_RANDOM is an entropy source and AT_SYSINFO_EHDR
# is an address; printing them raw is the point, because a backend that leaks
# host entropy or a host vdso placement into the guest diverges here and
# nowhere else in the corpus. Values are host-specific, so native comparison is
# not a contract -- the naked mode stays disabled.
case ${1:-} in
    --prepare) exit 0 ;;
    --run)
        # /bin/true is the smallest real dynamically-linked binary; the work
        # under test is the loader's, not the program's.
        mkdir -p "$E2E_TMPDIR"
        LD_SHOW_AUXV=1 /bin/true 2>&1 | sort >"$E2E_TMPDIR/auxv.txt"

        # The whole dump, verbatim: every key and value must repeat exactly.
        printf 'AUXV-DUMP\n'
        cat "$E2E_TMPDIR/auxv.txt"

        # Structural summary, so a truncated or empty dump is not silently
        # mistaken for a stable one.
        printf 'AUXV-KEYS %s\n' "$(grep -c '^AT_' "$E2E_TMPDIR/auxv.txt" | tr -d '[:space:]')"
        for key in AT_RANDOM AT_SYSINFO_EHDR AT_PAGESZ AT_SECURE AT_CLKTCK; do
            printf 'HAS %s %s\n' "$key" "$(grep -c "^$key:" "$E2E_TMPDIR/auxv.txt" | tr -d '[:space:]')"
        done
        ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
