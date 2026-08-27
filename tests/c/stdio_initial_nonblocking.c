/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc != 2) {
        return 64;
    }
    if (strcmp(argv[1], "get") == 0) {
        int flags = fcntl(STDIN_FILENO, F_GETFL);
        if (flags < 0) {
            perror("fcntl(F_GETFL)");
            return 1;
        }
        printf("nonblock=%d\n", (flags & O_NONBLOCK) != 0);
        return 0;
    }
    if (strcmp(argv[1], "set") == 0) {
        /* Deliberately do not call F_GETFL first. */
        if (fcntl(STDIN_FILENO, F_SETFL, O_NONBLOCK) < 0) {
            perror("fcntl(F_SETFL, O_NONBLOCK)");
            return 1;
        }
        char byte = 0;
        ssize_t got = recv(STDIN_FILENO, &byte, 1, 0);
        if (got < 0) {
            perror("recv");
            return 1;
        }
        printf("recv=%zd byte=%d\n", got, (int)byte);
        return 0;
    }
    return 64;
}
