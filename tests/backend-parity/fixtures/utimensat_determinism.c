// utimensat(2) file-timestamp determinization parity probe.
//
// A file's access and modification times are host-derived state: outside Hermit
// they reflect whatever a program sets (or the real wall clock via UTIME_NOW).
// Hermit's determinize_stat normalizes the timestamp fields that stat/fstat
// report to a single deterministic value, so a guest cannot observe the true
// stored times. utimensat itself is accepted (it must not spuriously fail), but
// the value read back is the determinized constant, not the caller's request.
//
// The checks are epoch-agnostic and relational: they never hard-code Hermit's
// internal time base. They assert that the two timestamp fields collapse to a
// single value and that the value the program requested was overridden. Under
// Hermit all five checks pass (ok=5); native passes only the two acceptance
// checks (ok=2) because it faithfully echoes the requested atime/mtime.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <unistd.h>
#include <time.h>

/* Number of behavioural checks this fixture must complete UNDER HERMIT; a lower
   count is a failure, not a smaller success. Native reaches only 2 (the two
   acceptance checks) because it faithfully echoes the requested atime/mtime,
   which is why the constant names Hermit explicitly -- the idiom mincore
   residency established. Its one enabled cell is verify/ptrace, where 5 is
   correct. */
#define EXPECTED_CHECKS_UNDER_HERMIT 5

int main(void) {
    char dir[] = "/tmp/utimensat_determinism.XXXXXX";
    if (!mkdtemp(dir)) {
        printf("utimensat MKDTEMP_FAIL\n");
        return 1;
    }
    char path[256];
    snprintf(path, sizeof(path), "%s/f", dir);
    int fd = open(path, O_CREAT | O_RDWR, 0600);
    if (fd < 0) {
        printf("utimensat OPEN_FAIL\n");
        return 1;
    }

    int ok = 0;

    // Request two distinct explicit timestamps.
    struct timespec ts[2] = {{1111111111, 0}, {2222222222, 0}};
    // (1) utimensat is accepted (native and all backends).
    if (utimensat(AT_FDCWD, path, ts, 0) == 0) ok++;

    struct stat st;
    /* fstat is a PRODUCER: checks (2) and (3) read the struct it fills. It used
       to be called without checking the result, so on failure `st` stayed
       indeterminate and the comparisons below read uninitialised memory --
       whose contents are host state, the one thing a determinism fixture must
       not depend on. A producer that failed cannot yield an observation, so
       this refuses rather than comparing garbage. */
    if (fstat(fd, &st) != 0) {
        fprintf(stderr, "utimensat: fstat failed; no observation to compare\n");
        close(fd);
        unlink(path);
        rmdir(dir);
        return 1;
    }
    long a = st.st_atim.tv_sec;
    long m = st.st_mtim.tv_sec;
    // (2) Determinized: both timestamp fields collapse to one value.
    if (a == m) ok++;
    // (3) Determinized: the requested mtime was overridden (native keeps it).
    if (m != 2222222222L) ok++;

    // Omit atime, request a fresh distinct mtime.
    struct timespec ts2[2] = {{0, UTIME_OMIT}, {3333333333, 0}};
    // (4) utimensat with UTIME_OMIT is accepted.
    if (utimensat(AT_FDCWD, path, ts2, 0) == 0) ok++;

    struct stat st2;
    /* Same producer, same reason -- check (5) reads this struct. */
    if (fstat(fd, &st2) != 0) {
        fprintf(stderr, "utimensat: second fstat failed; no observation to compare\n");
        close(fd);
        unlink(path);
        rmdir(dir);
        return 1;
    }
    long a2 = st2.st_atim.tv_sec;
    long m2 = st2.st_mtim.tv_sec;
    // (5) Determinized: fields still collapse and the new mtime was overridden.
    if (a2 == m2 && m2 != 3333333333L) ok++;

    close(fd);
    unlink(path);
    rmdir(dir);
    printf("utimensat ok=%d\n", ok);

    /* Route a behavioural failure into the exit status. Without this the guest
       exits 0 whatever `ok` reached, so a determinization that stopped applying
       only lowered the printed number -- and under --verify both runs lower it
       identically, so the comparison still matches and the cell stays green.
       The five checks above are unchanged; this only requires all of them.
       Run standalone (native) it will now exit 1, which is the honest report
       that the determinization it tests was absent. */
    if (ok != EXPECTED_CHECKS_UNDER_HERMIT) {
        fprintf(stderr, "utimensat completed %d of %d checks under Hermit\n", ok,
                EXPECTED_CHECKS_UNDER_HERMIT);
        return 1;
    }
    return 0;
}
