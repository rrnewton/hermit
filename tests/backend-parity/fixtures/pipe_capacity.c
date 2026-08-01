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
 * timing, or cross-thread input, so it is identical across repeated runs. The
 * capacity numbers themselves are never printed, keeping the byte stream
 * ("pipecap ok=5") stable regardless of the host's default pipe size or
 * pipe-max-size limit.
 */
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>

int main(void) {
    int ok = 0;
    int fds[2];
    if (pipe2(fds, 0) == 0) {
        ok++;
    }
    int def = fcntl(fds[0], F_GETPIPE_SZ);
    if (def > 0) {
        ok++;
    }
    int shrunk = fcntl(fds[1], F_SETPIPE_SZ, 4096);
    if (shrunk > 0) {
        ok++;
    }
    if (shrunk > 0 && shrunk <= def) {
        ok++;
    }
    if (shrunk > 0 && fcntl(fds[1], F_GETPIPE_SZ) == shrunk) {
        ok++;
    }
    close(fds[0]);
    close(fds[1]);
    printf("pipecap ok=%d\n", ok);
    return 0;
}
