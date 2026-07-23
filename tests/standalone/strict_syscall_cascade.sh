#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Regression test for the --strict "syscall cascade".
#
# After rseq was determinized, real applications advanced past glibc startup and
# then aborted on the next unhandled syscall under --strict
# (panic_on_unsupported_syscalls): lseek, ioctl, getcwd, getuid/gid, getpid,
# mincore, chdir, fadvise64, getpgrp, getgroups, ...
#
# Detcore now handles each of these deterministically (see detcore/src/lib.rs and
# the handlers in detcore/src/syscalls/). This test guards that common tools run
# to completion AND verify as deterministic under --strict, with no
# GLIBC_TUNABLES workaround. It only uses stdin and files in a scratch dir under
# the cwd, because Hermit isolates the guest's /tmp.

set -euo pipefail

if [ "$*" == "" ]; then
    hermit="hermit"
else
    hermit="$1"
fi

unset GLIBC_TUNABLES || true

work=$(mktemp -d strict_cascade_test_XXXXXXX)
function on_exit {
    rm -rf -- "$work"
}
trap on_exit EXIT

printf 'aaa\nbbb\nccc\n' > "$work/in.txt"

# Each entry runs under `--strict --verify` and must report determinism.
check() {
    local name="$1"
    shift
    if "$hermit" run --strict --verify -- "$@" < /dev/null 2>&1 \
        | grep -q "Determinism verified"; then
        echo "ok: $name verified deterministic under --strict"
    else
        echo "FAIL: $name did not verify deterministic under --strict"
        "$hermit" run --strict --verify -- "$@" < /dev/null 2>&1 | tail -20
        exit 1
    fi
}

# lseek + mincore: grep/awk over an mmap'd file.
check grep grep bbb "$work/in.txt"
check awk awk '{print $1}' "$work/in.txt"
# ioctl (isatty -> ENOTTY): compressors probe the terminal.
check gzip gzip -c "$work/in.txt"
# getcwd + chdir: make queries and changes the working directory.
check make make --version
# getuid family + getgroups: perl and awk read process credentials.
check perl perl -e 'print 6*7'
# getpid/getpgrp: bash reads its own pid and process group.
check bash bash -c 'echo hi'

echo "Test succeeded."
