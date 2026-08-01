/* Backend-parity fixture: prctl process-identity and dumpable flags.
 *
 * Exercises the two most portable prctl families that carry no host-derived
 * state: the per-task name (PR_SET_NAME / PR_GET_NAME round-trip through a
 * 16-byte buffer) and the dumpable flag (PR_SET_DUMPABLE / PR_GET_DUMPABLE).
 * Under ptrace and DBI all five checks pass ("prctl ok=5"). The KVM ElfExecutor
 * returns deterministic ENOSYS for PR_SET_NAME / PR_GET_NAME, so it reports
 * "prctl ok=3": the dumpable checks still pass, which is why this row is a
 * documented KVM gap rather than false parity.
 */
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>

int main(void) {
    int ok = 0;

    char name[16];
    memset(name, 0, sizeof name);
    if (prctl(PR_SET_NAME, "parityname", 0, 0, 0) == 0) {
        ok++;
    }
    if (prctl(PR_GET_NAME, name, 0, 0, 0) == 0 &&
        strncmp(name, "parityname", sizeof name) == 0) {
        ok++;
    }

    if (prctl(PR_SET_DUMPABLE, 0, 0, 0, 0) == 0) {
        ok++;
    }
    if (prctl(PR_GET_DUMPABLE, 0, 0, 0, 0) == 0) {
        ok++;
    }
    if (prctl(PR_SET_DUMPABLE, 1, 0, 0, 0) == 0 &&
        prctl(PR_GET_DUMPABLE, 0, 0, 0, 0) == 1) {
        ok++;
    }

    printf("prctl ok=%d\n", ok);
    return 0;
}
