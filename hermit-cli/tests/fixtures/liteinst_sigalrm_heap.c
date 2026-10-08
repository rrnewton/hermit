/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * With guest SIGALRM handlers admitted, the in-guest runtime does more before
 * main (it records which file is the C library) and more while the program
 * runs (it checks the handler's restorer and delivers the signal). None of it
 * may allocate through this process's C library heap. This program allocates
 * nothing itself: it reports the heap at main's entry, installs a SIGALRM
 * handler, takes one SIGALRM, and reports the heap again, using write() and
 * stack buffers only.
 */

#include <errno.h>
#include <malloc.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static volatile sig_atomic_t runs;

static void handler(int signal) {
    (void)signal;
    runs++;
}

static void say(const char *what) {
    struct mallinfo2 heap = mallinfo2();
    char line[160];
    int length = snprintf(
        line, sizeof line, "%s: arena=%zu in_use=%zu\n", what, heap.arena, heap.uordblks);
    if (write(1, line, (size_t)length) != length) {
        _exit(2);
    }
}

int main(void) {
    say("heap at main");
    struct sigaction action;
    memset(&action, 0, sizeof action);
    action.sa_handler = handler;
    int installed = sigaction(SIGALRM, &action, NULL);
    alarm(1);
    int paused = pause();
    int pause_errno = errno;
    char line[96];
    int length = snprintf(
        line, sizeof line, "install=%d pause=%d errno=%d runs=%d\n", installed, paused,
        pause_errno, (int)runs);
    if (write(1, line, (size_t)length) != length) {
        return 2;
    }
    say("heap after SIGALRM");
    return 0;
}
