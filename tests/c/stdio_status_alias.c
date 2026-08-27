/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>

int main(void) {
    int stdout_flags = fcntl(STDOUT_FILENO, F_GETFL);
    int stderr_flags = fcntl(STDERR_FILENO, F_GETFL);
    if (stdout_flags < 0 || stderr_flags < 0) {
        perror("fcntl(F_GETFL)");
        return 1;
    }
    if (fcntl(STDOUT_FILENO, F_SETFL, stdout_flags | O_APPEND) < 0) {
        perror("fcntl(F_SETFL, O_APPEND)");
        return 1;
    }
    stderr_flags = fcntl(STDERR_FILENO, F_GETFL);
    if (stderr_flags < 0) {
        perror("fcntl(F_GETFL) after");
        return 1;
    }
    dprintf(
        STDERR_FILENO,
        "stderr_append_after_stdout_set=%d\n",
        (stderr_flags & O_APPEND) != 0);
    return 0;
}
