/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Exercise every write path whose Linux behavior depends on O_APPEND.
 *
 * stdout is an inherited regular file opened without O_APPEND and positioned
 * before EOF. The guest enables O_APPEND logically, then invokes one syscall:
 *
 *   sendfile              must fail with EINVAL and move neither offset;
 *   pwrite/pwritev         must append despite the explicit zero offset and
 *                          must not move stdout's current offset;
 *   pwritev2(offset=0)     has the same Linux O_APPEND behavior;
 *   pwritev2(offset=-1)    must append and advance the current offset.
 *
 * The Rust caller checks the bytes in the shared file and that the supervisor's
 * status flags did not change. This program checks the guest-visible return,
 * errno, logical flag, explicit input offset, and current output offset.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/sendfile.h>
#include <sys/uio.h>
#include <unistd.h>

static int sendfile_case(void) {
    char path[] = "/tmp/hermit-stdio-append-sendfile-XXXXXX";
    int input = mkstemp(path);
    if (input < 0 || unlink(path) != 0 || write(input, "guest\n", 6) != 6 ||
        lseek(input, 0, SEEK_SET) != 0) {
        perror("prepare sendfile input");
        return 1;
    }

    off_t offset = 0;
    errno = 0;
    ssize_t result = sendfile(STDOUT_FILENO, input, &offset, 6);
    int saved_errno = errno;
    close(input);
    dprintf(STDERR_FILENO,
            "sendfile result=%zd errno=%d input_offset=%lld\n", result,
            saved_errno, (long long)offset);
    return result == -1 && saved_errno == EINVAL && offset == 0 ? 0 : 1;
}

static int positioned_case(const char *mode) {
    static char payload[] = "guest\n";
    struct iovec iov = {.iov_base = payload, .iov_len = 6};
    off_t before = lseek(STDOUT_FILENO, 0, SEEK_CUR);
    errno = 0;
    ssize_t result;
    off_t expected_after = before;

    if (strcmp(mode, "pwrite") == 0) {
        result = pwrite(STDOUT_FILENO, payload, 6, 0);
    } else if (strcmp(mode, "pwritev") == 0) {
        result = pwritev(STDOUT_FILENO, &iov, 1, 0);
    } else if (strcmp(mode, "pwritev2") == 0) {
        result = pwritev2(STDOUT_FILENO, &iov, 1, 0, 0);
    } else if (strcmp(mode, "pwritev2-current") == 0) {
        result = pwritev2(STDOUT_FILENO, &iov, 1, -1, 0);
        expected_after = 13;
    } else {
        return 64;
    }

    int saved_errno = errno;
    off_t after = lseek(STDOUT_FILENO, 0, SEEK_CUR);
    dprintf(STDERR_FILENO,
            "%s result=%zd errno=%d position_before=%lld position_after=%lld\n",
            mode, result, saved_errno, (long long)before, (long long)after);
    return result == 6 && saved_errno == 0 && before == 0 &&
            after == expected_after
        ? 0
        : 1;
}

int main(int argc, char **argv) {
    if (argc != 2) {
        return 64;
    }
    int before = fcntl(STDOUT_FILENO, F_GETFL);
    if (before < 0 || fcntl(STDOUT_FILENO, F_SETFL, before | O_APPEND) != 0) {
        perror("fcntl(O_APPEND)");
        return 1;
    }
    int after = fcntl(STDOUT_FILENO, F_GETFL);
    if (after < 0 || (after & O_APPEND) == 0) {
        fputs("O_APPEND not visible to guest\n", stderr);
        return 1;
    }

    if (strcmp(argv[1], "sendfile") == 0) {
        return sendfile_case();
    }
    return positioned_case(argv[1]);
}
