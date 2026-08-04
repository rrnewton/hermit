/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Register-file hashing fixture for `hermit run --verify`.
 *
 * This guest's ONLY run-to-run difference is a single general-purpose register
 * value: argv[1] is parsed into a marker that is pinned into the callee-saved
 * register %r15 and held live across a `getpid` syscall. The syscall does not
 * clobber %r15, so at the syscall-commit boundary -- exactly where hermit
 * samples the register file -- %r15 holds the marker.
 *
 * Crucially, the guest's stdout, stderr, exit status, and syscall SEQUENCE are
 * identical regardless of the marker (getpid's result is discarded and nothing
 * is printed). So a stdout/exit-based `--verify` cannot tell two markers apart;
 * only the register-file hash can. Two runs with the SAME marker produce an
 * identical register-hash stream (determinism); two runs with DIFFERENT markers
 * differ at the getpid `[regs]` line (a hard catch of a real register-state
 * divergence).
 *
 * The marker is kept below 0x1000 so hermit classifies it as a plain value
 * (v<N>) rather than a host address (which would be canonicalized to an
 * ordinal); this makes the divergence a value-token difference.
 */

#include <stdint.h>
#include <stdlib.h>

int main(int argc, char** argv) {
    uint64_t marker = (argc > 1) ? strtoull(argv[1], NULL, 0) : 0;

    /* Pin the marker into %r15 (callee-saved) and issue getpid (syscall 39). */
    register uint64_t r15 asm("r15") = marker;
    long ret;
    __asm__ volatile("syscall"
                     : "=a"(ret)
                     : "a"((long)39), "r"(r15)
                     : "rcx", "r11", "memory");

    (void)ret;
    return 0;
}
