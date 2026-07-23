#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Regression test for rseq determinization.
#
# glibc >= 2.35 registers a restartable-sequences (rseq) area at startup. Before
# Detcore handled the rseq syscall, `--strict` (panic_on_unsupported_syscalls)
# aborted on that very first syscall, so *no* modern-glibc binary could run.
# Detcore now returns -ENOSYS for rseq, telling glibc to use its deterministic
# fallback path. This test guards that a plain dynamically-linked binary runs to
# completion under --strict and that its execution verifies as deterministic --
# with no GLIBC_TUNABLES workaround.

set -euo pipefail

if [ "$*" == "" ]; then
    hermit="hermit"
else
    hermit="$1"
fi

# Must NOT depend on any GLIBC_TUNABLES rseq opt-out.
unset GLIBC_TUNABLES || true

output=$("$hermit" run --strict -- /bin/echo hello_rseq)
if [ "$output" != "hello_rseq" ]; then
    echo "Expected 'hello_rseq' from echo under --strict, got: '$output'"
    exit 1
fi
echo "echo ran under --strict (rseq handled)."

# --verify re-runs the guest and diffs the deterministic logs.
"$hermit" run --strict --verify -- /bin/echo hello_rseq
echo "echo verified deterministic under --strict."

echo "Test succeeded."
