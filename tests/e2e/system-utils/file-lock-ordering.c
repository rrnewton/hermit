/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Guest-observable file-lock semantics and xattr enumeration order.
 *
 * WHAT THIS PINS
 *   1. flock(2) mutual exclusion between processes -- REPORTED, not asserted.
 *      Under Hermit today it is NOT enforced (handle_flock is an unconditional
 *      no-op success), so this line prints NOT_ENFORCED. Native prints ENFORCED.
 *      The observed word is emitted so the day flock starts working the output
 *      changes and someone must look, rather than the gap staying invisible.
 *   2. fcntl POSIX record locks -- ASSERTED. Exclusion must hold across
 *      processes.
 *   3. POSIX vs OFD inheritance across fork -- ASSERTED, and they must DIFFER.
 *      POSIX locks are owned by (process, inode) and are not inherited, so a
 *      child conflicts with its parent's lock. OFD locks are owned by the open
 *      file description, which fork shares, so the child using the inherited
 *      descriptor is the same owner and does not conflict. Getting these the
 *      same way round is the subtle error a backend makes.
 *   4. listxattr enumeration ORDER -- emitted verbatim, not sorted or counted,
 *      because sorting would hide exactly the reordering this is here to catch.
 *
 * WHY THERE IS NO ACQUISITION-ORDER TEST HERE
 *   Acquisition order under contention is the interesting property, but it is
 *   UNMEASURABLE while flock is a no-op: contention cannot be created, so any
 *   "stable order" would be an artifact of never having contended. Measured:
 *   natively four contending children give a MOVING winner; under Hermit no
 *   child is ever denied. That leg is blocked on the flock fix and is
 *   deliberately absent rather than present and vacuous.
 *
 * NO SLEEPS anywhere: a sleep would decide the outcome instead of observing it.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <sys/xattr.h>
#include <unistd.h>

static int failures = 0;

static void check(int ok, const char *what) {
    if (!ok) {
        printf("FAIL %s\n", what);
        failures++;
    }
}

static const char *tmp_dir(void) {
    const char *t = getenv("TMPDIR");
    return (t && *t) ? t : ".";
}

/* Does a second process see an exclusive lock this process holds? */
static const char *exclusion(const char *path, int use_flock, int ofd) {
    int fd = open(path, O_RDWR);
    if (fd < 0) {
        return "OPEN_FAILED";
    }
    struct flock fl;
    memset(&fl, 0, sizeof fl);
    fl.l_type = F_WRLCK;
    fl.l_whence = SEEK_SET;
    fl.l_len = 1;
    int held = use_flock ? flock(fd, LOCK_EX)
                         : fcntl(fd, ofd ? F_OFD_SETLK : F_SETLK, &fl);
    if (held != 0) {
        close(fd);
        return "HOLDER_FAILED";
    }
    pid_t pid = fork();
    if (pid == 0) {
        /* A FRESH descriptor: a different open file description, so this is a
         * genuine second owner under both lock families. */
        int fd2 = open(path, O_RDWR);
        int rc;
        if (use_flock) {
            rc = flock(fd2, LOCK_EX | LOCK_NB);
        } else {
            struct flock c;
            memset(&c, 0, sizeof c);
            c.l_type = F_WRLCK;
            c.l_whence = SEEK_SET;
            c.l_len = 1;
            rc = fcntl(fd2, ofd ? F_OFD_SETLK : F_SETLK, &c);
        }
        _exit(rc == 0 ? 0 : 1);
    }
    int status = 0;
    waitpid(pid, &status, 0);
    if (use_flock) {
        flock(fd, LOCK_UN);
    }
    close(fd);
    if (!WIFEXITED(status)) {
        return "CHILD_SIGNALLED";
    }
    return WEXITSTATUS(status) == 0 ? "NOT_ENFORCED" : "ENFORCED";
}

/* Does a forked child inherit the parent's lock on the INHERITED descriptor? */
static const char *inheritance(const char *path, int ofd) {
    int fd = open(path, O_RDWR);
    if (fd < 0) {
        return "OPEN_FAILED";
    }
    struct flock fl;
    memset(&fl, 0, sizeof fl);
    fl.l_type = F_WRLCK;
    fl.l_whence = SEEK_SET;
    fl.l_len = 1;
    if (fcntl(fd, ofd ? F_OFD_SETLK : F_SETLK, &fl) != 0) {
        close(fd);
        return "HOLDER_FAILED";
    }
    pid_t pid = fork();
    if (pid == 0) {
        struct flock c;
        memset(&c, 0, sizeof c);
        c.l_type = F_WRLCK;
        c.l_whence = SEEK_SET;
        c.l_len = 1;
        int rc = fcntl(fd, ofd ? F_OFD_SETLK : F_SETLK, &c);
        _exit(rc == 0 ? 0 : 1);
    }
    int status = 0;
    waitpid(pid, &status, 0);
    close(fd);
    if (!WIFEXITED(status)) {
        return "CHILD_SIGNALLED";
    }
    return WEXITSTATUS(status) == 0 ? "child-acquired" : "child-conflicted";
}

int main(void) {
    char dir[512];
    snprintf(dir, sizeof dir, "%s/lockordXXXXXX", tmp_dir());
    if (!mkdtemp(dir)) {
        printf("FATAL mkdtemp errno=%d\n", errno);
        return 1;
    }
    char lock_path[600], xattr_path[600];
    snprintf(lock_path, sizeof lock_path, "%s/lock", dir);
    snprintf(xattr_path, sizeof xattr_path, "%s/xattr", dir);
    for (int i = 0; i < 2; i++) {
        int fd = open(i ? xattr_path : lock_path, O_CREAT | O_RDWR, 0600);
        if (fd < 0) {
            printf("FATAL create errno=%d\n", errno);
            return 1;
        }
        close(fd);
    }

    /* Emit the OBSERVED word for each probe, never a bare ok/fail count. */
    const char *flock_excl = exclusion(lock_path, 1, 0);
    const char *posix_excl = exclusion(lock_path, 0, 0);
    const char *ofd_excl = exclusion(lock_path, 0, 1);
    const char *posix_inh = inheritance(lock_path, 0);
    const char *ofd_inh = inheritance(lock_path, 1);

    printf("flock exclusion: %s\n", flock_excl);
    printf("posix exclusion: %s\n", posix_excl);
    printf("ofd exclusion: %s\n", ofd_excl);
    printf("posix inheritance: %s\n", posix_inh);
    printf("ofd inheritance: %s\n", ofd_inh);

    /* fcntl exclusion is a real Linux contract and is asserted. flock is only
     * reported: see the header. */
    check(strcmp(posix_excl, "ENFORCED") == 0, "posix record locks must exclude a second process");
    check(strcmp(ofd_excl, "ENFORCED") == 0, "OFD locks must exclude a second open file description");
    /* The two families MUST disagree here; that difference is the contract. */
    check(strcmp(posix_inh, "child-conflicted") == 0, "POSIX locks are not inherited by a child");
    check(strcmp(ofd_inh, "child-acquired") == 0, "OFD locks are shared with a forked child");
    check(strcmp(posix_inh, ofd_inh) != 0, "POSIX and OFD inheritance must differ");

    static const char *names[] = {"user.zeta", "user.alpha", "user.mike", "user.bravo"};
    int wrote = 0;
    for (size_t i = 0; i < sizeof names / sizeof *names; i++) {
        if (setxattr(xattr_path, names[i], "v", 1, 0) == 0) {
            wrote++;
        }
    }
    if (wrote == 0) {
        printf("xattr UNSUPPORTED_FS\n");
    } else {
        char buf[512];
        ssize_t n = listxattr(xattr_path, buf, sizeof buf);
        if (n < 0) {
            printf("xattr LIST_FAILED errno=%d\n", errno);
            failures++;
        } else {
            printf("xattr order:");
            int seen = 0;
            for (ssize_t off = 0; off < n; off += (ssize_t)strlen(buf + off) + 1) {
                printf(" %s", buf + off);
                seen++;
            }
            printf("\n");
            /* NON-VACUITY: enumerating nothing is not a passing enumeration. */
            check(seen == wrote, "listxattr must enumerate every attribute that was set");
        }
    }

    /* Leave nothing behind: the harness may run this from a shared directory,
     * and a guest that litters perturbs whatever runs next. */
    unlink(lock_path);
    unlink(xattr_path);
    rmdir(dir);

    printf("checks-failed: %d\n", failures);
    return failures == 0 ? 0 : 1;
}
