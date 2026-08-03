/*
 * File-descriptor-flag parity fixture (fcntl FD_CLOEXEC / F_DUPFD family).
 *
 * Exercises the per-descriptor flag namespace (F_GETFD / F_SETFD), which is
 * distinct from the open-file status flags (F_GETFL / F_SETFL). It confirms a
 * fresh descriptor starts with FD_CLOEXEC clear, that the flag can be set and
 * cleared and read straight back, and that F_DUPFD_CLOEXEC vs plain F_DUPFD
 * duplicate the descriptor with and without FD_CLOEXEC respectively. Every
 * observable is a per-descriptor flag bit read back from the kernel, which
 * carries no host-derived, timing, or cross-thread input, so it is identical
 * across ptrace, DBI, and KVM.
 */
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

int main(void) {
    char path[] = "/tmp/fdcloexecXXXXXX";
    int fd = mkstemp(path);
    int ok = 0;

    if (fd < 0) {
        printf("fdflags ok=-1 mkstemp\n");
        return 0;
    }

    /* check 1: a fresh mkstemp descriptor has FD_CLOEXEC clear. */
    int flags = fcntl(fd, F_GETFD);
    if (flags >= 0 && (flags & FD_CLOEXEC) == 0) {
        ok++;
    }

    /* check 2: set FD_CLOEXEC and read it back set. */
    if (fcntl(fd, F_SETFD, FD_CLOEXEC) == 0) {
        flags = fcntl(fd, F_GETFD);
        if (flags >= 0 && (flags & FD_CLOEXEC)) {
            ok++;
        }
    }

    /* check 3: clear FD_CLOEXEC and read it back clear. */
    if (fcntl(fd, F_SETFD, 0) == 0) {
        flags = fcntl(fd, F_GETFD);
        if (flags >= 0 && (flags & FD_CLOEXEC) == 0) {
            ok++;
        }
    }

    /* check 4: F_DUPFD_CLOEXEC duplicates with FD_CLOEXEC set. */
    int dup_cloexec = fcntl(fd, F_DUPFD_CLOEXEC, 100);
    if (dup_cloexec >= 100) {
        flags = fcntl(dup_cloexec, F_GETFD);
        if (flags >= 0 && (flags & FD_CLOEXEC)) {
            ok++;
        }
        close(dup_cloexec);
    }

    /* check 5: plain F_DUPFD duplicates without FD_CLOEXEC. */
    int dup_plain = fcntl(fd, F_DUPFD, 200);
    if (dup_plain >= 200) {
        flags = fcntl(dup_plain, F_GETFD);
        if (flags >= 0 && (flags & FD_CLOEXEC) == 0) {
            ok++;
        }
        close(dup_plain);
    }

    close(fd);
    unlink(path);
    printf("fdflags ok=%d\n", ok);
    return 0;
}
