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
 *       sees from clock_gettime, for both clocks, including a past deadline
 *       (which expires promptly, though not necessarily before settime returns).
 *   select_mixed / pselect_mixed / select_remaining
 *       A ready pipe plus an expired timerfd reports both bits and a count of
 *       2; a timerfd ending a select early leaves the remaining timeout.
 *   wide_select / wide_pselect / wide_select_poll
 *       The same with the timerfd above descriptor 64, beyond one fd_set word:
 *       a finite select and an infinite pselect end when the timer fires, and
 *       a zero-timeout select reports it beside a ready pipe.
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
 *   poll_cross_arm / poll_cross_rearm / epoll_cross_arm / epoll_cross_rearm
 *       Another thread arming a disarmed timer, or re-arming a distant one to
 *       fire soon, ends a blocked poll or epoll_wait when that timer fires,
 *       with an infinite and with a finite timeout.
 *   epoll_ctl_add_while_waiting
 *       An armed timerfd added by another thread to an epoll that a thread is
 *       already waiting on ends the wait when it fires.
 *   readv_split / readv_short / preadv2_current / pread_espipe
 *       readv and preadv2 at offset -1 read the count like read, across a
 *       split iovec; a total length below 8 is EINVAL; a positioned read is
 *       ESPIPE.
 *   readv_partial / readv_fault
 *       A readv whose second iovec faults returns the bytes copied into the
 *       first; one whose first iovec faults is EFAULT. Both take the
 *       expiration, so the next read finds none.
 *   read_fault_consumes
 *       A read into an unmapped buffer faults after taking the expirations,
 *       so the next read finds none.
 *   create_errors / gettime_errors / settime_errors
 *       Linux's argument-checking order: bad flags or clock are EINVAL (even
 *       for an alarm clock); gettime checks the descriptor before the output
 *       pointer; settime checks the input pointer before the descriptor; a
 *       disarmed timer reads back as zero.
 *   periodic_sleep / close_armed_sleep
 *       A fine-interval periodic timer, whether watched later or already
 *       closed, does not stall an unrelated sleep.
 *   huge_interval / max_interval
 *       An interval beyond KTIME_MAX reads back clamped to KTIME_MAX, and the
 *       next expiry after the first one neither wraps nor crashes.
 *   epoll_pwait_masked_ready
 *       epoll_pwait with a signal mask returns a ready pipe beside a distant
 *       timerfd, and an expired timerfd, without blocking.
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
#include <pthread.h>
#include <sys/epoll.h>
#include <sys/select.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/timerfd.h>
#include <sys/uio.h>
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
    /* Linux expires a past deadline from its timer interrupt, not inside
     * settime, so a read issued at once can still see EAGAIN natively. */
    struct pollfd pfd = {.fd = fd, .events = POLLIN};
    int n = poll(&pfd, 1, 5000);
    uint64_t count = 0;
    ssize_t r = read(fd, &count, sizeof count);
    close(fd);
    if (n != 1 || r != 8 || count != 1) fail(name, "r=%ld count=%ld", (long)r, (long)count);
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

/* Move a descriptor above one fd_set word. */
static int wide_fd(int fd) {
    int wide = fcntl(fd, F_DUPFD, 100);
    close(fd);
    return wide;
}

static void check_wide_select(int use_pselect) {
    const char *name = use_pselect ? "wide_pselect" : "wide_select";
    int tfd = wide_fd(armed_tfd(CLOCK_MONOTONIC, 0, 20 * MS, 0, 0));
    if (tfd < 64) { fail(name, "fd=%ld errno=%ld", tfd, errno); return; }
    fd_set rfds;
    FD_ZERO(&rfds);
    FD_SET(tfd, &rfds);
    int n;
    if (use_pselect) {
        n = pselect(tfd + 1, &rfds, NULL, NULL, NULL, NULL);
    } else {
        struct timeval tv = {1, 0};
        n = select(tfd + 1, &rfds, NULL, NULL, &tv);
    }
    int isset = FD_ISSET(tfd, &rfds) ? 1 : 0;
    close(tfd);
    if (n != 1 || !isset) fail(name, "n=%ld isset=%ld", n, isset);
    else ok(name);
}

static void check_wide_select_poll(void) {
    const char *name = "wide_select_poll";
    int p[2];
    if (ready_pipe(p) != 0) { fail(name, "pipe errno=%ld%ld", errno, 0); return; }
    int tfd = wide_fd(armed_tfd(CLOCK_MONOTONIC, 0, 1 * MS, 0, 0));
    if (tfd < 64) { fail(name, "fd=%ld errno=%ld", tfd, errno); return; }
    sleep_ns(10 * MS);
    fd_set rfds;
    FD_ZERO(&rfds);
    FD_SET(p[0], &rfds);
    FD_SET(tfd, &rfds);
    struct timeval tv = {0, 0};
    int n = select(tfd + 1, &rfds, NULL, NULL, &tv);
    int bits = (FD_ISSET(p[0], &rfds) ? 1 : 0) + (FD_ISSET(tfd, &rfds) ? 2 : 0);
    if (n != 2 || bits != 3) fail(name, "n=%ld bits=%ld", n, bits);
    else ok(name);
    close(p[0]);
    close(p[1]);
    close(tfd);
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

/* Another thread's timerfd_settime while the main thread waits. */
struct arm_job {
    int tfd;
    int epfd;          /* when >= 0, EPOLL_CTL_ADD tfd after arming */
    int64_t delay_ns;  /* before arming */
    int64_t value_ns;  /* relative expiry */
};

static void *arm_later(void *arg) {
    struct arm_job *job = arg;
    sleep_ns(job->delay_ns);
    struct itimerspec its;
    memset(&its, 0, sizeof its);
    its.it_value = ns_ts(job->value_ns);
    timerfd_settime(job->tfd, 0, &its, NULL);
    if (job->epfd >= 0) {
        struct epoll_event ev = {.events = EPOLLIN, .data.u64 = 77};
        epoll_ctl(job->epfd, EPOLL_CTL_ADD, job->tfd, &ev);
    }
    return NULL;
}

/* use_epoll selects the waiter; initial_ns 0 starts disarmed, otherwise the
 * timer starts armed far away and is re-armed; timeout_ms is the wait's. */
static void check_cross_arm(const char *name, int use_epoll, int64_t initial_ns,
                            int timeout_ms) {
    int tfd = initial_ns ? armed_tfd(CLOCK_MONOTONIC, 0, initial_ns, 0, 0)
                         : timerfd_create(CLOCK_MONOTONIC, 0);
    int ep = use_epoll ? epoll_with(tfd, EPOLLIN, 5) : -1;
    struct arm_job job = {tfd, -1, 20 * MS, 10 * MS};
    pthread_t thread;
    int64_t start = now_ns(CLOCK_MONOTONIC);
    pthread_create(&thread, NULL, arm_later, &job);
    int n;
    uint64_t data = 0;
    if (use_epoll) {
        struct epoll_event ev;
        n = epoll_wait(ep, &ev, 1, timeout_ms);
        data = n == 1 ? ev.data.u64 : 0;
    } else {
        struct pollfd pfd = {.fd = tfd, .events = POLLIN};
        n = poll(&pfd, 1, timeout_ms);
        data = n == 1 && pfd.revents == POLLIN ? 5 : 0;
    }
    int64_t elapsed = now_ns(CLOCK_MONOTONIC) - start;
    pthread_join(thread, NULL);
    if (ep >= 0) close(ep);
    close(tfd);
    if (n != 1 || data != 5) fail(name, "n=%ld data=%ld", n, (long)data);
    else if (elapsed < 30 * MS || elapsed > 2000 * MS)
        fail(name, "elapsed_ms=%ld%ld", (long)(elapsed / MS), 0);
    else ok(name);
}

static void check_epoll_ctl_add_while_waiting(void) {
    const char *name = "epoll_ctl_add_while_waiting";
    int tfd = timerfd_create(CLOCK_MONOTONIC, 0);
    int ep = epoll_create1(0);
    struct arm_job job = {tfd, ep, 20 * MS, 10 * MS};
    pthread_t thread;
    int64_t start = now_ns(CLOCK_MONOTONIC);
    pthread_create(&thread, NULL, arm_later, &job);
    struct epoll_event ev;
    memset(&ev, 0, sizeof ev);
    int n = epoll_wait(ep, &ev, 1, 10000);
    int64_t elapsed = now_ns(CLOCK_MONOTONIC) - start;
    pthread_join(thread, NULL);
    close(ep);
    close(tfd);
    if (n != 1 || ev.data.u64 != 77) fail(name, "n=%ld data=%ld", n, (long)ev.data.u64);
    else if (elapsed < 30 * MS || elapsed > 2000 * MS)
        fail(name, "elapsed_ms=%ld%ld", (long)(elapsed / MS), 0);
    else ok(name);
}

/* A nonblocking timer that has expired exactly once. */
static int expired_tfd(void) {
    int fd = armed_tfd(CLOCK_MONOTONIC, TFD_NONBLOCK, 10 * MS, 0, 0);
    sleep_ns(30 * MS);
    return fd;
}

static void check_vectored_reads(void) {
    int tfd = expired_tfd();
    unsigned char bytes[8];
    memset(bytes, 0, sizeof bytes);
    struct iovec split[2] = {{bytes, 3}, {bytes + 3, 5}};
    ssize_t r = readv(tfd, split, 2);
    uint64_t count = 0;
    memcpy(&count, bytes, sizeof count);
    if (r != 8 || count != 1) fail("readv_split", "r=%ld count=%ld", (long)r, (long)count);
    else ok("readv_split");
    close(tfd);

    tfd = expired_tfd();
    struct iovec small[2] = {{bytes, 3}, {bytes + 3, 4}};
    errno = 0;
    r = readv(tfd, small, 2);
    int err = errno;
    /* The rejected read left the expiration in place. */
    uint64_t left = 0;
    ssize_t again = read(tfd, &left, sizeof left);
    if (r != -1 || err != EINVAL) fail("readv_short", "r=%ld errno=%ld", (long)r, err);
    else if (again != 8 || left != 1) fail("readv_short", "again=%ld left=%ld", (long)again, (long)left);
    else ok("readv_short");
    close(tfd);

    tfd = expired_tfd();
    count = 0;
    struct iovec whole = {&count, sizeof count};
    r = preadv2(tfd, &whole, 1, -1, 0);
    if (r != 8 || count != 1) fail("preadv2_current", "r=%ld count=%ld", (long)r, (long)count);
    else ok("preadv2_current");
    close(tfd);

    tfd = expired_tfd();
    errno = 0;
    r = pread(tfd, &count, sizeof count, 0);
    err = errno;
    if (r != -1 || err != ESPIPE) fail("pread_espipe", "r=%ld errno=%ld", (long)r, err);
    else ok("pread_espipe");
    close(tfd);
}

static void check_readv_faults(void) {
    int tfd = expired_tfd();
    unsigned char bytes[4] = {0xff, 0xff, 0xff, 0xff};
    struct iovec partial[2] = {{bytes, 4}, {(void *)1, 4}};
    errno = 0;
    ssize_t r = readv(tfd, partial, 2);
    int err = errno;
    uint64_t count = 0;
    errno = 0;
    ssize_t again = read(tfd, &count, sizeof count);
    int again_err = errno;
    close(tfd);
    uint32_t low;
    memcpy(&low, bytes, sizeof low);
    if (r != 4 || low != 1) fail("readv_partial", "r=%ld errno=%ld", (long)r, r == 4 ? (long)low : err);
    else if (again != -1 || again_err != EAGAIN)
        fail("readv_partial", "again=%ld errno=%ld", (long)again, again_err);
    else ok("readv_partial");

    tfd = expired_tfd();
    struct iovec bad = {(void *)1, 8};
    errno = 0;
    r = readv(tfd, &bad, 1);
    err = errno;
    errno = 0;
    again = read(tfd, &count, sizeof count);
    again_err = errno;
    close(tfd);
    if (r != -1 || err != EFAULT) fail("readv_fault", "r=%ld errno=%ld", (long)r, err);
    else if (again != -1 || again_err != EAGAIN)
        fail("readv_fault", "again=%ld errno=%ld", (long)again, again_err);
    else ok("readv_fault");
}

static void check_read_fault_consumes(void) {
    const char *name = "read_fault_consumes";
    int tfd = expired_tfd();
    errno = 0;
    ssize_t r = syscall(SYS_read, tfd, NULL, 8);
    int err = errno;
    uint64_t count = 0;
    errno = 0;
    ssize_t again = read(tfd, &count, sizeof count);
    int again_err = errno;
    close(tfd);
    if (r != -1 || err != EFAULT) fail(name, "r=%ld errno=%ld", (long)r, err);
    else if (again != -1 || again_err != EAGAIN)
        fail(name, "again=%ld errno=%ld", (long)again, again_err);
    else ok(name);
}

static void check_create_errors(void) {
    const char *name = "create_errors";
    struct { int clock; int flags; } cases[] = {
        {12345, 0},
        {CLOCK_PROCESS_CPUTIME_ID, 0},
        {CLOCK_MONOTONIC, 0x1},
        {CLOCK_REALTIME_ALARM, 0x1},
        {CLOCK_BOOTTIME_ALARM, O_APPEND},
    };
    for (int i = 0; i < (int)(sizeof cases / sizeof cases[0]); i++) {
        errno = 0;
        int fd = timerfd_create(cases[i].clock, cases[i].flags);
        if (fd != -1 || errno != EINVAL) {
            fail(name, "case=%ld errno=%ld", i, errno);
            if (fd >= 0) close(fd);
            return;
        }
    }
    ok(name);
}

static void check_gettime_errors(void) {
    const char *name = "gettime_errors";
    int p[2];
    pipe(p);
    struct itimerspec cur;
    errno = 0;
    long bad_null = syscall(SYS_timerfd_gettime, -1, NULL);
    int bad_null_err = errno;
    errno = 0;
    long not_timer = syscall(SYS_timerfd_gettime, p[0], NULL);
    int not_timer_err = errno;
    int tfd = timerfd_create(CLOCK_MONOTONIC, 0);
    errno = 0;
    long null_out = syscall(SYS_timerfd_gettime, tfd, NULL);
    int null_out_err = errno;
    memset(&cur, 0xff, sizeof cur);
    long disarmed = timerfd_gettime(tfd, &cur);
    close(tfd);
    close(p[0]);
    close(p[1]);
    if (bad_null != -1 || bad_null_err != EBADF)
        fail(name, "bad_fd_null errno=%ld%ld", bad_null_err, 0);
    else if (not_timer != -1 || not_timer_err != EINVAL)
        fail(name, "pipe_null errno=%ld%ld", not_timer_err, 0);
    else if (null_out != -1 || null_out_err != EFAULT)
        fail(name, "timer_null errno=%ld%ld", null_out_err, 0);
    else if (disarmed != 0 || cur.it_value.tv_sec || cur.it_value.tv_nsec ||
             cur.it_interval.tv_sec || cur.it_interval.tv_nsec)
        fail(name, "disarmed r=%ld sec=%ld", disarmed, (long)cur.it_value.tv_sec);
    else ok(name);
}

static void check_settime_errors(void) {
    const char *name = "settime_errors";
    int p[2];
    pipe(p);
    struct itimerspec its;
    memset(&its, 0, sizeof its);
    its.it_value.tv_sec = 1;
    errno = 0;
    long null_bad_fd = syscall(SYS_timerfd_settime, -1, 0, NULL, NULL);
    int null_bad_fd_err = errno;
    errno = 0;
    long bad_fd = syscall(SYS_timerfd_settime, -1, 0, &its, NULL);
    int bad_fd_err = errno;
    errno = 0;
    long not_timer = syscall(SYS_timerfd_settime, p[0], 0, &its, NULL);
    int not_timer_err = errno;
    close(p[0]);
    close(p[1]);
    if (null_bad_fd != -1 || null_bad_fd_err != EFAULT)
        fail(name, "null_bad_fd errno=%ld%ld", null_bad_fd_err, 0);
    else if (bad_fd != -1 || bad_fd_err != EBADF) fail(name, "bad_fd errno=%ld%ld", bad_fd_err, 0);
    else if (not_timer != -1 || not_timer_err != EINVAL)
        fail(name, "pipe errno=%ld%ld", not_timer_err, 0);
    else ok(name);
}

static void check_huge_interval(const char *name, long interval_sec) {
    int tfd = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK);
    struct itimerspec its;
    memset(&its, 0, sizeof its);
    its.it_value.tv_nsec = 1000;
    its.it_interval.tv_sec = interval_sec;
    long set = timerfd_settime(tfd, 0, &its, NULL);
    sleep_ns(5 * MS);
    struct itimerspec cur;
    memset(&cur, 0, sizeof cur);
    long got = timerfd_gettime(tfd, &cur);
    uint64_t count = 0;
    ssize_t r = read(tfd, &count, sizeof count);
    struct itimerspec after;
    memset(&after, 0, sizeof after);
    long got_after = timerfd_gettime(tfd, &after);
    close(tfd);
    /* timespec64_to_ktime clamps to KTIME_MAX; the next expiry must not wrap. */
    if (set != 0 || got != 0 || got_after != 0)
        fail(name, "settime=%ld gettime=%ld", set, got != 0 ? got : got_after);
    else if (cur.it_interval.tv_sec != 9223372036L || cur.it_interval.tv_nsec != 854775807L)
        fail(name, "interval_sec=%ld nsec=%ld", (long)cur.it_interval.tv_sec,
             (long)cur.it_interval.tv_nsec);
    else if (r != 8 || count != 1) fail(name, "r=%ld count=%ld", (long)r, (long)count);
    else if (cur.it_value.tv_sec < 1000000000L || after.it_value.tv_sec < 1000000000L)
        fail(name, "value_sec=%ld after_sec=%ld", (long)cur.it_value.tv_sec,
             (long)after.it_value.tv_sec);
    else ok(name);
}

static void check_epoll_pwait_masked_ready(void) {
    const char *name = "epoll_pwait_masked_ready";
    int p[2];
    ready_pipe(p);
    int far = armed_tfd(CLOCK_MONOTONIC, 0, 100000 * MS, 0, 0);
    int ep = epoll_with(far, EPOLLIN, 1);
    struct epoll_event ev;
    memset(&ev, 0, sizeof ev);
    ev.events = EPOLLIN;
    ev.data.u64 = 2;
    epoll_ctl(ep, EPOLL_CTL_ADD, p[0], &ev);
    sigset_t mask;
    sigemptyset(&mask);
    sigaddset(&mask, SIGUSR2);
    struct epoll_event out[4];
    memset(out, 0, sizeof out);
    int host = epoll_pwait(ep, out, 4, 1000, &mask);
    long host_data = host > 0 ? (long)out[0].data.u64 : 0;
    close(ep);
    close(far);
    close(p[0]);
    close(p[1]);
    int tfd = armed_tfd(CLOCK_MONOTONIC, 0, 1 * MS, 0, 0);
    ep = epoll_with(tfd, EPOLLIN, 3);
    sleep_ns(10 * MS);
    memset(out, 0, sizeof out);
    int timer = epoll_pwait(ep, out, 4, 1000, &mask);
    long timer_data = timer > 0 ? (long)out[0].data.u64 : 0;
    close(ep);
    close(tfd);
    /* A signal mask does not stop a wait from returning what is ready at entry. */
    if (host != 1 || host_data != 2) fail(name, "pipe n=%ld data=%ld", host, host_data);
    else if (timer != 1 || timer_data != 3) fail(name, "timer n=%ld data=%ld", timer, timer_data);
    else ok(name);
}

static void check_fine_periodic_sleep(int close_first) {
    const char *name = close_first ? "close_armed_sleep" : "periodic_sleep";
    int tfd = armed_tfd(CLOCK_MONOTONIC, TFD_NONBLOCK, 1000, 1000, 0);
    if (close_first) close(tfd);
    int64_t start = now_ns(CLOCK_MONOTONIC);
    sleep_ns(100 * MS);
    int64_t elapsed = now_ns(CLOCK_MONOTONIC) - start;
    uint64_t count = 0;
    ssize_t r = close_first ? 8 : read(tfd, &count, sizeof count);
    if (!close_first) close(tfd);
    if (elapsed < 100 * MS || elapsed > 5000 * MS)
        fail(name, "elapsed_ms=%ld%ld", (long)(elapsed / MS), 0);
    else if (r != 8 || (!close_first && count < 1000))
        fail(name, "r=%ld count_ge_1000=%ld", (long)r, (long)(count >= 1000));
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
    check_wide_select(0);
    check_wide_select(1);
    check_wide_select_poll();
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
    check_cross_arm("poll_cross_arm", 0, 0, -1);
    check_cross_arm("poll_cross_rearm", 0, 5000 * MS, 10000);
    check_cross_arm("epoll_cross_arm", 1, 0, -1);
    check_cross_arm("epoll_cross_rearm", 1, 5000 * MS, 10000);
    check_epoll_ctl_add_while_waiting();
    check_vectored_reads();
    check_readv_faults();
    check_read_fault_consumes();
    check_create_errors();
    check_gettime_errors();
    check_settime_errors();
    check_fine_periodic_sleep(0);
    check_fine_periodic_sleep(1);
    check_huge_interval("huge_interval", 17000000000L);
    check_huge_interval("max_interval", 0x7fffffffffffffffL);
    check_epoll_pwait_masked_ready();
    printf("failures=%d\n", failures);
    return failures == 0 ? 0 : 1;
}
