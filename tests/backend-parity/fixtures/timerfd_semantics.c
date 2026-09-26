/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * CONTRACT FIXTURE: timerfd semantics that a virtual-time timerfd must keep.
 *
 * Every case checks its own result against what Linux does and prints either
 * `ok <case>` or `FAIL <case> <detail>`; any FAIL makes the exit status 1. The
 * native run is therefore the oracle, and a strict-verification run compares
 * the two Hermit runs against each other on top of that. Durations are checked
 * as ranges only, and are never printed, so host load cannot change stdout.
 *
 * Cases:
 *   abstime_realtime / abstime_monotonic / abstime_past
 *       TFD_TIMER_ABSTIME deadlines are read in the same clock domain the guest
 *       sees from clock_gettime, for both clocks, including a past deadline.
 *   select_mixed / pselect_mixed / select_remaining
 *       A ready pipe plus an expired timerfd reports both bits and a count of
 *       2; a timerfd ending a select early leaves the remaining timeout.
 *   poll_mixed
 *       The same for poll.
 *   epoll_mask / epoll_close / epoll_reuse / epoll_dup_alive
 *       An EPOLLOUT-only interest never reports; closing the last descriptor
 *       removes the interest; a reused descriptor number does not inherit it;
 *       a surviving dup keeps it.
 *   epoll_et_periodic / epoll_et_maxevents / epoll_oneshot
 *       An unread periodic timer raises one edge, and the first expiry after
 *       a read raises the next; an edge event cut by maxevents is still
 *       delivered on the next wait; EPOLLONESHOT disables until EPOLL_CTL_MOD.
 *   invalid_timespec
 *       A negative or out-of-range timespec is EINVAL and leaves the timer as
 *       it was.
 *   cancel_on_set_flags
 *       TFD_TIMER_CANCEL_ON_SET is accepted, not refused, for combinations
 *       where Linux ignores it.
 *   read_restart / read_eintr
 *       A blocking read interrupted by a handler restarts under SA_RESTART and
 *       fails with EINTR without it.
 *   fork_shared
 *       A forked child shares the open file description: its read consumes the
 *       expiration the parent then no longer sees.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <poll.h>
#include <sys/epoll.h>
#include <sys/select.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/timerfd.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define MS (1000L * 1000L)

static int failures = 0;

static void ok(const char *name) { printf("ok %s\n", name); }

static void fail(const char *name, const char *fmt, long a, long b) {
    printf("FAIL %s ", name);
    printf(fmt, a, b);
    printf("\n");
    failures++;
}

static int64_t now_ns(clockid_t clk) {
    struct timespec ts;
    clock_gettime(clk, &ts);
    return (int64_t)ts.tv_sec * 1000000000LL + ts.tv_nsec;
}

static struct timespec ns_ts(int64_t ns) {
    struct timespec ts = {ns / 1000000000LL, ns % 1000000000LL};
    return ts;
}

static int armed_tfd(clockid_t clk, int flags, int64_t value_ns, int64_t interval_ns,
                     int settime_flags) {
    int fd = timerfd_create(clk, flags);
    if (fd < 0) return -1;
    struct itimerspec its;
    memset(&its, 0, sizeof its);
    its.it_value = ns_ts(value_ns);
    its.it_interval = ns_ts(interval_ns);
    if (timerfd_settime(fd, settime_flags, &its, NULL) != 0) {
        close(fd);
        return -1;
    }
    return fd;
}

static void sleep_ns(int64_t ns) {
    struct timespec ts = ns_ts(ns);
    while (nanosleep(&ts, &ts) != 0 && errno == EINTR) {
    }
}

static void check_abstime(const char *name, clockid_t clk) {
    int64_t start = now_ns(CLOCK_MONOTONIC);
    int fd = armed_tfd(clk, 0, now_ns(clk) + 40 * MS, 0, TFD_TIMER_ABSTIME);
    if (fd < 0) { fail(name, "settime errno=%ld%ld", errno, 0); return; }
    uint64_t count = 0;
    ssize_t r = read(fd, &count, sizeof count);
    int64_t elapsed = now_ns(CLOCK_MONOTONIC) - start;
    close(fd);
    if (r != 8 || count != 1) fail(name, "r=%ld count=%ld", (long)r, (long)count);
    else if (elapsed < 40 * MS || elapsed > 5000 * MS)
        fail(name, "elapsed_ms=%ld%ld", (long)(elapsed / MS), 0);
    else ok(name);
}

static void check_abstime_past(void) {
    const char *name = "abstime_past";
    int fd = armed_tfd(CLOCK_REALTIME, TFD_NONBLOCK, now_ns(CLOCK_REALTIME) - 1000 * MS, 0,
                       TFD_TIMER_ABSTIME);
    if (fd < 0) { fail(name, "settime errno=%ld%ld", errno, 0); return; }
    uint64_t count = 0;
    ssize_t r = read(fd, &count, sizeof count);
    close(fd);
    if (r != 8 || count != 1) fail(name, "r=%ld count=%ld", (long)r, (long)count);
    else ok(name);
}

/* A pipe with one readable byte, returned through out-params. */
static int ready_pipe(int p[2]) {
    if (pipe(p) != 0) return -1;
    return write(p[1], "x", 1) == 1 ? 0 : -1;
}

static void check_select_mixed(int use_pselect) {
    const char *name = use_pselect ? "pselect_mixed" : "select_mixed";
    int p[2];
    if (ready_pipe(p) != 0) { fail(name, "pipe errno=%ld%ld", errno, 0); return; }
    int tfd = armed_tfd(CLOCK_MONOTONIC, 0, 1 * MS, 0, 0);
    sleep_ns(10 * MS);
    fd_set rfds;
    FD_ZERO(&rfds);
    FD_SET(p[0], &rfds);
    FD_SET(tfd, &rfds);
    int maxfd = (p[0] > tfd ? p[0] : tfd) + 1;
    int n;
    if (use_pselect) {
        struct timespec ts = {1, 0};
        n = pselect(maxfd, &rfds, NULL, NULL, &ts, NULL);
    } else {
        struct timeval tv = {1, 0};
        n = select(maxfd, &rfds, NULL, NULL, &tv);
    }
    int bits = (FD_ISSET(p[0], &rfds) ? 1 : 0) + (FD_ISSET(tfd, &rfds) ? 2 : 0);
    if (n != 2 || bits != 3) fail(name, "n=%ld bits=%ld", n, bits);
    else ok(name);
    close(p[0]);
    close(p[1]);
    close(tfd);
}

static void check_select_remaining(void) {
    const char *name = "select_remaining";
    int tfd = armed_tfd(CLOCK_MONOTONIC, 0, 40 * MS, 0, 0);
    fd_set rfds;
    FD_ZERO(&rfds);
    FD_SET(tfd, &rfds);
    struct timeval tv = {1, 0};
    int n = select(tfd + 1, &rfds, NULL, NULL, &tv);
    long left_ms = tv.tv_sec * 1000 + tv.tv_usec / 1000;
    close(tfd);
    if (n != 1 || !FD_ISSET(tfd, &rfds)) fail(name, "n=%ld isset=%ld", n, FD_ISSET(tfd, &rfds));
    else if (left_ms < 500 || left_ms > 960) fail(name, "left_ms=%ld%ld", left_ms, 0);
    else ok(name);
}

static void check_poll_mixed(void) {
    const char *name = "poll_mixed";
    int p[2];
    if (ready_pipe(p) != 0) { fail(name, "pipe errno=%ld%ld", errno, 0); return; }
    int tfd = armed_tfd(CLOCK_MONOTONIC, 0, 1 * MS, 0, 0);
    sleep_ns(10 * MS);
    struct pollfd fds[2] = {{p[0], POLLIN, 0}, {tfd, POLLIN, 0}};
    int n = poll(fds, 2, 1000);
    int bits = (fds[0].revents == POLLIN ? 1 : 0) + (fds[1].revents == POLLIN ? 2 : 0);
    if (n != 2 || bits != 3) fail(name, "n=%ld bits=%ld", n, bits);
    else ok(name);
    close(p[0]);
    close(p[1]);
    close(tfd);
}

static int epoll_with(int tfd, uint32_t events, uint64_t data) {
    int ep = epoll_create1(0);
    struct epoll_event ev;
    memset(&ev, 0, sizeof ev);
    ev.events = events;
    ev.data.u64 = data;
    if (ep < 0 || epoll_ctl(ep, EPOLL_CTL_ADD, tfd, &ev) != 0) return -1;
    return ep;
}

static void check_epoll_mask(void) {
    const char *name = "epoll_mask";
    int tfd = armed_tfd(CLOCK_MONOTONIC, 0, 1 * MS, 0, 0);
    int ep = epoll_with(tfd, EPOLLOUT, 7);
    sleep_ns(10 * MS);
    struct epoll_event out[4];
    int n = epoll_wait(ep, out, 4, 0);
    if (n != 0) fail(name, "n=%ld events=%ld", n, n > 0 ? (long)out[0].events : 0);
    else ok(name);
    close(ep);
    close(tfd);
}

static void check_epoll_close(void) {
    const char *name = "epoll_close";
    int tfd = armed_tfd(CLOCK_MONOTONIC, 0, 1 * MS, 0, 0);
    int ep = epoll_with(tfd, EPOLLIN, 7);
    sleep_ns(10 * MS);
    close(tfd);
    struct epoll_event out[4];
    int n = epoll_wait(ep, out, 4, 0);
    if (n != 0) fail(name, "n=%ld errno=%ld", n, n < 0 ? errno : 0);
    else ok(name);
    close(ep);
}

static void check_epoll_reuse(void) {
    const char *name = "epoll_reuse";
    int old = armed_tfd(CLOCK_MONOTONIC, 0, 1 * MS, 0, 0);
    int ep = epoll_with(old, EPOLLIN, 111);
    close(old);
    /* The lowest free descriptor is the one just closed. */
    int fresh = armed_tfd(CLOCK_MONOTONIC, 0, 1 * MS, 0, 0);
    sleep_ns(10 * MS);
    struct epoll_event out[4];
    int n = epoll_wait(ep, out, 4, 0);
    if (fresh != old) fail(name, "fresh=%ld old=%ld", fresh, old);
    else if (n != 0) fail(name, "n=%ld data=%ld", n, n > 0 ? (long)out[0].data.u64 : 0);
    else ok(name);
    close(ep);
    close(fresh);
}

static void check_epoll_dup_alive(void) {
    const char *name = "epoll_dup_alive";
    int tfd = armed_tfd(CLOCK_MONOTONIC, 0, 1 * MS, 0, 0);
    int ep = epoll_with(tfd, EPOLLIN, 222);
    int alias = dup(tfd);
    close(tfd);
    sleep_ns(10 * MS);
    struct epoll_event out[4];
    int n = epoll_wait(ep, out, 4, 0);
    if (n != 1 || out[0].data.u64 != 222 || out[0].events != EPOLLIN)
        fail(name, "n=%ld data=%ld", n, n > 0 ? (long)out[0].data.u64 : 0);
    else ok(name);
    close(ep);
    close(alias);
}

static void check_epoll_et_periodic(void) {
    const char *name = "epoll_et_periodic";
    int tfd = armed_tfd(CLOCK_MONOTONIC, 0, 20 * MS, 20 * MS, 0);
    int ep = epoll_with(tfd, EPOLLIN | EPOLLET, 5);
    struct epoll_event out[4];
    int first = epoll_wait(ep, out, 4, 1000);
    /* Linux forwards a periodic timerfd only when it is read, so later
     * expiries of an unread timer raise no new edge. */
    sleep_ns(50 * MS);
    int unread = epoll_wait(ep, out, 4, 0);
    uint64_t count = 0;
    ssize_t r = read(tfd, &count, sizeof count);
    /* After the read, the next expiry is a new edge. */
    int after_read = epoll_wait(ep, out, 4, 1000);
    if (first != 1 || unread != 0 || r != 8 || count < 2 || after_read != 1)
        fail(name, "unread=%ld after_read=%ld", unread, after_read);
    else ok(name);
    close(ep);
    close(tfd);
}

static void check_epoll_et_maxevents(void) {
    const char *name = "epoll_et_maxevents";
    int a = armed_tfd(CLOCK_MONOTONIC, 0, 1 * MS, 0, 0);
    int b = armed_tfd(CLOCK_MONOTONIC, 0, 1 * MS, 0, 0);
    int ep = epoll_with(a, EPOLLIN | EPOLLET, 1);
    struct epoll_event ev;
    memset(&ev, 0, sizeof ev);
    ev.events = EPOLLIN | EPOLLET;
    ev.data.u64 = 2;
    epoll_ctl(ep, EPOLL_CTL_ADD, b, &ev);
    sleep_ns(10 * MS);
    struct epoll_event out[1];
    int first = epoll_wait(ep, out, 1, 0);
    uint64_t first_data = first == 1 ? out[0].data.u64 : 0;
    int second = epoll_wait(ep, out, 1, 0);
    uint64_t second_data = second == 1 ? out[0].data.u64 : 0;
    int third = epoll_wait(ep, out, 1, 0);
    if (first != 1 || second != 1 || first_data == second_data || third != 0)
        fail(name, "second=%ld third=%ld", second, third);
    else ok(name);
    close(ep);
    close(a);
    close(b);
}

static void check_epoll_oneshot(void) {
    const char *name = "epoll_oneshot";
    int tfd = armed_tfd(CLOCK_MONOTONIC, 0, 1 * MS, 0, 0);
    int ep = epoll_with(tfd, EPOLLIN | EPOLLONESHOT, 9);
    sleep_ns(10 * MS);
    struct epoll_event out[4];
    int first = epoll_wait(ep, out, 4, 0);
    int second = epoll_wait(ep, out, 4, 0);
    struct epoll_event ev;
    memset(&ev, 0, sizeof ev);
    ev.events = EPOLLIN | EPOLLONESHOT;
    ev.data.u64 = 9;
    epoll_ctl(ep, EPOLL_CTL_MOD, tfd, &ev);
    int rearmed = epoll_wait(ep, out, 4, 0);
    if (first != 1 || second != 0 || rearmed != 1)
        fail(name, "second=%ld rearmed=%ld", second, rearmed);
    else ok(name);
    close(ep);
    close(tfd);
}

static void check_invalid_timespec(void) {
    const char *name = "invalid_timespec";
    int tfd = armed_tfd(CLOCK_MONOTONIC, 0, 10000 * MS, 0, 0);
    struct itimerspec bad[4];
    memset(bad, 0, sizeof bad);
    bad[0].it_value.tv_nsec = -1;
    bad[1].it_value.tv_nsec = 1000000000L;
    bad[2].it_value.tv_sec = -1;
    bad[3].it_value.tv_sec = 1;
    bad[3].it_interval.tv_nsec = -1;
    for (int i = 0; i < 4; i++) {
        errno = 0;
        int r = timerfd_settime(tfd, 0, &bad[i], NULL);
        if (r != -1 || errno != EINVAL) {
            fail(name, "case=%ld errno=%ld", i, errno);
            close(tfd);
            return;
        }
    }
    struct itimerspec cur;
    timerfd_gettime(tfd, &cur);
    close(tfd);
    /* Still armed with the original ~10 s deadline. */
    if (cur.it_value.tv_sec < 5) fail(name, "left_s=%ld%ld", (long)cur.it_value.tv_sec, 0);
    else ok(name);
}

static void check_cancel_on_set_flags(void) {
    const char *name = "cancel_on_set_flags";
    int combos[3][2] = {
        {CLOCK_MONOTONIC, TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET},
        {CLOCK_MONOTONIC, TFD_TIMER_CANCEL_ON_SET},
        {CLOCK_REALTIME, TFD_TIMER_CANCEL_ON_SET},
    };
    for (int i = 0; i < 3; i++) {
        int fd = timerfd_create(combos[i][0], 0);
        struct itimerspec its;
        memset(&its, 0, sizeof its);
        its.it_value.tv_sec = 1000;
        if (combos[i][1] & TFD_TIMER_ABSTIME)
            its.it_value.tv_sec += now_ns(combos[i][0]) / 1000000000LL;
        errno = 0;
        int r = timerfd_settime(fd, combos[i][1], &its, NULL);
        close(fd);
        if (r != 0) { fail(name, "combo=%ld errno=%ld", i, errno); return; }
    }
    ok(name);
}

static volatile sig_atomic_t handled = 0;
static void on_alarm(int s) { (void)s; handled++; }

static void check_read_signal(int restart) {
    const char *name = restart ? "read_restart" : "read_eintr";
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_alarm;
    sa.sa_flags = restart ? SA_RESTART : 0;
    sigaction(SIGALRM, &sa, NULL);
    handled = 0;
    int tfd = armed_tfd(CLOCK_MONOTONIC, 0, 200 * MS, 0, 0);
    struct itimerval itv;
    memset(&itv, 0, sizeof itv);
    itv.it_value.tv_usec = 20 * 1000;
    setitimer(ITIMER_REAL, &itv, NULL);
    uint64_t count = 0;
    errno = 0;
    ssize_t r = read(tfd, &count, sizeof count);
    int err = errno;
    close(tfd);
    signal(SIGALRM, SIG_DFL);
    if (handled != 1) fail(name, "handled=%ld r=%ld", (long)handled, (long)r);
    else if (restart && (r != 8 || count != 1)) fail(name, "r=%ld errno=%ld", (long)r, err);
    else if (!restart && (r != -1 || err != EINTR)) fail(name, "r=%ld errno=%ld", (long)r, err);
    else ok(name);
}

static void check_fork_shared(void) {
    const char *name = "fork_shared";
    int tfd = armed_tfd(CLOCK_MONOTONIC, TFD_NONBLOCK, 20 * MS, 0, 0);
    pid_t pid = fork();
    if (pid == 0) {
        sleep_ns(50 * MS);
        uint64_t count = 0;
        ssize_t r = read(tfd, &count, sizeof count);
        _exit(r == 8 && count == 1 ? 0 : 1);
    }
    int status = 0;
    waitpid(pid, &status, 0);
    uint64_t count = 0;
    errno = 0;
    ssize_t r = read(tfd, &count, sizeof count);
    int err = errno;
    close(tfd);
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0)
        fail(name, "child_status=%ld%ld", (long)status, 0);
    else if (r != -1 || err != EAGAIN) fail(name, "parent_r=%ld errno=%ld", (long)r, err);
    else ok(name);
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    check_abstime("abstime_realtime", CLOCK_REALTIME);
    check_abstime("abstime_monotonic", CLOCK_MONOTONIC);
    check_abstime_past();
    check_select_mixed(0);
    check_select_mixed(1);
    check_select_remaining();
    check_poll_mixed();
    check_epoll_mask();
    check_epoll_close();
    check_epoll_reuse();
    check_epoll_dup_alive();
    check_epoll_et_periodic();
    check_epoll_et_maxevents();
    check_epoll_oneshot();
    check_invalid_timespec();
    check_cancel_on_set_flags();
    check_read_signal(1);
    check_read_signal(0);
    check_fork_shared();
    printf("failures=%d\n", failures);
    return failures == 0 ? 0 : 1;
}
