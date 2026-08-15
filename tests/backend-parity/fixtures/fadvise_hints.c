/*
 * File-advice parity fixture (posix_fadvise / fadvise64).
 *
 * The file-advice analog of the memory_advice (madvise) row: it issues the five
 * standard POSIX_FADV_* hints against a temporary file and requires each to be
 * deterministically accepted. posix_fadvise returns 0 on success or a positive
 * errno directly (it does not set errno). Advice is a pure hint with no
 * observable side effect on file contents, so acceptance must be identical
 * across ptrace, DBT, and KVM. The fixture deliberately avoids the
 * invalid-advice edge case, whose refusal semantics are a backend modeling
 * choice rather than a cross-backend contract.
 */
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

/* Number of behavioural checks this fixture must complete; a lower count is a
   failure, not a smaller success. */
#define EXPECTED_CHECKS 5

int main(void) {
    char path[] = "/tmp/fadviseXXXXXX";
    int fd = mkstemp(path);
    int ok = 0;

    if (fd < 0) {
        /* Setup failed, so no check ran. Exiting 0 here would report success
           for a run that observed nothing. */
        printf("fadvise ok=-1 mkstemp\n");
        return 1;
    }

    char buf[8192];
    memset(buf, 'A', sizeof buf);
    if (write(fd, buf, sizeof buf) != (ssize_t)sizeof buf) {
        printf("fadvise ok=-1 write\n");
        close(fd);
        unlink(path);
        return 1;
    }

    /* check 1: NORMAL over the whole file. */
    if (posix_fadvise(fd, 0, 0, POSIX_FADV_NORMAL) == 0) {
        ok++;
    }
    /* check 2: SEQUENTIAL over the whole file. */
    if (posix_fadvise(fd, 0, 0, POSIX_FADV_SEQUENTIAL) == 0) {
        ok++;
    }
    /* check 3: WILLNEED over a bounded range. */
    if (posix_fadvise(fd, 0, 4096, POSIX_FADV_WILLNEED) == 0) {
        ok++;
    }
    /* check 4: DONTNEED over a bounded range. */
    if (posix_fadvise(fd, 0, 4096, POSIX_FADV_DONTNEED) == 0) {
        ok++;
    }
    /* check 5: RANDOM over the whole file. */
    if (posix_fadvise(fd, 0, 0, POSIX_FADV_RANDOM) == 0) {
        ok++;
    }

    close(fd);
    unlink(path);
    printf("fadvise ok=%d\n", ok);

    /* Route a behavioural failure into the exit status. Without this the guest
       exits 0 whatever `ok` reached, so a hint that stopped being accepted only
       lowered the printed number -- and under --verify both runs lower it
       identically, so the comparison still matches and the cell stays green.
       The five checks above are unchanged; this only requires all of them. */
    if (ok != EXPECTED_CHECKS) {
        fprintf(stderr, "fadvise completed %d of %d checks\n", ok,
                EXPECTED_CHECKS);
        return 1;
    }
    return 0;
}
