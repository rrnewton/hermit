/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Signal phase 1, step I4 (in-guest LiteInst): a guest SIGALRM handler runs.
 * Prints one line per observation; the test compares them.
 *
 * - pause: alarm(1) then pause(); the handler runs once and pause returns
 *   EINTR (4). The handler sees SIGALRM, si_code SI_KERNEL (128), SIGALRM
 *   blocked inside it, and the mask is restored after it returns;
 * - unblock: with SIGALRM blocked, a one-shot ITIMER_REAL expires while the
 *   guest writes; nothing runs until sigprocmask unblocks it, and the handler
 *   has run once when that call returns.
 */

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/time.h>
#include <unistd.h>

static volatile sig_atomic_t runs;
static volatile sig_atomic_t last_signal;
static volatile sig_atomic_t last_code;
static volatile sig_atomic_t blocked_inside;

static void handler(int signal, siginfo_t *info, void *context) {
    (void)context;
    sigset_t current;
    sigprocmask(SIG_BLOCK, NULL, &current);
    runs++;
    last_signal = signal;
    last_code = info->si_code;
    blocked_inside = sigismember(&current, SIGALRM);
}

static int alarm_blocked(void) {
    sigset_t current;
    sigprocmask(SIG_BLOCK, NULL, &current);
    return sigismember(&current, SIGALRM);
}

int main(void) {
    struct sigaction action;
    memset(&action, 0, sizeof action);
    action.sa_sigaction = handler;
    action.sa_flags = SA_SIGINFO;
    int result = sigaction(SIGALRM, &action, NULL);
    printf("install=%d errno=%d\n", result, result != 0 ? errno : 0);
    fflush(stdout);

    alarm(1);
    result = pause();
    printf("pause=%d errno=%d runs=%d signal=%d code=%d blocked_inside=%d blocked_after=%d\n",
           result, result != 0 ? errno : 0, (int)runs, (int)last_signal, (int)last_code,
           (int)blocked_inside, alarm_blocked());
    fflush(stdout);

    sigset_t blocked;
    sigemptyset(&blocked);
    sigaddset(&blocked, SIGALRM);
    sigprocmask(SIG_BLOCK, &blocked, NULL);
    struct itimerval timer;
    memset(&timer, 0, sizeof timer);
    timer.it_value.tv_usec = 1;
    if (setitimer(ITIMER_REAL, &timer, NULL) != 0) {
        printf("setitimer_failed errno=%d\n", errno);
        return 1;
    }
    /* Scheduler turns, during which the expiry commits. */
    for (int i = 0; i < 200; i++) {
        if (write(STDERR_FILENO, ".", 1) != 1) {
            return 1;
        }
    }
    int before = runs;
    sigprocmask(SIG_UNBLOCK, &blocked, NULL);
    int after = runs;
    printf("unblock: before=%d after=%d blocked_after=%d\n", before - 1, after - 1, alarm_blocked());
    return 0;
}
