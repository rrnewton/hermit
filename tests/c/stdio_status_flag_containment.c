/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Reads stdout's file status flags, sets O_APPEND, reads them back, and writes
 * through stdout. The caller places the shared file offset before EOF, so the
 * final write proves the flag has behavior rather than being reflected only by
 * F_GETFL.
 *
 * This separates the two halves of a containment claim that are easy to
 * conflate. `fcntl(F_SETFL)` mutates the open file DESCRIPTION, which Hermit
 * may be sharing with the process that invoked it, so a fix that simply
 * dropped the guest's request would make the supervisor's descriptor look
 * correct while silently breaking the guest. The printed line is the guest's
 * OWN view; the test that runs this program separately checks the supervisor's
 * descriptor. Both must hold: the guest sees its change, and nobody else does.
 *
 * Only the O_APPEND bit is printed, not the raw flag word, because the rest of
 * the word legitimately depends on what the caller redirected stderr to.
 */

#include <fcntl.h>
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv) {
    int before = fcntl(STDOUT_FILENO, F_GETFL);
    if (before < 0) {
        perror("fcntl(F_GETFL) before");
        return 1;
    }
    if (argc == 2 && strcmp(argv[1], "clear") == 0) {
        errno = 0;
        int clear_result = fcntl(STDOUT_FILENO, F_SETFL, before & ~O_APPEND);
        int clear_errno = errno;
        int after_clear = fcntl(STDOUT_FILENO, F_GETFL);
        dprintf(
            STDERR_FILENO,
            "clear_result=%d clear_errno=%d append_after=%d\n",
            clear_result,
            clear_errno,
            after_clear >= 0 && (after_clear & O_APPEND) != 0);
        return before & O_APPEND && clear_result == -1 && clear_errno == EOPNOTSUPP &&
                after_clear & O_APPEND
            ? 0
            : 1;
    }
    if (argc != 1) {
        return 64;
    }
    if (fcntl(STDOUT_FILENO, F_SETFL, before | O_APPEND) < 0) {
        perror("fcntl(F_SETFL, O_APPEND)");
        return 1;
    }
    int after = fcntl(STDOUT_FILENO, F_GETFL);
    if (after < 0) {
        perror("fcntl(F_GETFL) after");
        return 1;
    }
    dprintf(
        STDERR_FILENO,
        "append_before=%d append_after=%d\n",
        (before & O_APPEND) != 0,
        (after & O_APPEND) != 0);
    if (write(STDOUT_FILENO, "guest\n", 6) != 6) {
        perror("write(stdout)");
        return 1;
    }
    return 0;
}
