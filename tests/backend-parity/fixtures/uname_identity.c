/*
 * uname_identity: cross-backend determinization contract for uname(2).
 *
 * Hermit pins the guest kernel identity so that a program's view of the host
 * does not leak nondeterministic, host-specific facts. This fixture asserts the
 * fields Hermit is expected to determinize identically for every guest:
 *
 *   sysname  == "Linux"                    (kernel family, host-generic)
 *   machine  == "x86_64"                   (ISA, host-generic on this corpus)
 *   release  == "5.2.0"                    (pinned kernel release; native leaks
 *                                           the real running kernel, e.g. 6.x)
 *   nodename == "hermetic-container.local" (pinned hostname; native and the DBT
 *                                           backend leak the real host hostname)
 *
 * The ptrace and KVM backends determinize all four. The DBT (DynamoRIO) backend
 * pins release but forwards the *host* nodename, so it deterministically-but-
 * host-dependently fails the nodename check; matrix.tsv records that as a DBT
 * gap. Native Linux honors none of the pinned values, proving these are Hermit
 * determinization choices rather than host coincidences.
 *
 * Uses only the libc uname() wrapper and POSIX <sys/utsname.h>; no _GNU_SOURCE.
 */
#include <stdio.h>
#include <string.h>
#include <sys/utsname.h>

#define PINNED_SYSNAME "Linux"
#define PINNED_MACHINE "x86_64"
#define PINNED_RELEASE "5.2.0"
#define PINNED_NODENAME "hermetic-container.local"

/* Number of behavioural checks this fixture must complete under Hermit; a
   lower count is a failure, not a smaller success. Named for Hermit because
   the value is environment-dependent: native honours none of the pinned
   values and the DBT backend forwards the host nodename, so only the
   determinizing backends reach 4. The one enabled cell is verify/ptrace. */
#define EXPECTED_CHECKS_UNDER_HERMIT 4

int main(void) {
    int ok = 0;
    struct utsname u;

    memset(&u, 0, sizeof(u));
    if (uname(&u) == 0) {
        if (strcmp(u.sysname, PINNED_SYSNAME) == 0) {
            ok += 1;
        }
        if (strcmp(u.machine, PINNED_MACHINE) == 0) {
            ok += 1;
        }
        if (strcmp(u.release, PINNED_RELEASE) == 0) {
            ok += 1;
        }
        if (strcmp(u.nodename, PINNED_NODENAME) == 0) {
            ok += 1;
        }
    }

    printf("uname ok=%d\n", ok);

    /* Route a behavioural failure into the exit status. Without this the
       guest exits 0 whatever `ok` reached, so a field Hermit determinizes
       stopped matching its pinned value only lowered the printed number --
       and under --verify both runs lower it identically, so the comparison
       still matches and the cell stays green. The checks above are unchanged;
       this only requires all of them. */
    if (ok != EXPECTED_CHECKS_UNDER_HERMIT) {
        fprintf(stderr, "uname completed %d of %d checks\n",
                ok, EXPECTED_CHECKS_UNDER_HERMIT);
        return 1;
    }

    return 0;
}
