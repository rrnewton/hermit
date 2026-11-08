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
 * Usage: procfs_sysctl_files [FILE | --write FILE VALUE | --as UID FILE |
 *                              --drop UID FILE VALUE | --inherit FILE]...
 * where UID may also be `nocap` or `nopermit`.
 *
 * For /proc/sys/kernel/hostname and /proc/sys/kernel/domainname it also
 * checks that the file holds uname's node or domain name and a newline, as
 * on Linux, where all three read the caller's UTS namespace. Inode numbers
 * are not printed, only whether fstat agrees with stat. Each FILE is also
 * read a byte at a time, at the advancing position, until end of file, and
 * at position 1 by pread: a sysctl table's handler reads at the position it
 * is given (an integer file is empty past 0, a string one is not).
 *
 * --write FILE VALUE writes VALUE to FILE as the run's root: an open for
 * reading refuses the write (EBADF); then on a write-only descriptor, a
 * write, the position it leaves, a second write there, a pwrite at 0, and a
 * read back through a new descriptor.
 *
 * --as UID FILE, in a child: drops the supplementary groups, then sets
 * every group and user ID to UID (each call's result and the effective IDs
 * after are printed), then reads FILE as a control, and opens it for
 * writing and probes access(W_OK).
 *
 * --drop UID FILE VALUE, in a child: opens FILE for writing as the caller,
 * then sets every ID to UID as --as does, then writes VALUE through the
 * open descriptor (Linux asks the sysctl's permission again at the write,
 * with the IDs then: EPERM) and reads FILE back.
 *
 * With `nocap` for UID, --as and --drop instead remove CAP_NET_ADMIN from
 * the effective set (capset), keeping the IDs: a network namespace's
 * sysctls give their owner's access to that capability in the namespace's
 * owner (net_ctl_permissions), not to an ID. With `nopermit`, from the
 * permitted set too, which access(W_OK) checks for a real user ID of 0
 * (access_override_creds).
 *
 * --inherit FILE opens FILE, reads it once, and executes this program again
 * with the descriptor (--read-fd N), which reads on from where it was,
 * rewinds and reads the whole file. */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <grp.h>
#include <linux/capability.h>
#include <sys/syscall.h>
#include <sys/stat.h>
#include <sys/utsname.h>
#include <sys/wait.h>
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

/* Print `n` bytes of `buffer` with newlines shown as '|', tabs as ' '. */
static void print_bytes(const char *label, const char *buffer, ssize_t n) {
    printf(" %s=\"", label);
    for (ssize_t i = 0; i < n; i++) {
        putchar(buffer[i] == '\n' ? '|' : buffer[i] == '\t' ? ' ' : buffer[i]);
    }
    printf("\"");
}

/* A read of `fd` of up to 255 bytes, printed as `label`. */
static void read_line(const char *label, int fd) {
    char buffer[256];
    ssize_t n = read(fd, buffer, sizeof(buffer) - 1);
    if (n < 0) {
        printf(" %s=%s", label, strerrorname_np(errno));
    } else {
        print_bytes(label, buffer, n);
    }
}

/* FILE read a byte at a time until end of file, and at position 1. */
static void positions_line(const char *path) {
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    printf("%s: positions open=%s", path, result(fd));
    if (fd >= 0) {
        char bytes[64];
        ssize_t total = 0, n = 0;
        while (total < (ssize_t)sizeof(bytes) && (n = read(fd, bytes + total, 1)) == 1) {
            total++;
        }
        print_bytes("bytewise", bytes, total);
        printf(" end=%s pos=%ld", n < 0 ? strerrorname_np(errno) : "eof",
               (long)lseek(fd, 0, SEEK_CUR));
        char buffer[64];
        n = pread(fd, buffer, sizeof(buffer), 1);
        if (n < 0) {
            printf(" pread1=%s", strerrorname_np(errno));
        } else {
            print_bytes("pread1", buffer, n);
        }
        close(fd);
    }
    printf("\n");
}

static void write_line(const char *path, const char *value) {
    size_t length = strlen(value);
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    printf("%s: write", path);
    if (fd >= 0) {
        printf(" readonly-write=%s", write(fd, value, length) < 0 ? strerrorname_np(errno) : "ok");
        close(fd);
    }
    fd = open(path, O_WRONLY | O_CLOEXEC);
    printf(" open=%s", result(fd));
    if (fd >= 0) {
        ssize_t n = write(fd, value, length);
        printf(" write=%ld", n < 0 ? -(long)errno : (long)n);
        printf(" pos=%ld", (long)lseek(fd, 0, SEEK_CUR));
        n = write(fd, value, length);
        printf(" again=%ld", n < 0 ? -(long)errno : (long)n);
        printf(" pos=%ld", (long)lseek(fd, 0, SEEK_CUR));
        n = pwrite(fd, value, length, 0);
        printf(" pwrite0=%ld", n < 0 ? -(long)errno : (long)n);
        printf(" pos=%ld", (long)lseek(fd, 0, SEEK_CUR));
        close(fd);
    }
    fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd >= 0) {
        read_line("readback", fd);
        close(fd);
    }
    printf("\n");
}

/* Remove CAP_NET_ADMIN from the effective set, printing the result and
 * whether the effective set still holds it. */
static void drop_net_admin(int permitted_too) {
    struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
    struct __user_cap_data_struct data[2];
    memset(data, 0, sizeof(data));
    int got = (int)syscall(SYS_capget, &header, data);
    data[CAP_NET_ADMIN / 32].effective &= ~(1u << (CAP_NET_ADMIN % 32));
    if (permitted_too) {
        data[CAP_NET_ADMIN / 32].permitted &= ~(1u << (CAP_NET_ADMIN % 32));
    }
    int set = got < 0 ? -1 : (int)syscall(SYS_capset, &header, data);
    printf(" capset=%s", result(set));
    memset(data, 0, sizeof(data));
    syscall(SYS_capget, &header, data);
    printf(" net_admin=%d/%d", (data[CAP_NET_ADMIN / 32].effective >> (CAP_NET_ADMIN % 32)) & 1,
           (data[CAP_NET_ADMIN / 32].permitted >> (CAP_NET_ADMIN % 32)) & 1);
}

/* Drop the supplementary groups and set every ID to `id`, printing each
 * call's result and the effective IDs after; or, for "nocap", remove
 * CAP_NET_ADMIN from the effective set. */
static void become_spelled(const char *spelled);

static void become(unsigned id) {
    int groups = setgroups(0, NULL);
    printf(" setgroups=%s", result(groups));
    int gid = setresgid(id, id, id);
    printf(" setresgid=%s", result(gid));
    int uid = setresuid(id, id, id);
    printf(" setresuid=%s", result(uid));
    printf(" euid=%u egid=%u", (unsigned)geteuid(), (unsigned)getegid());
}

static void become_spelled(const char *spelled) {
    if (strcmp(spelled, "nocap") == 0 || strcmp(spelled, "nopermit") == 0) {
        drop_net_admin(strcmp(spelled, "nopermit") == 0);
    } else {
        become((unsigned)atoi(spelled));
    }
}

/* Run `body` in a child process, so that its credential change stays
 * there, and wait for it. */
static void in_child(void (*body)(const char **), const char **arguments) {
    fflush(stdout);
    pid_t child = fork();
    if (child == 0) {
        body(arguments);
        fflush(stdout);
        _exit(0);
    }
    int status = 0;
    if (child < 0 || waitpid(child, &status, 0) != child) {
        printf("child=%s\n", strerrorname_np(errno));
    }
}

/* --as UID FILE */
static void as_body(const char **arguments) {
    const char *path = arguments[1];
    printf("%s: as %s", path, arguments[0]);
    become_spelled(arguments[0]);
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    printf(" open(R)=%s", result(fd));
    if (fd >= 0) {
        read_line("read", fd);
        close(fd);
    }
    fd = open(path, O_WRONLY | O_CLOEXEC);
    printf(" open(W)=%s", result(fd));
    if (fd >= 0) {
        close(fd);
    }
    printf(" access(R)=%s access(W)=%s\n", result(access(path, R_OK)),
           result(access(path, W_OK)));
}

/* --drop UID FILE VALUE */
static void drop_body(const char **arguments) {
    const char *path = arguments[1], *value = arguments[2];
    size_t length = strlen(value);
    printf("%s: drop %s", path, arguments[0]);
    int fd = open(path, O_WRONLY | O_CLOEXEC);
    printf(" open(W)=%s", result(fd));
    become_spelled(arguments[0]);
    if (fd >= 0) {
        ssize_t n = write(fd, value, length);
        printf(" write=%s", n < 0 ? strerrorname_np(errno) : "ok");
        n = pwrite(fd, value, length, 0);
        printf(" pwrite0=%s", n < 0 ? strerrorname_np(errno) : "ok");
        close(fd);
    }
    fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd >= 0) {
        read_line("readback", fd);
        close(fd);
    }
    printf("\n");
}

/* Open FILE, read it once, and execute this program on the descriptor. */
static int inherit(const char *self, const char *path) {
    int fd = open(path, O_RDONLY);
    printf("%s: inherit open=%s", path, result(fd));
    if (fd < 0) {
        printf("\n");
        return 1;
    }
    char buffer[2];
    ssize_t n = read(fd, buffer, 1);
    printf(" first=%ld\n", (long)n);
    char number[16];
    snprintf(number, sizeof(number), "%d", fd);
    execl(self, self, "--read-fd", number, (char *)NULL);
    printf("exec=%s\n", strerrorname_np(errno));
    return 1;
}

/* The executed image's reads of the inherited descriptor `fd`. */
static int read_inherited(int fd) {
    printf("inherited: pos=%ld", (long)lseek(fd, 0, SEEK_CUR));
    read_line("rest", fd);
    printf(" rewind=%ld", (long)lseek(fd, 0, SEEK_SET));
    read_line("whole", fd);
    printf("\nRESULT failures=0\n");
    return 0;
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr,
                "usage: %s [FILE | --write FILE VALUE | --as UID|nocap|nopermit FILE | "
                "--drop UID|nocap|nopermit FILE VALUE | --inherit FILE]...\n",
                argv[0]);
        return 2;
    }
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (argc == 3 && strcmp(argv[1], "--read-fd") == 0) {
        return read_inherited(atoi(argv[2]));
    }
    struct utsname name;
    if (uname(&name) < 0) {
        perror("uname");
        return 1;
    }
    int failures = 0;
    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--write") == 0 && i + 2 < argc) {
            write_line(argv[i + 1], argv[i + 2]);
            i += 2;
            continue;
        }
        if (strcmp(argv[i], "--as") == 0 && i + 2 < argc) {
            in_child(as_body, (const char **)argv + i + 1);
            i += 2;
            continue;
        }
        if (strcmp(argv[i], "--drop") == 0 && i + 3 < argc) {
            in_child(drop_body, (const char **)argv + i + 1);
            i += 3;
            continue;
        }
        if (strcmp(argv[i], "--inherit") == 0 && i + 1 < argc) {
            return failures ? 1 : inherit(argv[0], argv[i + 1]);
        }
        file_line(argv[i]);
        positions_line(argv[i]);
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
