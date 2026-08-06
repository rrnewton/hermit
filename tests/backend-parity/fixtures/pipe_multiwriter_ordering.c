/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Multi-writer pipe ordering parity probe.
 *
 * The existing pipe fixtures do not reach this case: pipe_ipc forks exactly ONE
 * child, and pipe_capacity / pipe2_flags do not fork at all. So nothing yet
 * exercises SEVERAL CONCURRENT WRITERS sharing one pipe, which is the shape
 * where process scheduling becomes guest-observable.
 *
 * The parent creates a pipe and forks N children. Each child burns a
 * DELIBERATELY UNEQUAL amount of CPU -- child i does (N-i) units, so on a real
 * machine the later children finish FIRST -- then writes one identifying line
 * and exits. The parent drains to EOF, prints what it read, then reaps all N
 * with wait(-1) and prints the reap order.
 *
 * Two guest-observable orderings therefore fall out of the schedule, and both
 * are printed to stdout:
 *   1. the ORDER OF LINES in the pipe (which writer won the race), and
 *   2. the REAP ORDER from wait(-1).
 * Natively both vary run to run precisely because the CPU burns are unequal.
 * Under Hermit's deterministic scheduler both must be fixed, and --verify
 * compares this stdout byte-for-byte across two runs, so a scheduling
 * divergence fails the test rather than merely looking different.
 *
 * The unequal burn is load-bearing: with equal work the children would tend to
 * finish in fork order on any machine, and the fixture would pass without
 * discriminating a deterministic scheduler from an accidental one.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/wait.h>

#define NKIDS 5

int main(void) {
    int fd[2];
    if (pipe(fd) != 0) {
        perror("pipe");
        return 2;
    }

    pid_t kids[NKIDS];
    for (int i = 0; i < NKIDS; i++) {
        pid_t p = fork();
        if (p < 0) {
            perror("fork");
            return 2;
        }
        if (p == 0) {
            close(fd[0]);
            /* Unequal work: later children finish sooner on a real machine. */
            volatile unsigned long acc = 0;
            for (unsigned long k = 0; k < (unsigned long)(NKIDS - i) * 200000UL; k++) {
                acc += k;
            }
            char buf[32];
            int n = snprintf(buf, sizeof buf, "w%d\n", i);
            if (write(fd[1], buf, (size_t)n) != n) {
                _exit(3);
            }
            _exit((int)(acc & 1) == 2 ? 4 : i + 1);
        }
        kids[i] = p;
    }
    close(fd[1]);

    char buf[512];
    ssize_t total = 0, r;
    while ((r = read(fd[0], buf + total, sizeof buf - (size_t)total - 1)) > 0) {
        total += r;
    }
    buf[total] = '\0';
    fputs(buf, stdout);

    for (int i = 0; i < NKIDS; i++) {
        int st = 0;
        pid_t got = wait(&st);
        int slot = -1;
        for (int j = 0; j < NKIDS; j++) {
            if (kids[j] == got) {
                slot = j;
            }
        }
        printf("reap%d slot=%d code=%d\n", i, slot, WEXITSTATUS(st));
    }
    fflush(stdout);
    return 0;
}
