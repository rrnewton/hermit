#include <stdio.h>
#include <sys/resource.h>
#include <sys/time.h>

/*
 * prlimit64 / getrlimit / setrlimit soft-limit round-trip on RLIMIT_NOFILE.
 * The initial soft/hard limits are host-derived and are deliberately never
 * asserted; only the process-local set/get round-trip is checked, which is
 * deterministic because the process supplies the value it later reads back:
 *   1. getrlimit reads the current limits (values not asserted).
 *   2. setrlimit lowers the soft limit to a fixed 64 under the host hard cap.
 *   3. getrlimit echoes the soft value we just set.
 *   4. prlimit(0, ...) (self) reads back the same soft value.
 *   5. prlimit atomically installs soft=32 and returns the previous soft (64).
 *   6. getrlimit confirms the new soft value.
 * Lowering the soft limit is always permitted for an unprivileged process, so
 * the contract needs no capability and mutates no filesystem state.
 */
int main(void) {
    int ok = 0;
    struct rlimit rl, old;

    if (getrlimit(RLIMIT_NOFILE, &rl) == 0) {
        ok++;
    }
    rlim_t hard = rl.rlim_max;

    rl.rlim_cur = 64;
    rl.rlim_max = hard;
    if (setrlimit(RLIMIT_NOFILE, &rl) == 0) {
        ok++;
    }

    struct rlimit chk;
    if (getrlimit(RLIMIT_NOFILE, &chk) == 0 && chk.rlim_cur == 64) {
        ok++;
    }

    if (prlimit(0, RLIMIT_NOFILE, NULL, &old) == 0 && old.rlim_cur == 64) {
        ok++;
    }

    struct rlimit next;
    next.rlim_cur = 32;
    next.rlim_max = hard;
    if (prlimit(0, RLIMIT_NOFILE, &next, &old) == 0 && old.rlim_cur == 64) {
        ok++;
    }

    if (getrlimit(RLIMIT_NOFILE, &chk) == 0 && chk.rlim_cur == 32) {
        ok++;
    }

    printf("prlimit ok=%d\n", ok);
    return 0;
}
