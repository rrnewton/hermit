/*
 * keepcaps_prctl: cross-backend round-trip contract for the per-process
 * PR_GET_KEEPCAPS / PR_SET_KEEPCAPS capability-inheritance flag.
 *
 * PR_SET_KEEPCAPS controls whether a process retains its permitted capability
 * set across an all-UID-to-nonzero transition (see capabilities(7) and
 * prctl(2)). The flag is pure per-process state: it does not grant any
 * capability, touch the filesystem, block, consult host time/PID/UID identity,
 * or depend on the scheduler. It therefore has a single deterministic answer
 * that every backend must reproduce identically.
 *
 * The contract toggles the flag off -> on -> off and, after each write, reads
 * it back and requires the observed value to match what was just set. A guest
 * must see a faithful, self-consistent boolean regardless of backend:
 *
 *   1. GET returns the initial value 0 (KEEPCAPS defaults off)
 *   2. SET 1 succeeds (rc == 0)
 *   3. GET now returns exactly 1
 *   4. SET 0 succeeds (rc == 0)
 *   5. GET now returns exactly 0
 *
 * golden ok=5 on native Linux and on all three Hermit backends: this is a
 * faithful-behavior parity contract (the deterministic answer coincides with
 * native), not a determinization override.
 *
 * Uses only the libc prctl() wrapper; no raw syscall, so the harness supplies
 * no -D_GNU_SOURCE for this fixture.
 */
#include <stdio.h>
#include <sys/prctl.h>

#ifndef PR_GET_KEEPCAPS
#define PR_GET_KEEPCAPS 7
#endif
#ifndef PR_SET_KEEPCAPS
#define PR_SET_KEEPCAPS 8
#endif

int main(void) {
    int ok = 0;

    if (prctl(PR_GET_KEEPCAPS, 0, 0, 0, 0) == 0) {
        ok += 1;
    }
    if (prctl(PR_SET_KEEPCAPS, 1, 0, 0, 0) == 0) {
        ok += 1;
    }
    if (prctl(PR_GET_KEEPCAPS, 0, 0, 0, 0) == 1) {
        ok += 1;
    }
    if (prctl(PR_SET_KEEPCAPS, 0, 0, 0, 0) == 0) {
        ok += 1;
    }
    if (prctl(PR_GET_KEEPCAPS, 0, 0, 0, 0) == 0) {
        ok += 1;
    }

    printf("keepcaps ok=%d\n", ok);
    return 0;
}
