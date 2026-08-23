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

/* Number of behavioural checks this fixture must complete UNDER HERMIT; a lower
   count is a failure, not a smaller success. Native reaches fewer because it
   does not pin the virtualized identity this fixture reads.

   CONSEQUENCE TO WEIGH BEFORE ENABLING ANOTHER CELL. The only enabled cell is
   verify/ptrace, and 4 is measured correct there. This file's header above also
   records that the DBT backend forwards the host nodename and so reaches only 3.
   MEASURED 2026-08-23 on hermit 485a0ad4 built with third-party-backends, that is
   no longer what DBT does: `--backend=dbt` reaches 4 and exits 0, i.e. all four
   pinned values including nodename matched. The header's claim looks stale rather
   than wrong-at-the-time; it is left in place because confirming when the leak
   closed is not this change's job. Either way the gate reports what it observes,
   so a backend that does leak the nodename exits 1 instead of passing quietly.
*/
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
    /* Route a behavioural failure into the exit status. Without this the guest
       exits 0 whatever `ok` reached, so a regression only lowered the printed
       number -- and under --verify both runs lower it identically, so the
       comparison still matches and the cell stays green. Every check above is
       unchanged; this only requires all of them. */
    if (ok != EXPECTED_CHECKS_UNDER_HERMIT) {
        fprintf(stderr, "uname completed %d of %d checks\n", ok, EXPECTED_CHECKS_UNDER_HERMIT);
        return 1;
    }
    return 0;
}
