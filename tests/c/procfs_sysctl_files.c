/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* stat, lstat, open and read, fstat and access of /proc/sys files, one line
 * per file, which the KVM cli test requires to equal ptrace's
 * (run_kvm_procfs_sysctl_files_answer_as_on_ptrace).
 *
 * Usage: procfs_sysctl_files FILE...
 *
 * For /proc/sys/kernel/hostname and /proc/sys/kernel/domainname it also
 * checks that the file holds uname's node or domain name and a newline, as
 * on Linux, where all three read the caller's UTS namespace. Inode numbers
 * are not printed, only whether fstat agrees with stat. */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/utsname.h>
#include <unistd.h>

static const char *result(int rc) {
    return rc < 0 ? strerrorname_np(errno) : "ok";
}

/* The file's contents from one read at its start, or NULL. */
static const char *contents(const char *path, char *buffer, size_t size) {
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        return NULL;
    }
    ssize_t n = read(fd, buffer, size - 1);
    close(fd);
    if (n < 0) {
        return NULL;
    }
    buffer[n] = '\0';
    return buffer;
}

static void file_line(const char *path) {
    struct stat st, lst, fst;
    int s = stat(path, &st);
    printf("%s: stat=%s", path, result(s));
    if (s == 0) {
        printf("(mode=%o nlink=%lu uid=%u gid=%u size=%ld)", st.st_mode,
               (unsigned long)st.st_nlink, st.st_uid, st.st_gid, (long)st.st_size);
    }
    int l = lstat(path, &lst);
    printf(" lstat=%s", l < 0 ? result(l)
                              : s == 0 && lst.st_ino == st.st_ino && lst.st_mode == st.st_mode
                                    ? "same"
                                    : "differs");
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    printf(" open=%s", result(fd));
    if (fd >= 0) {
        char buffer[256];
        ssize_t n = read(fd, buffer, sizeof(buffer) - 1);
        if (n < 0) {
            printf(" read=%s", strerrorname_np(errno));
        } else {
            buffer[n] = '\0';
            for (char *c = buffer; *c; c++) {
                if (*c == '\n') {
                    *c = '|';
                } else if (*c == '\t') {
                    *c = ' ';
                }
            }
            printf(" read=\"%s\"", buffer);
        }
        int f = fstat(fd, &fst);
        printf(" fstat=%s", f < 0 ? result(f)
                                  : s == 0 && fst.st_ino == st.st_ino &&
                                            fst.st_dev == st.st_dev && fst.st_mode == st.st_mode
                                        ? "same"
                                        : "differs");
        close(fd);
    }
    printf(" access(R)=%s", result(access(path, R_OK)));
    printf(" access(W)=%s\n", result(access(path, W_OK)));
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: %s FILE...\n", argv[0]);
        return 2;
    }
    setvbuf(stdout, NULL, _IOLBF, 0);
    struct utsname name;
    if (uname(&name) < 0) {
        perror("uname");
        return 1;
    }
    int failures = 0;
    for (int i = 1; i < argc; i++) {
        file_line(argv[i]);
        const char *expected = strcmp(argv[i], "/proc/sys/kernel/hostname") == 0 ? name.nodename
                               : strcmp(argv[i], "/proc/sys/kernel/domainname") == 0
                                   ? name.domainname
                                   : NULL;
        if (expected) {
            char buffer[256], wanted[sizeof(name.nodename) + 1];
            snprintf(wanted, sizeof(wanted), "%s\n", expected);
            const char *text = contents(argv[i], buffer, sizeof(buffer));
            int ok = text && strcmp(text, wanted) == 0;
            failures += !ok;
            printf("CHECK %s is uname's %d\n", argv[i], ok);
        }
    }
    printf("RESULT failures=%d\n", failures);
    return failures ? 1 : 0;
}
