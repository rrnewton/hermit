/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* readlink and readlinkat of the system-wide procfs directories by many
 * spellings, each answer printed as an errno name or the link's length and
 * bytes, so that native Linux and a Hermit backend can be compared line by
 * line.
 *
 *   procfs_system_directory_links DIR   spellings; DIR is a writable
 *                                       directory for symbolic links to
 *                                       /proc/sys, one direct and one at
 *                                       the end of a chain of 39
 *   procfs_system_directory_links --bind
 *                                       the two names a bind over /proc/sys
 *                                       holding kernel -> /dev/null changes
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static void show(const char *what, ssize_t length, const char *buffer) {
    if (length < 0) {
        printf(" %s=%s", what, strerrorname_np(errno));
    } else {
        printf(" %s=%zd:%.*s", what, length, (int)length, buffer);
    }
}

static void both(const char *label, const char *path) {
    char buffer[256];
    printf("%s:", label);
    show("readlink", readlink(path, buffer, sizeof(buffer)), buffer);
    show("readlinkat", readlinkat(AT_FDCWD, path, buffer, sizeof(buffer)), buffer);
    printf("\n");
}

static void relative(const char *label, int dirfd, const char *path) {
    char buffer[256];
    printf("%s:", label);
    show("readlinkat", readlinkat(dirfd, path, buffer, sizeof(buffer)), buffer);
    printf("\n");
}

/* readlink (at false) or readlinkat (at true) of `path`, asked again while
 * it fails with ELOOP, up to 8 times. Linux keeps a lookup's link count
 * across the restart of its RCU walk (set_nameidata once, fs/namei.c), and
 * a mount or rename anywhere on the host can force that restart, so a lookup
 * near MAXSYMLINKS can fail with ELOOP when it is not past it, though never
 * succeed when it is. Natively, under concurrent mounts, 5 of 200 runs gave
 * one such ELOOP; this keeps the answer the walk's own count. */
static ssize_t settled(bool at, const char *path, char *buffer, size_t size) {
    ssize_t length = -1;
    for (int attempt = 0; attempt < 8; attempt++) {
        length = at ? readlinkat(AT_FDCWD, path, buffer, size)
                    : readlink(path, buffer, size);
        if (length >= 0 || errno != ELOOP) {
            break;
        }
    }
    return length;
}

/* `prefix`, then `real` (a path with no symbolic link in it), then
 * "/chainN/": the final "/" makes the lookup follow the last link too. */
static void chained(const char *label, const char *prefix, const char *real, int first) {
    char path[2 * PATH_MAX];
    char buffer[256];
    snprintf(path, sizeof(path), "%s%s/chain%d/", prefix, real, first);
    printf("%s:", label);
    show("readlink", settled(false, path, buffer, sizeof(buffer)), buffer);
    show("readlinkat", settled(true, path, buffer, sizeof(buffer)), buffer);
    printf("\n");
}

/* "/proc/", then `slashes` slashes, then "sys"; or "/proc/sys" padded with
 * "/." to `length` bytes. */
static char *spelling(size_t slashes, size_t length) {
    char *path = calloc(1, slashes + length + 16);
    if (path == NULL) {
        exit(2);
    }
    if (length == 0) {
        strcpy(path, "/proc/");
        memset(path + 6, '/', slashes);
        strcpy(path + 6 + slashes, "sys");
    } else {
        strcpy(path, "/proc/sys");
        while (strlen(path) + 2 <= length) {
            strcat(path, "/.");
        }
    }
    return path;
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: %s DIR | --bind\n", argv[0]);
        return 2;
    }
    if (strcmp(argv[1], "--bind") == 0) {
        both("/proc/sys/kernel", "/proc/sys/kernel");
        both("/proc/sys", "/proc/sys");
        return 0;
    }
    both("/proc/sys", "/proc/sys");
    both("/proc/sys/", "/proc/sys/");
    both("//proc/sys", "//proc/sys");
    both("/proc/./sys", "/proc/./sys");
    both("/proc/sys/kernel/random", "/proc/sys/kernel/random");
    both("/proc/sys/kernel/random/uuid", "/proc/sys/kernel/random/uuid");
    both("/proc/sys/hermit-absent", "/proc/sys/hermit-absent");
    char *overlong = spelling(4096, 0);
    printf("(%zu bytes) ", strlen(overlong));
    both("/proc/ + slashes + sys", overlong);
    char *longest = spelling(0, 4095);
    printf("(%zu bytes) ", strlen(longest));
    both("/proc/sys + /. padding", longest);
    int proc = open("/proc", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (proc < 0) {
        perror("open /proc");
        return 1;
    }
    relative("sys from a /proc descriptor", proc, "sys");
    relative("sys/kernel from a /proc descriptor", proc, "sys/kernel");
    char link[4096];
    snprintf(link, sizeof(link), "%s/to-sys", argv[1]);
    if (symlink("/proc/sys", link) != 0) {
        perror("symlink");
        return 1;
    }
    both("DIR/to-sys", link);
    strcat(link, "/kernel");
    both("DIR/to-sys/kernel", link);

    /* One MAXSYMLINKS (40) budget covers the whole lookup. DIR/chain0 ->
     * chain1 -> ... -> chain38 -> /proc/sys is 39 links, chainN/ is 39 - N;
     * /proc/self/fd/N spends 2 (self and the descriptor's link) and each
     * /proc/self/../.. 1 (self). Up to 40 links reach /proc/sys, no link:
     * EINVAL. 41 is ELOOP (fs/namei.c, reserve_stack). */
    char real[PATH_MAX];
    if (realpath(argv[1], real) == NULL) {
        perror("realpath");
        return 1;
    }
    for (int index = 0; index <= 38; index++) {
        char name[2 * PATH_MAX];
        char target[32];
        snprintf(name, sizeof(name), "%s/chain%d", real, index);
        if (index == 38) {
            strcpy(target, "/proc/sys");
        } else {
            snprintf(target, sizeof(target), "chain%d", index + 1);
        }
        if (symlink(target, name) != 0) {
            perror("symlink chain");
            return 1;
        }
    }
    char through[64];
    snprintf(through, sizeof(through), "/proc/self/fd/%d/..", proc);
    char through_self[128];
    snprintf(through_self, sizeof(through_self), "%s/proc/self/../..", through);
    chained("chain, 39 links", "", real, 0);
    chained("/proc/self/fd/N/.. + chain, 39 links", through, real, 2);
    chained("/proc/self/fd/N/.. + chain, 40 links", through, real, 1);
    chained("/proc/self/fd/N/.. + chain, 41 links", through, real, 0);
    chained("/proc/self/../.. + chain, 40 links", "/proc/self/../..", real, 0);
    chained("/proc/self/../.. x2 + chain, 41 links",
            "/proc/self/../../proc/self/../..", real, 0);
    chained("/proc/self/fd/N/../proc/self/../.. + chain, 40 links", through_self, real, 2);
    chained("/proc/self/fd/N/../proc/self/../.. + chain, 41 links", through_self, real, 1);
    free(overlong);
    free(longest);
    close(proc);
    return 0;
}
