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
 * stack buffers only. It never waits: with SIGALRM blocked it arms a 1 us
 * timer, makes 200 one-byte writes to stderr (scheduler turns, during which
 * the expiry commits under Hermit), and unblocks SIGALRM, which delivers the
 * pending signal before sigprocmask returns. No signal can be missed and
 * nothing can hang.
 */

#include <errno.h>
#include <malloc.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/time.h>
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
    int install_errno = installed == 0 ? 0 : errno;
    sigset_t alarm_only;
    sigemptyset(&alarm_only);
    sigaddset(&alarm_only, SIGALRM);
    sigprocmask(SIG_BLOCK, &alarm_only, NULL);
    struct itimerval timer;
    memset(&timer, 0, sizeof timer);
    timer.it_value.tv_usec = 1;
    if (setitimer(ITIMER_REAL, &timer, NULL) != 0) {
        return 3;
    }
    for (int turn = 0; turn < 200; turn++) {
        if (write(STDERR_FILENO, ".", 1) != 1) {
            return 4;
        }
    }
    sigprocmask(SIG_UNBLOCK, &alarm_only, NULL);
    char line[96];
    int length = snprintf(
        line, sizeof line, "install=%d errno=%d runs=%d\n", installed, install_errno,
        (int)runs);
    if (write(1, line, (size_t)length) != length) {
        return 2;
    }
    say("heap after SIGALRM");
    return 0;
}
