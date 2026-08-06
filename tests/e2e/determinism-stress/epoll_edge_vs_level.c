/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Contract fixture: epoll EDGE-triggered vs LEVEL-triggered wakeup counts.
 *
 * WHY THIS GAP IS INVISIBLE TO SIMPLE FIXTURES. The SAME fd sequence yields
 * DIFFERENT wakeup counts under EPOLLET than without it: level-triggered
 * re-reports readiness on every wait while data remains buffered, edge-
 * triggered reports it once per transition. A backend whose edge semantics are
 * subtly wrong therefore diverges ONLY for edge-triggered guests -- which is
 * most real high-performance code and none of the simple fixtures here, all of
 * which use the level-triggered default. The bug hides behind the default.
 *
 * The discriminating observable is the DIFFERENCE between the two modes over
 * an identical fd script, so this runs both against the same sequence.
 *
 * BRANCHES on the counts rather than printing them, and emits the OBSERVED
 * counts rather than ok=N, so the record shows what was seen.
 */

#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <unistd.h>

/* One wait with a 0ms timeout: never blocks, so the count is a function of
   readiness semantics only and never of timing. That is what makes this
   deterministic enough to pin. */
static int poll_once(int ep) {
    struct epoll_event ev;
    int n = epoll_wait(ep, &ev, 1, 0);
    return n;
}

/* Identical fd script under both modes: write 2 bytes, then poll 3 times
   WITHOUT draining. Level re-reports each time; edge reports the transition
   once and then goes quiet. */
static int run_mode(int edge, int *first, int *second, int *third) {
    int fds[2];
    if (pipe(fds) != 0) {
        return -1;
    }
    int ep = epoll_create1(0);
    if (ep < 0) {
        return -1;
    }
    struct epoll_event ev;
    memset(&ev, 0, sizeof(ev));
    ev.events = EPOLLIN | (edge ? EPOLLET : 0);
    ev.data.fd = fds[0];
    if (epoll_ctl(ep, EPOLL_CTL_ADD, fds[0], &ev) != 0) {
        return -1;
    }

    if (write(fds[1], "ab", 2) != 2) {
        return -1;
    }
    *first = poll_once(ep);
    *second = poll_once(ep);
    *third = poll_once(ep);

    close(ep);
    close(fds[0]);
    close(fds[1]);
    return 0;
}

int main(void) {
    int l1 = -1, l2 = -1, l3 = -1;
    int e1 = -1, e2 = -1, e3 = -1;

    if (run_mode(0, &l1, &l2, &l3) != 0) {
        fprintf(stderr, "level-triggered setup failed: %s\n", strerror(errno));
        return 1;
    }
    if (run_mode(1, &e1, &e2, &e3) != 0) {
        fprintf(stderr, "edge-triggered setup failed: %s\n", strerror(errno));
        return 1;
    }

    /* OBSERVED VALUES, not a pass count. */
    printf("level.wakeups=%d,%d,%d\n", l1, l2, l3);
    printf("edge.wakeups=%d,%d,%d\n", e1, e2, e3);

    int level_total = l1 + l2 + l3;
    int edge_total = e1 + e2 + e3;
    printf("level.total=%d\n", level_total);
    printf("edge.total=%d\n", edge_total);

    /* BRANCH on the semantics, so a wrong implementation changes control flow.
       Level: readiness persists while the pipe holds data -> reported every
       wait. Edge: one transition -> reported once, then silent. */
    printf("branch.level_repeats=%s\n", (l1 == 1 && l2 == 1 && l3 == 1) ? "yes" : "no");
    printf("branch.edge_fires_once=%s\n", (e1 == 1 && e2 == 0 && e3 == 0) ? "yes" : "no");
    /* THE discriminator: the two modes must NOT agree over this script. If a
       backend implements EPOLLET as level, this flips and the fixture fails. */
    printf("branch.modes_differ=%s\n", (level_total != edge_total) ? "yes" : "no");
    fflush(stdout);
    return 0;
}
