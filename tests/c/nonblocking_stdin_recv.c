/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Sets O_NONBLOCK on the container's stdin and then performs a socket-family
 * read on it.
 *
 * This pairing is the one that breaks if Detcore's guest-visible O_NONBLOCK and
 * its own physical view of the descriptor are allowed to disagree.
 * `ioaction_based_on_fd_status` panics on `virt && !phys` -- "we cannot simulate
 * nonblocking behavior when set to blocking mode in the kernel" -- so a change
 * that models the flag without asking the kernel makes the next socket syscall
 * on that descriptor abort the container.
 *
 * It has to be `recv` and not `read`. `setup_stdio` types the standard streams
 * `FdType::Regular`, and `handle_read` only routes Socket/Pipe/notification
 * descriptors through the nonblockable path, so `read(0, ...)` never classifies
 * and never reaches the invariant. The socket handlers in `syscalls/io.rs` call
 * it unconditionally by syscall kind, with no fd-type guard. A regression test
 * built on `read` would pass while the defect was live.
 *
 * The caller supplies a socket as stdin with a byte already readable, so this
 * is not a question about blocking: the recv can be satisfied immediately, and
 * anything other than one byte means the classification path went wrong.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

int main(void) {
    int flags = fcntl(STDIN_FILENO, F_GETFL);
    if (flags < 0) {
        perror("fcntl(F_GETFL)");
        return 1;
    }
    if (fcntl(STDIN_FILENO, F_SETFL, flags | O_NONBLOCK) < 0) {
        perror("fcntl(F_SETFL, O_NONBLOCK)");
        return 1;
    }
    char byte = 0;
    ssize_t got = recv(STDIN_FILENO, &byte, 1, 0);
    if (got < 0) {
        printf("recv=-1 errno=%s\n", strerror(errno));
        return 1;
    }
    printf("recv=%zd byte=%d\n", got, (int)byte);
    return 0;
}
