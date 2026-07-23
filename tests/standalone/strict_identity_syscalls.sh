#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Regression test for the batch of identity/session syscalls determinized on top
# of the --strict "syscall cascade": getresuid, getresgid, umask, and setpgid.
#
# These typed syscalls previously fell through to Detcore's catch-all dispatch
# arm, which panics under --strict (panic_on_unsupported_syscalls). Detcore now
# passes them through deterministically (the guest runs in a fixed PID namespace
# with a constant credential/umask state), so a program that exercises them runs
# to completion AND verifies as deterministic under --strict.
#
# This test compiles a tiny helper because these syscalls are not reliably
# emitted by common shell tools. If no C compiler is available it skips.

set -euo pipefail

if [ "$*" == "" ]; then
    hermit="hermit"
else
    hermit="$1"
fi

cc_bin=""
for candidate in "${CC:-}" cc gcc clang; do
    if [ -n "$candidate" ] && command -v "$candidate" >/dev/null 2>&1; then
        cc_bin="$candidate"
        break
    fi
done

if [ -z "$cc_bin" ]; then
    echo "SKIP: no C compiler available to build the identity-syscall helper"
    exit 0
fi

unset GLIBC_TUNABLES || true

# The scratch dir lives under the cwd because Hermit isolates the guest's /tmp.
work=$(mktemp -d strict_identity_test_XXXXXXX)
function on_exit {
    rm -rf -- "$work"
}
trap on_exit EXIT

cat > "$work/idtest.c" <<'EOF'
#define _GNU_SOURCE
#include <unistd.h>
#include <sys/stat.h>
#include <stdio.h>

int main(void) {
    uid_t r, e, s;
    gid_t rg, eg, sg;
    getresuid(&r, &e, &s);
    getresgid(&rg, &eg, &sg);
    mode_t old = umask(022);
    umask(old);
    /* Move ourselves into our own process group. */
    setpgid(0, 0);
    printf("resuid=%d/%d/%d resgid=%d/%d/%d umask=%o pgid=%d\n",
           (int) r, (int) e, (int) s,
           (int) rg, (int) eg, (int) sg,
           (int) old, (int) getpgid(0));
    return 0;
}
EOF

"$cc_bin" -O2 -o "$work/idtest" "$work/idtest.c"

if "$hermit" run --strict --verify -- "$work/idtest" < /dev/null 2>&1 \
    | grep -q "Determinism verified"; then
    echo "ok: getresuid/getresgid/umask/setpgid verified deterministic under --strict"
else
    echo "FAIL: identity-syscall helper did not verify deterministic under --strict"
    "$hermit" run --strict --verify -- "$work/idtest" < /dev/null 2>&1 | tail -20
    exit 1
fi

echo "Test succeeded."
