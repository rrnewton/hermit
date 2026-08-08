/*
 * F_GETPIPE_SZ / F_SETPIPE_SZ pipe-capacity round-trip parity fixture.
 *
 * Exercises the pipe-buffer sizing fcntl command family (distinct from the
 * F_GETFD/F_SETFD descriptor-flag and F_GETFL/F_SETFL status-flag namespaces
 * already covered elsewhere). It asserts only backend-invariant relational
 * properties and never an absolute, host-config-derived capacity, so the
 * golden stdout is portable across hosts and kernels:
 *   1. pipe2 opens a pipe.
 *   2. the default capacity reported by F_GETPIPE_SZ is positive.
 *   3. shrinking the capacity to one page returns a positive rounded size
 *      (shrinking is always permitted for an unprivileged process; growing a
 *      pipe can require CAP_SYS_RESOURCE under per-user page accounting and is
 *      deliberately never attempted).
 *   4. the shrunk capacity does not exceed the original default.
 *   5. a subsequent F_GETPIPE_SZ echoes exactly the size the shrink returned.
 *
 * Every observable is process-local pipe-object state with no host-derived,
 * timing, or cross-thread input, so it is identical across repeated runs.
 *
 * The GUEST-CHOSEN capacity is printed; the HOST's default is not. That split is
 * the point. Printing only "pipecap ok=5" kept the byte stream host-independent
 * but made the fixture structurally blind twice over: a sum cannot distinguish
 * which check failed, and a `shrunk > 0` predicate accepts any positive size, so
 * a backend that consistently reported a wrong-but-positive capacity scored a
 * clean ok=5. Two backends doing that AGREE, so cross-backend parity could never
 * see it either -- only an expected-value oracle can. Printing the value the
 * guest itself asked for (4096) and comparing it exactly closes that, while the
 * host's default pipe size stays out of the byte stream so the output remains
 * host-independent.
 */
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

int main(void) {
    enum { EXPECTED_CHECKS = 5, WANT_PIPE_SZ = 4096 };
    int ok = 0;
    int fds[2] = {-1, -1};
    if (pipe2(fds, 0) != 0) {
        printf("pipecap ok=0\n");
        return EXIT_FAILURE;
    }
    ok++;
    int def = fcntl(fds[0], F_GETPIPE_SZ);
    if (def > 0) {
        ok++;
    }
    int shrunk = fcntl(fds[1], F_SETPIPE_SZ, WANT_PIPE_SZ);
    /* Exact, not "> 0": the guest ASKED for WANT_PIPE_SZ, so any other positive
     * answer is wrong. The old `shrunk > 0` accepted every positive value, which
     * is the hole -- two backends could both return a wrong-but-positive size and
     * both score ok=5. */
    if (shrunk == WANT_PIPE_SZ) {
        ok++;
    }
    if (shrunk > 0 && shrunk <= def) {
        ok++;
    }
    int readback = fcntl(fds[1], F_GETPIPE_SZ);
    if (readback == shrunk) {
        ok++;
    }
    close(fds[0]);
    close(fds[1]);
#ifdef HERMIT_TEST_ORACLE_NEGATIVE
    ok--; /* plant one failed contract check to bracket the exit oracle */
#endif
    /* Emit the OBSERVED VALUES, not just the check sum. `ok=%d` is a SUM, so two
     * backends that fail DIFFERENT checks alias to the same total and compare
     * equal; and a sum can never expose a wrong-but-accepted value. Only
     * guest-DETERMINED quantities are printed -- the host's default pipe size is
     * deliberately still absent, so the byte stream stays host-independent. */
    printf("pipecap ok=%d set=%d readback=%d fits_default=%d\n",
           ok, shrunk, readback, (shrunk > 0 && shrunk <= def) ? 1 : 0);
    return ok == EXPECTED_CHECKS ? EXIT_SUCCESS : EXIT_FAILURE;
}
