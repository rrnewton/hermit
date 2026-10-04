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
 *   epoll_lt_rotation / epoll_lt_rotation_across_mod / epoll_lt_host_fairness /
 *   epoll_lt_host_fairness_pipe_first
 *       With maxevents 1, two always-ready level-triggered items take turns,
 *       as Linux re-queues a delivered one at the tail of the ready list:
 *       two expired timerfds, also when both interests are re-applied with
 *       EPOLL_CTL_MOD after every wait (MOD leaves a queued item in place),
 *       and an expired timerfd beside a readable pipe in either registration
 *       order.
 *   invalid_timespec
 *       A negative or out-of-range timespec is EINVAL and leaves the timer as
 *       it was.
 *   cancel_on_set_flags
 *       TFD_TIMER_CANCEL_ON_SET is accepted, not refused, for combinations
 *       where Linux ignores it.
 *   read_restart / read_eintr
 *       A blocking read interrupted by a handler restarts under SA_RESTART and
 *       fails with EINTR without it.
 *   fork_shared / fork_expired / fork_rearm / fork_disarm
 *       A forked child shares the open file description: its read consumes the
 *       expiration the parent then no longer sees, whether the timer expired
 *       after the fork or before it, and when the child re-arms the timer to
 *       fire soon or disarms it, the parent's timer is the one that changed.
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
 *   read_across_close / read_across_reuse / readv_across_close /
 *   readv_across_reuse
 *       A read or readv blocked on a timer keeps that timer when another
 *       thread closes the descriptor, and when a new timerfd then takes the
 *       freed number: it returns the old timer's count, not EBADF, and leaves
 *       the new timer's expiration unread.
 *   readv_empty / readv_zero_lengths / preadv2_zero_length /
 *   readv_zero_blocking / read_zero_einval
 *       A readv or preadv2 whose iovecs hold no bytes returns 0 without
 *       consuming the expiration, and without waiting on a blocking timer;
 *       a read of 0 bytes is EINVAL and leaves the expiration in place.
 *   read_readonly / readv_readonly / read_straddle_readonly /
 *   readv_readonly_partial / gettime_readonly / settime_old_readonly
 *       Results are copied out with the caller's access rights: a read into a
 *       read-only page is EFAULT and leaves the page untouched, a buffer that
 *       runs into a read-only page gets the bytes before it, and either way
 *       the expiration is gone; gettime, and settime's old value, into a
 *       read-only page are EFAULT, and settime's new arming still holds.
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
 *       next expiry after the first one neither wraps nor crashes: it stops at
 *       KTIME_MAX, so the time left equals that of an absolute timer armed at
 *       KTIME_MAX, read just before and just after.
 *   huge_relative / near_max_relative
 *       A relative arming whose expiry would pass KTIME_MAX (a value beyond it,
 *       or one just below it plus the current time) expires at KTIME_MAX, the
 *       same instant as that absolute reference.
 *   epoll_pwait_masked_ready
 *       epoll_pwait with a signal mask returns a ready pipe beside a distant
 *       timerfd, and an expired timerfd, without blocking.
 *   ppoll_mixed / ppoll_ready / ppoll_disarmed / ppoll_ready_infinite
 *       ppoll reports an expired timerfd beside a ready pipe, ends a finite
 *       and an infinite wait when an armed timer fires, and times out when
 *       the timer was disarmed before it could fire.
 *   scm_rights_sendmsg / scm_rights_sendmmsg
 *       A timerfd received over SCM_RIGHTS, through sendmsg or sendmmsg, is
 *       the sender's timer under a new number: the arming made before the
 *       send reads back through the new number, a re-arming through the new
 *       number reads back through the original, and so does a disarming
 *       through the original.
 *   preadv2_hipri / preadv2_nowait / preadv2_dsync / preadv2_nowait_pending /
 *   preadv2_hipri_blocking / preadv2_dsync_blocking
 *       preadv2 at offset -1 with RWF_HIPRI reads an expired timer's count on
 *       every kernel. With RWF_NOWAIT or RWF_DSYNC it reads it where timerfd
 *       reads through read_iter, and is EOPNOTSUPP on older kernels. RWF_NOWAIT
 *       on a blocking timer that has not fired is EAGAIN (or EOPNOTSUPP) and
 *       leaves the timer armed. Expired blocking timers return their count
 *       without waiting.
 *
 * With the single argument `sharing`, only fork_expired, fork_rearm,
 * fork_disarm and the ppoll cases run. They check what a forked child shares
 * and what a wait reports, and never compare an expiry with the guest's
 * clock, so they also hold on backends that keep timerfds as host kernel
 * objects (DBT, SaBRe, in-guest LiteInst and KVM). The other cases, fork_shared
 * among them, time expiries against the guest's clock.
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
#include <sys/mman.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/timerfd.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define MS (1000L * 1000L)

#ifndef RWF_HIPRI
#define RWF_HIPRI 0x00000001
#endif
#ifndef RWF_DSYNC
#define RWF_DSYNC 0x00000002
#endif
#ifndef RWF_NOWAIT
#define RWF_NOWAIT 0x00000008
#endif

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

/* Four timeout-0 epoll_wait calls with maxevents 1; stores each call's data
 * and returns the number of calls that reported exactly one EPOLLIN event.
 * With remod, every call is followed by EPOLL_CTL_MOD re-applying both
 * interests unchanged (first_fd with data 1, second_fd with data 2), as an
 * event loop that re-arms its interests on every iteration does. */
static int epoll_wait_one_each(int ep, uint64_t seen[4], int remod, int first_fd,
                               int second_fd) {
    int good = 0;
    for (int i = 0; i < 4; i++) {
        struct epoll_event out[1];
        memset(out, 0, sizeof out);
        int n = epoll_wait(ep, out, 1, 0);
        seen[i] = n == 1 ? out[0].data.u64 : 0;
        if (n == 1 && out[0].events == EPOLLIN) good++;
        if (!remod) continue;
        struct epoll_event mod;
        memset(&mod, 0, sizeof mod);
        mod.events = EPOLLIN;
        mod.data.u64 = 1;
        if (epoll_ctl(ep, EPOLL_CTL_MOD, first_fd, &mod) != 0) good = -100;
        mod.data.u64 = 2;
        if (epoll_ctl(ep, EPOLL_CTL_MOD, second_fd, &mod) != 0) good = -100;
    }
    return good;
}

/* Linux re-queues a delivered level-triggered item at the tail of the ready
 * list (ep_send_events), and EPOLL_CTL_MOD leaves an item already on the
 * ready list in place (ep_modify), so with maxevents 1 two always-ready items
 * take turns, with or without the re-arming: both appear within two calls,
 * and four calls alternate. */
static void check_epoll_lt_alternation(const char *name, int first_fd, int second_fd,
                                       int remod) {
    int ep = epoll_with(first_fd, EPOLLIN, 1);
    struct epoll_event ev;
    memset(&ev, 0, sizeof ev);
    ev.events = EPOLLIN;
    ev.data.u64 = 2;
    if (ep < 0 || epoll_ctl(ep, EPOLL_CTL_ADD, second_fd, &ev) != 0) {
        fail(name, "setup errno=%ld%ld", errno, 0);
        if (ep >= 0) close(ep);
        return;
    }
    sleep_ns(10 * MS);
    uint64_t seen[4];
    int good = epoll_wait_one_each(ep, seen, remod, first_fd, second_fd);
    int alternates = seen[0] != seen[1] && seen[1] != seen[2] && seen[2] != seen[3];
    if (good != 4 || !alternates)
        fail(name, "calls01=%ld calls23=%ld", (long)(seen[0] * 10 + seen[1]),
             (long)(seen[2] * 10 + seen[3]));
    else ok(name);
    close(ep);
}

static void check_epoll_lt_rotation(void) {
    /* Two expired level-triggered timerfds: neither may starve the other. */
    int a = armed_tfd(CLOCK_MONOTONIC, 0, 1 * MS, 0, 0);
    int b = armed_tfd(CLOCK_MONOTONIC, 0, 1 * MS, 0, 0);
    check_epoll_lt_alternation("epoll_lt_rotation", a, b, 0);
    check_epoll_lt_alternation("epoll_lt_rotation_across_mod", a, b, 1);
    close(a);
    close(b);
}

static void check_epoll_lt_host_fairness(void) {
    /* An always-readable pipe (its byte is never read) and an expired
     * level-triggered timerfd: neither may starve the other, whichever
     * was registered first. */
    int p[2];
    if (pipe(p) != 0 || write(p[1], "x", 1) != 1) {
        fail("epoll_lt_host_fairness", "pipe errno=%ld%ld", errno, 0);
        return;
    }
    int tfd = armed_tfd(CLOCK_MONOTONIC, 0, 1 * MS, 0, 0);
    check_epoll_lt_alternation("epoll_lt_host_fairness", tfd, p[0], 0);
    check_epoll_lt_alternation("epoll_lt_host_fairness_pipe_first", p[0], tfd, 0);
    close(tfd);
    close(p[0]);
    close(p[1]);
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

/* The fork cases below wait for an expiry with poll rather than a sleep, so
 * they hold whether the timer runs on the guest's clock or the host's. */
static void wait_readable(int fd) {
    struct pollfd pfd = {.fd = fd, .events = POLLIN};
    while (poll(&pfd, 1, -1) < 0 && errno == EINTR) {
    }
}

static void check_fork_expired(void) {
    const char *name = "fork_expired";
    int tfd = armed_tfd(CLOCK_MONOTONIC, TFD_NONBLOCK, 1 * MS, 0, 0);
    wait_readable(tfd);
    pid_t pid = fork();
    if (pid == 0) {
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

/* The child re-arms a distant timer to fire in 10 ms, or disarms one due in
 * 30 ms; either way the parent's timer is the one that changed. */
static void check_fork_rearm(int disarm) {
    const char *name = disarm ? "fork_disarm" : "fork_rearm";
    int tfd = armed_tfd(CLOCK_MONOTONIC, TFD_NONBLOCK, (disarm ? 30 : 100000) * MS, 0, 0);
    pid_t pid = fork();
    if (pid == 0) {
        struct itimerspec its;
        memset(&its, 0, sizeof its);
        if (!disarm) its.it_value = ns_ts(10 * MS);
        _exit(timerfd_settime(tfd, 0, &its, NULL) == 0 ? 0 : 1);
    }
    int status = 0;
    waitpid(pid, &status, 0);
    struct itimerspec cur;
    memset(&cur, 0xff, sizeof cur);
    int g = timerfd_gettime(tfd, &cur);
    int64_t left = (int64_t)cur.it_value.tv_sec * 1000000000LL + cur.it_value.tv_nsec;
    int64_t interval = (int64_t)cur.it_interval.tv_sec * 1000000000LL + cur.it_interval.tv_nsec;
    uint64_t count = 0;
    ssize_t r;
    int err;
    if (disarm) {
        /* Long enough for the original arming to have fired. */
        sleep_ns(60 * MS);
    } else {
        wait_readable(tfd);
    }
    errno = 0;
    r = read(tfd, &count, sizeof count);
    err = errno;
    close(tfd);
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0)
        fail(name, "child_status=%ld%ld", (long)status, 0);
    else if (g != 0 || interval != 0) fail(name, "gettime=%ld interval_ns=%ld", g, (long)interval);
    else if (disarm ? left != 0 : (left < 0 || left > 10 * MS))
        fail(name, "left_ns=%ld%ld", (long)left, 0);
    else if (disarm && (r != -1 || err != EAGAIN))
        fail(name, "read=%ld errno=%ld", (long)r, err);
    else if (!disarm && (r != 8 || count != 1))
        fail(name, "read=%ld count=%ld", (long)r, (long)count);
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

/* Closes the timerfd another thread is blocked reading. With reuse it then
 * creates a nonblocking timerfd, which takes the freed number, and lets that
 * one expire once while the read still waits. */
struct close_job {
    int tfd;
    int reuse;
    int new_fd; /* out: the replacement, or -1 */
};

static void *close_later(void *arg) {
    struct close_job *job = arg;
    sleep_ns(20 * MS);
    close(job->tfd);
    job->new_fd = -1;
    if (job->reuse) {
        job->new_fd = armed_tfd(CLOCK_MONOTONIC, TFD_NONBLOCK, 5 * MS, 0, 0);
        sleep_ns(20 * MS);
    }
    return NULL;
}

/* A blocked read holds the open file, not the descriptor number: Linux keeps
 * reading the timer it started on after another thread closes the number,
 * and after a new timerfd takes it. */
static void check_read_across_close(const char *name, int vectored, int reuse) {
    int tfd = armed_tfd(CLOCK_MONOTONIC, 0, 80 * MS, 0, 0);
    struct close_job job = {tfd, reuse, -1};
    pthread_t thread;
    pthread_create(&thread, NULL, close_later, &job);
    uint64_t count = 0;
    struct iovec whole = {&count, sizeof count};
    errno = 0;
    ssize_t r = vectored ? readv(tfd, &whole, 1) : read(tfd, &count, sizeof count);
    int err = errno;
    pthread_join(thread, NULL);
    uint64_t left = 0;
    ssize_t again = -2;
    if (reuse && job.new_fd == tfd) again = read(job.new_fd, &left, sizeof left);
    if (job.new_fd >= 0) close(job.new_fd);
    if (r != 8 || count != 1)
        fail(name, "r=%ld errno=%ld", (long)r, r == 8 ? (long)count : err);
    else if (reuse && (again != 8 || left != 1))
        fail(name, "replacement_r=%ld left=%ld", (long)again, (long)left);
    else ok(name);
}

/* vfs_readv returns 0 for a zero total before it reaches the timer, so a
 * vector read of no bytes neither fails, nor consumes, nor waits. A scalar
 * read of 0 bytes does reach timerfd_read_iter, which refuses it. */
static void check_zero_length_reads(void) {
    unsigned char byte = 0;
    struct iovec zeros[2] = {{&byte, 0}, {&byte, 0}};
    const char *names[3] = {"readv_empty", "readv_zero_lengths", "preadv2_zero_length"};
    for (int i = 0; i < 3; i++) {
        int tfd = expired_tfd();
        errno = 0;
        ssize_t r = i == 0   ? readv(tfd, zeros, 0)
                    : i == 1 ? readv(tfd, zeros, 2)
                             : preadv2(tfd, zeros, 1, -1, 0);
        int err = errno;
        uint64_t left = 0;
        ssize_t again = read(tfd, &left, sizeof left);
        close(tfd);
        if (r != 0) fail(names[i], "r=%ld errno=%ld", (long)r, err);
        else if (again != 8 || left != 1)
            fail(names[i], "again=%ld left=%ld", (long)again, (long)left);
        else ok(names[i]);
    }

    /* A blocking timer due in 200 ms is still pending after the empty read. */
    int tfd = armed_tfd(CLOCK_MONOTONIC, 0, 200 * MS, 0, 0);
    errno = 0;
    ssize_t r = readv(tfd, zeros, 2);
    int err = errno;
    struct itimerspec cur;
    memset(&cur, 0, sizeof cur);
    long got = timerfd_gettime(tfd, &cur);
    close(tfd);
    if (r != 0) fail("readv_zero_blocking", "r=%ld errno=%ld", (long)r, err);
    else if (got != 0 || (cur.it_value.tv_sec == 0 && cur.it_value.tv_nsec == 0))
        fail("readv_zero_blocking", "gettime=%ld pending=%ld", got, 0);
    else ok("readv_zero_blocking");

    tfd = expired_tfd();
    errno = 0;
    r = read(tfd, &byte, 0);
    err = errno;
    uint64_t left = 0;
    ssize_t again = read(tfd, &left, sizeof left);
    close(tfd);
    if (r != -1 || err != EINVAL) fail("read_zero_einval", "r=%ld errno=%ld", (long)r, err);
    else if (again != 8 || left != 1)
        fail("read_zero_einval", "again=%ld left=%ld", (long)again, (long)left);
    else ok("read_zero_einval");
}

/* Zero the read-only page, which needs write access for a moment. */
static void clear_readonly(unsigned char *ro, long pg) {
    mprotect(ro, pg, PROT_READ | PROT_WRITE);
    memset(ro, 0, pg);
    mprotect(ro, pg, PROT_READ);
}

/* The first written byte at the head of the read-only page, or -1. */
static long readonly_dirty(const unsigned char *ro) {
    for (long b = 0; b < 32; b++)
        if (ro[b] != 0) return b;
    return -1;
}

/* Results are copied out with the caller's access rights, as copy_to_iter and
 * copy_to_user do: a read-only page faults instead of being written, the bytes
 * before a fault stay copied, and a read loses its expirations either way. */
static void check_readonly_buffers(void) {
    long pg = sysconf(_SC_PAGESIZE);
    unsigned char *map =
        mmap(NULL, 2 * pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (map == MAP_FAILED) {
        fail("readonly_setup", "errno=%ld pg=%ld", errno, pg);
        return;
    }
    unsigned char *ro = map + pg;
    const char *names[4] = {"read_readonly", "readv_readonly", "read_straddle_readonly",
                            "readv_readonly_partial"};
    for (int i = 0; i < 4; i++) {
        memset(map, 0xff, pg);
        clear_readonly(ro, pg);
        int tfd = expired_tfd();
        struct iovec v[2] = {{ro, 8}, {ro, 4}};
        ssize_t want = i < 2 ? -1 : 4;
        errno = 0;
        ssize_t r;
        if (i == 0) r = read(tfd, ro, 8);
        else if (i == 1) r = readv(tfd, v, 1);
        else if (i == 2) r = read(tfd, ro - 4, 8);
        else {
            v[0].iov_base = map;
            v[0].iov_len = 4;
            r = readv(tfd, v, 2);
        }
        int err = errno;
        uint64_t count = 0;
        errno = 0;
        ssize_t again = read(tfd, &count, sizeof count);
        int again_err = errno;
        close(tfd);
        uint32_t low = 0;
        memcpy(&low, i == 2 ? ro - 4 : map, sizeof low);
        if (r != want || (want == -1 && err != EFAULT))
            fail(names[i], "r=%ld errno=%ld", (long)r, err);
        else if (readonly_dirty(ro) >= 0)
            fail(names[i], "readonly_written r=%ld at=%ld", (long)r, readonly_dirty(ro));
        else if (want == 4 && low != 1) fail(names[i], "low=%ld r=%ld", (long)low, (long)r);
        else if (again != -1 || again_err != EAGAIN)
            fail(names[i], "again=%ld errno=%ld", (long)again, again_err);
        else ok(names[i]);
    }

    clear_readonly(ro, pg);
    int tfd = armed_tfd(CLOCK_MONOTONIC, 0, 10000 * MS, 0, 0);
    errno = 0;
    long r = syscall(SYS_timerfd_gettime, tfd, ro);
    int err = errno;
    close(tfd);
    if (r != -1 || err != EFAULT) fail("gettime_readonly", "r=%ld errno=%ld", r, err);
    else if (readonly_dirty(ro) >= 0)
        fail("gettime_readonly", "readonly_written r=%ld at=%ld", r, readonly_dirty(ro));
    else ok("gettime_readonly");

    clear_readonly(ro, pg);
    tfd = timerfd_create(CLOCK_MONOTONIC, 0);
    struct itimerspec its;
    memset(&its, 0, sizeof its);
    its.it_value.tv_sec = 10;
    errno = 0;
    r = syscall(SYS_timerfd_settime, tfd, 0, &its, ro);
    err = errno;
    struct itimerspec cur;
    memset(&cur, 0, sizeof cur);
    timerfd_gettime(tfd, &cur);
    close(tfd);
    /* The new arming is in effect before the old value is copied out. */
    if (r != -1 || err != EFAULT) fail("settime_old_readonly", "r=%ld errno=%ld", r, err);
    else if (readonly_dirty(ro) >= 0)
        fail("settime_old_readonly", "readonly_written r=%ld at=%ld", r, readonly_dirty(ro));
    else if (cur.it_value.tv_sec < 5)
        fail("settime_old_readonly", "value_sec=%ld nsec=%ld", (long)cur.it_value.tv_sec,
             (long)cur.it_value.tv_nsec);
    else ok("settime_old_readonly");
    munmap(map, 2 * pg);
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

/* The reference for expiries Linux clamps: a CLOCK_MONOTONIC timer armed at
 * the absolute time KTIME_MAX. Returns -1 if it cannot be armed. */
static int ktime_max_reference(void) {
    int ref = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK);
    if (ref < 0) return -1;
    struct itimerspec its;
    memset(&its, 0, sizeof its);
    its.it_value.tv_sec = 9223372036L;
    its.it_value.tv_nsec = 854775807L;
    if (timerfd_settime(ref, TFD_TIMER_ABSTIME, &its, NULL) != 0) {
        close(ref);
        return -1;
    }
    return ref;
}

/* Time left on a timerfd in nanoseconds, or -1 if timerfd_gettime fails. */
static int64_t time_left_ns(int tfd) {
    struct itimerspec cur;
    memset(&cur, 0, sizeof cur);
    if (timerfd_gettime(tfd, &cur) != 0) return -1;
    return (int64_t)cur.it_value.tv_sec * 1000000000LL + cur.it_value.tv_nsec;
}

static void check_huge_interval(const char *name, long interval_sec) {
    int ref = ktime_max_reference();
    int tfd = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK);
    struct itimerspec its;
    memset(&its, 0, sizeof its);
    its.it_value.tv_nsec = 1000;
    its.it_interval.tv_sec = interval_sec;
    long set = timerfd_settime(tfd, 0, &its, NULL);
    sleep_ns(5 * MS);
    int64_t ref_before = time_left_ns(ref);
    struct itimerspec cur;
    memset(&cur, 0, sizeof cur);
    long got = timerfd_gettime(tfd, &cur);
    int64_t ref_mid = time_left_ns(ref);
    uint64_t count = 0;
    ssize_t r = read(tfd, &count, sizeof count);
    struct itimerspec after;
    memset(&after, 0, sizeof after);
    long got_after = timerfd_gettime(tfd, &after);
    int64_t ref_after = time_left_ns(ref);
    close(tfd);
    if (ref >= 0) close(ref);
    int64_t cur_ns = (int64_t)cur.it_value.tv_sec * 1000000000LL + cur.it_value.tv_nsec;
    int64_t after_ns = (int64_t)after.it_value.tv_sec * 1000000000LL + after.it_value.tv_nsec;
    /* timespec64_to_ktime clamps the interval to KTIME_MAX, and hrtimer_forward
     * adds it with ktime_add_safe, which clamps the next expiry to KTIME_MAX:
     * it must neither wrap nor pass KTIME_MAX. */
    if (set != 0 || got != 0 || got_after != 0)
        fail(name, "settime=%ld gettime=%ld", set, got != 0 ? got : got_after);
    else if (ref < 0 || ref_before < 0 || ref_mid < 0 || ref_after < 0)
        fail(name, "reference=%ld gettime=%ld", (long)ref, (long)ref_after);
    else if (cur.it_interval.tv_sec != 9223372036L || cur.it_interval.tv_nsec != 854775807L)
        fail(name, "interval_sec=%ld nsec=%ld", (long)cur.it_interval.tv_sec,
             (long)cur.it_interval.tv_nsec);
    else if (r != 8 || count != 1) fail(name, "r=%ld count=%ld", (long)r, (long)count);
    else if (cur.it_value.tv_sec < 1000000000L || after.it_value.tv_sec < 1000000000L)
        fail(name, "value_sec=%ld after_sec=%ld", (long)cur.it_value.tv_sec,
             (long)after.it_value.tv_sec);
    else if (cur_ns > ref_before || cur_ns < ref_mid)
        fail(name, "left_ns=%ld reference_ns=%ld", (long)cur_ns, (long)ref_before);
    else if (after_ns > ref_mid || after_ns < ref_after)
        fail(name, "after_ns=%ld reference_ns=%ld", (long)after_ns, (long)ref_mid);
    else ok(name);
}

static void check_huge_relative(const char *name, long value_sec, long value_nsec) {
    int ref = ktime_max_reference();
    int tfd = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK);
    struct itimerspec its;
    memset(&its, 0, sizeof its);
    its.it_value.tv_sec = value_sec;
    its.it_value.tv_nsec = value_nsec;
    long set = timerfd_settime(tfd, 0, &its, NULL);
    int64_t ref_before = time_left_ns(ref);
    int64_t left = time_left_ns(tfd);
    int64_t ref_after = time_left_ns(ref);
    uint64_t count = 0;
    errno = 0;
    ssize_t r = read(tfd, &count, sizeof count);
    long read_err = r < 0 ? errno : 0;
    close(tfd);
    if (ref >= 0) close(ref);
    /* hrtimer_start adds the current time with ktime_add_safe, which clamps an
     * expiry beyond KTIME_MAX to KTIME_MAX: the absolute reference's instant. */
    if (set != 0 || left < 0) fail(name, "settime=%ld gettime=%ld", set, (long)left);
    else if (ref < 0 || ref_before < 0 || ref_after < 0)
        fail(name, "reference=%ld gettime=%ld", (long)ref, (long)ref_after);
    else if (left > ref_before || left < ref_after)
        fail(name, "left_ns=%ld reference_ns=%ld", (long)left, (long)ref_before);
    else if (r != -1 || read_err != EAGAIN) fail(name, "read=%ld errno=%ld", (long)r, read_err);
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

static void check_ppoll_mixed(void) {
    const char *name = "ppoll_mixed";
    int p[2];
    if (ready_pipe(p) != 0) { fail(name, "pipe errno=%ld%ld", errno, 0); return; }
    int tfd = armed_tfd(CLOCK_MONOTONIC, 0, 1 * MS, 0, 0);
    wait_readable(tfd);
    struct pollfd fds[2] = {{p[0], POLLIN, 0}, {tfd, POLLIN, 0}};
    struct timespec zero = {0, 0};
    int n = ppoll(fds, 2, &zero, NULL);
    int bits = (fds[0].revents == POLLIN ? 1 : 0) + (fds[1].revents == POLLIN ? 2 : 0);
    if (n != 2 || bits != 3) fail(name, "n=%ld bits=%ld", n, bits);
    else ok(name);
    close(p[0]);
    close(p[1]);
    close(tfd);
}

/* A timer armed 20 ms ahead ends a ppoll with a 5 s limit, or none. */
static void check_ppoll_ready(int infinite) {
    const char *name = infinite ? "ppoll_ready_infinite" : "ppoll_ready";
    int tfd = armed_tfd(CLOCK_MONOTONIC, 0, 20 * MS, 0, 0);
    struct pollfd pfd = {.fd = tfd, .events = POLLIN};
    struct timespec limit = {5, 0};
    int n = ppoll(&pfd, 1, infinite ? NULL : &limit, NULL);
    uint64_t count = 0;
    ssize_t r = n == 1 ? read(tfd, &count, sizeof count) : -1;
    close(tfd);
    if (n != 1 || pfd.revents != POLLIN) fail(name, "n=%ld revents=%ld", n, pfd.revents);
    else if (r != 8 || count != 1) fail(name, "r=%ld count=%ld", (long)r, (long)count);
    else ok(name);
}

/* A timer disarmed before it fires leaves a 100 ms ppoll to time out. */
static void check_ppoll_disarmed(void) {
    const char *name = "ppoll_disarmed";
    int tfd = armed_tfd(CLOCK_MONOTONIC, 0, 20 * MS, 0, 0);
    struct itimerspec off;
    memset(&off, 0, sizeof off);
    int s = timerfd_settime(tfd, 0, &off, NULL);
    struct pollfd pfd = {.fd = tfd, .events = POLLIN};
    struct timespec limit = {0, 100 * MS};
    int n = ppoll(&pfd, 1, &limit, NULL);
    close(tfd);
    if (s != 0) fail(name, "settime=%ld errno=%ld", s, errno);
    else if (n != 0 || pfd.revents != 0) fail(name, "n=%ld revents=%ld", n, pfd.revents);
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

/*
 * Wait forms beyond epoll_wait, epoll_pwait and a select within FD_SETSIZE.
 *   wait_form_epoll_pwait2_zero / wait_form_epoll_pwait2_finite
 *       epoll_pwait2 without a mask reports an expired timerfd with a zero
 *       and with a finite timespec.
 *   wait_form_epoll_pwait2_infinite_block / wait_form_epoll_pwait2_finite_block /
 *   wait_form_epoll_pwait2_timeout
 *       It blocks until a timerfd armed for later fires, with a NULL timespec
 *       and with one longer than the timer, and returns 0 when a timespec
 *       shorter than the timer runs out.
 *   wait_form_epoll_pwait2_masked_ready
 *       With a signal mask it returns a ready pipe beside a distant timerfd,
 *       and an expired timerfd, without blocking.
 *   wait_form_epoll_pwait2_errors
 *       Linux's argument-checking order: the timespec pointer, then its
 *       value, then the mask size and pointer, then maxevents; a NULL mask
 *       ignores its size; a timespec in a write-only page, which Linux can
 *       read, is accepted.
 *   wait_form_select_wide_ready / wait_form_pselect6_wide_ready
 *       select and pselect6 with nfds above FD_SETSIZE, whose read set names
 *       an expired and a distant timerfd above fd 1100, report only the
 *       expired one without blocking. select's write set also reports a
 *       writable pipe and its empty except set stays empty; pselect6 passes
 *       only a read set and a signal mask. A word past nfds is untouched.
 *   wait_form_select_wide_block / wait_form_pselect6_wide_block
 *       They block until a timerfd armed for later fires, select with a NULL
 *       timeout and pselect6 with one longer than the timer.
 *   wait_form_select_wide_timeout / wait_form_pselect6_wide_timeout
 *       They return 0, with the read set cleared and no time left, when a
 *       timeout shorter than the timer runs out.
 */
#include <sys/mman.h>
#include <sys/resource.h>

static long wait_form_pwait2(int ep, struct epoll_event *out, int maxevents, const void *ts,
                             const void *mask, size_t sigsetsize) {
    return syscall(SYS_epoll_pwait2, ep, out, maxevents, ts, mask, sigsetsize);
}

static void wait_form_epoll_pwait2_ready(int finite) {
    const char *name = finite ? "wait_form_epoll_pwait2_finite" : "wait_form_epoll_pwait2_zero";
    int tfd = expired_tfd();
    int ep = epoll_with(tfd, EPOLLIN, 11);
    struct timespec ts = {finite ? 1 : 0, 0};
    struct epoll_event out[4];
    memset(out, 0, sizeof out);
    errno = 0;
    long n = wait_form_pwait2(ep, out, 4, &ts, NULL, 0);
    long err = errno;
    close(ep);
    close(tfd);
    if (n != 1 || out[0].data.u64 != 11 || out[0].events != EPOLLIN)
        fail(name, "n=%ld errno=%ld", n, err);
    else ok(name);
}

static void wait_form_epoll_pwait2_block(int finite) {
    const char *name =
        finite ? "wait_form_epoll_pwait2_finite_block" : "wait_form_epoll_pwait2_infinite_block";
    int64_t start = now_ns(CLOCK_MONOTONIC);
    int tfd = armed_tfd(CLOCK_MONOTONIC, 0, 30 * MS, 0, 0);
    int ep = epoll_with(tfd, EPOLLIN, 12);
    struct timespec ts = {5, 0};
    struct epoll_event out[4];
    memset(out, 0, sizeof out);
    errno = 0;
    long n = wait_form_pwait2(ep, out, 4, finite ? &ts : NULL, NULL, 0);
    long err = errno;
    int64_t elapsed = now_ns(CLOCK_MONOTONIC) - start;
    close(ep);
    close(tfd);
    if (n != 1 || out[0].data.u64 != 12) fail(name, "n=%ld errno=%ld", n, err);
    else if (elapsed < 30 * MS || elapsed > 4000 * MS)
        fail(name, "elapsed_ms=%ld%ld", (long)(elapsed / MS), 0);
    else ok(name);
}

static void wait_form_epoll_pwait2_timeout(void) {
    const char *name = "wait_form_epoll_pwait2_timeout";
    int tfd = armed_tfd(CLOCK_MONOTONIC, 0, 100000 * MS, 0, 0);
    int ep = epoll_with(tfd, EPOLLIN, 13);
    struct timespec ts = {0, 20 * MS};
    struct epoll_event out[4];
    int64_t start = now_ns(CLOCK_MONOTONIC);
    errno = 0;
    long n = wait_form_pwait2(ep, out, 4, &ts, NULL, 0);
    long err = errno;
    int64_t elapsed = now_ns(CLOCK_MONOTONIC) - start;
    close(ep);
    close(tfd);
    if (n != 0) fail(name, "n=%ld errno=%ld", n, err);
    else if (elapsed < 20 * MS || elapsed > 4000 * MS)
        fail(name, "elapsed_ms=%ld%ld", (long)(elapsed / MS), 0);
    else ok(name);
}

static void wait_form_epoll_pwait2_masked_ready(void) {
    const char *name = "wait_form_epoll_pwait2_masked_ready";
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
    struct timespec ts = {1, 0};
    struct epoll_event out[4];
    memset(out, 0, sizeof out);
    long host = wait_form_pwait2(ep, out, 4, &ts, &mask, 8);
    long host_data = host > 0 ? (long)out[0].data.u64 : 0;
    close(ep);
    close(far);
    close(p[0]);
    close(p[1]);
    int tfd = expired_tfd();
    ep = epoll_with(tfd, EPOLLIN, 3);
    memset(out, 0, sizeof out);
    long timer = wait_form_pwait2(ep, out, 4, &ts, &mask, 8);
    long timer_data = timer > 0 ? (long)out[0].data.u64 : 0;
    close(ep);
    close(tfd);
    if (host != 1 || host_data != 2) fail(name, "pipe n=%ld data=%ld", host, host_data);
    else if (timer != 1 || timer_data != 3) fail(name, "timer n=%ld data=%ld", timer, timer_data);
    else ok(name);
}

static void wait_form_epoll_pwait2_errors(void) {
    const char *name = "wait_form_epoll_pwait2_errors";
    int tfd = expired_tfd();
    int ep = epoll_with(tfd, EPOLLIN, 14);
    struct timespec *write_only =
        mmap(NULL, 4096, PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (write_only == MAP_FAILED) {
        fail(name, "mmap errno=%ld%ld", errno, 0);
        return;
    }
    write_only->tv_sec = 0;
    write_only->tv_nsec = 0;
    void *bad = (void *)1;
    sigset_t mask;
    sigemptyset(&mask);
    struct timespec zero = {0, 0};
    struct timespec big_nsec = {0, 1000000000L};
    struct timespec neg_nsec = {0, -1};
    struct timespec neg_sec = {-1, 0};
    /* want is the errno, or 0 for the one expired timer. */
    struct { const void *ts; const void *mask; size_t size; int maxevents; long want; } cases[] = {
        {bad, &mask, 1, 0, EFAULT},
        {&big_nsec, bad, 8, 0, EINVAL},
        {&neg_nsec, bad, 8, 4, EINVAL},
        {&neg_sec, bad, 8, 4, EINVAL},
        {&zero, bad, 8, 0, EFAULT},
        {&zero, bad, 8, 1, EFAULT},
        {&zero, &mask, 4, 1, EINVAL},
        {&zero, &mask, 4, 4, EINVAL},
        {&zero, NULL, 0, 0, EINVAL},
        {&zero, NULL, 0, -1, EINVAL},
        {&zero, NULL, 1, 4, 0},
        {write_only, NULL, 0, 4, 0},
    };
    long bad_case = -1, got = 0;
    for (int i = 0; i < (int)(sizeof cases / sizeof cases[0]); i++) {
        struct epoll_event out[4];
        memset(out, 0, sizeof out);
        errno = 0;
        long n = wait_form_pwait2(ep, out, cases[i].maxevents, cases[i].ts, cases[i].mask,
                                  cases[i].size);
        got = n < 0 ? errno : (n == 1 && out[0].data.u64 == 14 ? 0 : 1000 + n);
        if (got != cases[i].want) {
            bad_case = i;
            break;
        }
    }
    munmap(write_only, 4096);
    close(ep);
    close(tfd);
    if (bad_case >= 0) fail(name, "case=%ld got=%ld", bad_case, got);
    else ok(name);
}

#define WAIT_FORM_WORDS 32 /* bitmap words: descriptors 0..2047 */
#define WAIT_FORM_HIGH_FD 1100
#define WAIT_FORM_SENTINEL 0xa5a5a5a5a5a5a5a5UL

/* Raise RLIMIT_NOFILE so descriptors above WAIT_FORM_HIGH_FD can exist. */
static int wait_form_nofile(void) {
    struct rlimit rl;
    if (getrlimit(RLIMIT_NOFILE, &rl) != 0) return -1;
    if (rl.rlim_cur >= WAIT_FORM_WORDS * 64) return 0;
    rl.rlim_cur = WAIT_FORM_WORDS * 64;
    return setrlimit(RLIMIT_NOFILE, &rl);
}

/* Move a descriptor above FD_SETSIZE. */
static int wait_form_high_fd(int fd) {
    if (fd < 0) return -1;
    int high = fcntl(fd, F_DUPFD, WAIT_FORM_HIGH_FD);
    close(fd);
    return high;
}

static void wait_form_set(unsigned long *set, int fd) { set[fd / 64] |= 1UL << (fd % 64); }

static long wait_form_isset(const unsigned long *set, int fd) {
    return (long)((set[fd / 64] >> (fd % 64)) & 1);
}

/* Every set bit counts, so a stray bit fails as well as a missing one. */
static long wait_form_count(const unsigned long *set) {
    long count = 0;
    for (int i = 0; i < WAIT_FORM_WORDS; i++) count += __builtin_popcountl(set[i]);
    return count;
}

/* Raw select or pselect6; pselect6 also blocks SIGUSR2 for the wait. A
 * negative timeout passes NULL. *left_ns receives the time not slept. */
static long wait_form_select(int use_pselect, int nfds, unsigned long *rd, unsigned long *wr,
                             unsigned long *ex, int64_t timeout_ns, int64_t *left_ns) {
    sigset_t mask;
    sigemptyset(&mask);
    sigaddset(&mask, SIGUSR2);
    struct { const sigset_t *set; size_t size; } wrapper = {&mask, 8};
    struct timespec ts = ns_ts(timeout_ns < 0 ? 0 : timeout_ns);
    struct timeval tv = {ts.tv_sec, ts.tv_nsec / 1000};
    long n;
    if (use_pselect)
        n = syscall(SYS_pselect6, nfds, rd, wr, ex, timeout_ns < 0 ? NULL : &ts, &wrapper);
    else
        n = syscall(SYS_select, nfds, rd, wr, ex, timeout_ns < 0 ? NULL : &tv);
    *left_ns = use_pselect ? (int64_t)ts.tv_sec * 1000000000LL + ts.tv_nsec
                           : (int64_t)tv.tv_sec * 1000000000LL + tv.tv_usec * 1000LL;
    return n;
}

static void wait_form_wide_ready(int use_pselect) {
    const char *name =
        use_pselect ? "wait_form_pselect6_wide_ready" : "wait_form_select_wide_ready";
    if (wait_form_nofile() != 0) { fail(name, "rlimit errno=%ld%ld", errno, 0); return; }
    int ready = wait_form_high_fd(expired_tfd());
    int far = wait_form_high_fd(armed_tfd(CLOCK_MONOTONIC, 0, 100000 * MS, 0, 0));
    /* select also watches a pipe's write end, which has room to write. */
    int p[2] = {-1, -1};
    int out = -1;
    if (!use_pselect && pipe(p) == 0) out = wait_form_high_fd(p[1]);
    int low = ready < far ? ready : far;
    if (!use_pselect && out < low) low = out;
    if (low < WAIT_FORM_HIGH_FD) {
        fail(name, "low_fd=%ld errno=%ld", (long)low, errno);
        return;
    }
    unsigned long rd[WAIT_FORM_WORDS], wr[WAIT_FORM_WORDS], ex[WAIT_FORM_WORDS];
    memset(rd, 0, sizeof rd);
    memset(wr, 0, sizeof wr);
    memset(ex, 0, sizeof ex);
    wait_form_set(rd, ready);
    wait_form_set(rd, far);
    int nfds = (ready > far ? ready : far) + 1;
    if (!use_pselect) {
        wait_form_set(wr, out);
        if (out + 1 > nfds) nfds = out + 1;
    }
    /* Linux copies only the words that hold descriptors below nfds. */
    rd[WAIT_FORM_WORDS - 1] = WAIT_FORM_SENTINEL;
    int64_t left = 0;
    errno = 0;
    long n = use_pselect ? wait_form_select(1, nfds, rd, NULL, NULL, 1000 * MS, &left)
                         : wait_form_select(0, nfds, rd, wr, ex, 1000 * MS, &left);
    long err = errno;
    long sentinel_kept = rd[WAIT_FORM_WORDS - 1] == WAIT_FORM_SENTINEL;
    rd[WAIT_FORM_WORDS - 1] = 0;
    close(ready);
    close(far);
    if (!use_pselect) {
        close(p[0]);
        close(out);
    }
    if (n != (use_pselect ? 1 : 2)) fail(name, "n=%ld errno=%ld", n, err);
    else if (!wait_form_isset(rd, ready) || wait_form_count(rd) != 1)
        fail(name, "ready_bit=%ld read_bits=%ld", wait_form_isset(rd, ready), wait_form_count(rd));
    else if (!use_pselect && (!wait_form_isset(wr, out) || wait_form_count(wr) != 1))
        fail(name, "out_bit=%ld write_bits=%ld", wait_form_isset(wr, out), wait_form_count(wr));
    else if (wait_form_count(ex) != 0) fail(name, "except_bits=%ld%ld", wait_form_count(ex), 0);
    else if (!sentinel_kept) fail(name, "sentinel_kept=%ld%ld", sentinel_kept, 0);
    else if (left < 500 * MS || left > 1000 * MS)
        fail(name, "left_ms=%ld%ld", (long)(left / MS), 0);
    else ok(name);
}

static void wait_form_wide_block(int use_pselect) {
    const char *name =
        use_pselect ? "wait_form_pselect6_wide_block" : "wait_form_select_wide_block";
    if (wait_form_nofile() != 0) { fail(name, "rlimit errno=%ld%ld", errno, 0); return; }
    int64_t start = now_ns(CLOCK_MONOTONIC);
    int soon = wait_form_high_fd(armed_tfd(CLOCK_MONOTONIC, 0, 30 * MS, 0, 0));
    int far = wait_form_high_fd(armed_tfd(CLOCK_MONOTONIC, 0, 100000 * MS, 0, 0));
    if (soon < WAIT_FORM_HIGH_FD || far < WAIT_FORM_HIGH_FD) {
        fail(name, "low_fd=%ld errno=%ld", (long)(soon < far ? soon : far), errno);
        return;
    }
    unsigned long rd[WAIT_FORM_WORDS];
    memset(rd, 0, sizeof rd);
    wait_form_set(rd, soon);
    wait_form_set(rd, far);
    int nfds = (soon > far ? soon : far) + 1;
    int64_t left = 0;
    errno = 0;
    long n = wait_form_select(use_pselect, nfds, rd, NULL, NULL, use_pselect ? 5000 * MS : -1,
                              &left);
    long err = errno;
    int64_t elapsed = now_ns(CLOCK_MONOTONIC) - start;
    close(soon);
    close(far);
    if (n != 1) fail(name, "n=%ld errno=%ld", n, err);
    else if (!wait_form_isset(rd, soon) || wait_form_count(rd) != 1)
        fail(name, "soon_bit=%ld read_bits=%ld", wait_form_isset(rd, soon), wait_form_count(rd));
    else if (elapsed < 30 * MS || elapsed > 4000 * MS)
        fail(name, "elapsed_ms=%ld%ld", (long)(elapsed / MS), 0);
    else ok(name);
}

static void wait_form_wide_timeout(int use_pselect) {
    const char *name =
        use_pselect ? "wait_form_pselect6_wide_timeout" : "wait_form_select_wide_timeout";
    if (wait_form_nofile() != 0) { fail(name, "rlimit errno=%ld%ld", errno, 0); return; }
    int far = wait_form_high_fd(armed_tfd(CLOCK_MONOTONIC, 0, 100000 * MS, 0, 0));
    if (far < WAIT_FORM_HIGH_FD) { fail(name, "fd=%ld errno=%ld", (long)far, errno); return; }
    unsigned long rd[WAIT_FORM_WORDS];
    memset(rd, 0, sizeof rd);
    wait_form_set(rd, far);
    int64_t left = -1;
    int64_t start = now_ns(CLOCK_MONOTONIC);
    errno = 0;
    long n = wait_form_select(use_pselect, far + 1, rd, NULL, NULL, 20 * MS, &left);
    long err = errno;
    int64_t elapsed = now_ns(CLOCK_MONOTONIC) - start;
    close(far);
    if (n != 0) fail(name, "n=%ld errno=%ld", n, err);
    else if (wait_form_count(rd) != 0) fail(name, "read_bits=%ld%ld", wait_form_count(rd), 0);
    else if (left != 0) fail(name, "left_us=%ld%ld", (long)(left / 1000), 0);
    else if (elapsed < 20 * MS || elapsed > 4000 * MS)
        fail(name, "elapsed_ms=%ld%ld", (long)(elapsed / MS), 0);
    else ok(name);
}

/* T8: a timerfd behind a nested epoll. The outer epoll, poll or select asks
 * the kernel about the inner epoll, so the inner epoll's timerfd must be
 * visible there, as on Linux. */
static int wait_form_watch(int epfd, int fd, uint64_t data) {
    struct epoll_event ev;
    memset(&ev, 0, sizeof ev);
    ev.events = EPOLLIN;
    ev.data.u64 = data;
    return epoll_ctl(epfd, EPOLL_CTL_ADD, fd, &ev);
}

/* After a nested wait reported the timer: the inner epoll reports it too, a
 * read takes one expiration and quiets both epolls, and the timer can be
 * re-armed and disarmed. Returns 0, or fails `name` and returns -1. */
static int wait_form_nested_after(const char *name, int tfd, int inner, int outer) {
    struct epoll_event ev[4];
    memset(ev, 0, sizeof ev);
    int n = epoll_wait(inner, ev, 4, 0);
    if (n != 1 || ev[0].data.u64 != 7) {
        fail(name, "inner_n=%ld inner_data=%ld", n, n > 0 ? (long)ev[0].data.u64 : 0);
        return -1;
    }
    uint64_t count = 0;
    ssize_t r = read(tfd, &count, sizeof count);
    if (r != 8 || count != 1) {
        fail(name, "read=%ld count=%ld", (long)r, (long)count);
        return -1;
    }
    n = epoll_wait(inner, ev, 4, 0);
    long outer_n = outer >= 0 ? epoll_wait(outer, ev, 4, 0) : 0;
    if (n != 0 || outer_n != 0) {
        fail(name, "after_read inner_n=%ld outer_n=%ld", n, outer_n);
        return -1;
    }
    struct itimerspec its, cur;
    memset(&its, 0, sizeof its);
    its.it_value = ns_ts(10000 * MS);
    if (timerfd_settime(tfd, 0, &its, NULL) != 0 || timerfd_gettime(tfd, &cur) != 0) {
        fail(name, "rearm errno=%ld%ld", errno, 0);
        return -1;
    }
    int64_t left = (int64_t)cur.it_value.tv_sec * 1000000000LL + cur.it_value.tv_nsec;
    if (left <= 5000 * MS || left > 10000 * MS) {
        fail(name, "rearm_left_ms=%ld%ld", (long)(left / MS), 0);
        return -1;
    }
    memset(&its, 0, sizeof its);
    if (timerfd_settime(tfd, 0, &its, NULL) != 0 || timerfd_gettime(tfd, &cur) != 0) {
        fail(name, "disarm errno=%ld%ld", errno, 0);
        return -1;
    }
    if (cur.it_value.tv_sec != 0 || cur.it_value.tv_nsec != 0) {
        fail(name, "disarm_left_ns=%ld%ld", (long)cur.it_value.tv_nsec, 0);
        return -1;
    }
    return 0;
}

/* An outer epoll watches an inner epoll that watches a timerfd. The timer
 * expires before the outer epoll first watches the inner one, or before it
 * is added to the inner epoll that the outer one already watches. */
static void wait_form_epoll_nested(int add_later) {
    const char *name =
        add_later ? "wait_form_epoll_nested_add_later" : "wait_form_epoll_nested_expired";
    int tfd = armed_tfd(CLOCK_MONOTONIC, TFD_NONBLOCK, 1 * MS, 0, 0);
    int inner = epoll_create1(0);
    int outer = epoll_create1(0);
    int rc = tfd < 0 || inner < 0 || outer < 0 ? -1 : 0;
    if (rc == 0 && add_later) {
        rc = wait_form_watch(outer, inner, 0x1234);
        sleep_ns(5 * MS);
        if (rc == 0) rc = wait_form_watch(inner, tfd, 7);
    } else if (rc == 0) {
        rc = wait_form_watch(inner, tfd, 7);
        sleep_ns(5 * MS);
        if (rc == 0) rc = wait_form_watch(outer, inner, 0x1234);
    }
    struct epoll_event ev[4];
    memset(ev, 0, sizeof ev);
    errno = 0;
    long n = rc == 0 ? epoll_wait(outer, ev, 4, 0) : -1;
    if (rc != 0) fail(name, "setup errno=%ld%ld", errno, 0);
    else if (n != 1 || ev[0].events != EPOLLIN || ev[0].data.u64 != 0x1234)
        fail(name, "n=%ld data=%ld", n, n > 0 ? (long)ev[0].data.u64 : 0);
    else if (wait_form_nested_after(name, tfd, inner, outer) == 0) ok(name);
    if (outer >= 0) close(outer);
    if (inner >= 0) close(inner);
    if (tfd >= 0) close(tfd);
}

/* poll, ppoll, select and pselect6 on an epoll that watches an expired
 * timerfd. */
static void wait_form_wait_on_epoll(int form) {
    static const char *names[] = {"wait_form_poll_on_epoll", "wait_form_ppoll_on_epoll",
                                  "wait_form_select_on_epoll", "wait_form_pselect6_on_epoll"};
    const char *name = names[form];
    int tfd = armed_tfd(CLOCK_MONOTONIC, TFD_NONBLOCK, 1 * MS, 0, 0);
    int inner = epoll_create1(0);
    if (tfd < 0 || inner < 0 || wait_form_watch(inner, tfd, 7) != 0) {
        fail(name, "setup errno=%ld%ld", errno, 0);
        if (inner >= 0) close(inner);
        if (tfd >= 0) close(tfd);
        return;
    }
    sleep_ns(5 * MS);
    struct pollfd p = {inner, POLLIN, 0};
    struct timespec second = ns_ts(1000 * MS);
    struct timeval zero = {0, 0};
    fd_set rd;
    FD_ZERO(&rd);
    FD_SET(inner, &rd);
    long n, ready;
    errno = 0;
    if (form == 0) n = poll(&p, 1, 0);
    else if (form == 1) n = ppoll(&p, 1, &second, NULL);
    else if (form == 2) n = select(inner + 1, &rd, NULL, NULL, &zero);
    else n = pselect(inner + 1, &rd, NULL, NULL, &second, NULL);
    ready = form < 2 ? p.revents == POLLIN : FD_ISSET(inner, &rd) != 0;
    if (n != 1 || !ready) fail(name, "n=%ld ready=%ld", n, ready);
    else if (wait_form_nested_after(name, tfd, inner, -1) == 0) ok(name);
    close(inner);
    close(tfd);
}

/* epoll_pwait and epoll_pwait2 with a signal mask block until a timerfd
 * armed for later fires, and epoll_pwait with a mask times out on a distant
 * one, leaving it armed. */
static void wait_form_masked_wait(int form) {
    static const char *names[] = {"wait_form_masked_epoll_pwait_block",
                                  "wait_form_masked_epoll_pwait2_block",
                                  "wait_form_masked_epoll_pwait_timeout"};
    const char *name = names[form];
    int tfd = armed_tfd(CLOCK_MONOTONIC, TFD_NONBLOCK, (form == 2 ? 100000 : 30) * MS, 0, 0);
    int ep = tfd < 0 ? -1 : epoll_with(tfd, EPOLLIN, 9);
    sigset_t mask;
    sigemptyset(&mask);
    sigaddset(&mask, SIGUSR2);
    struct timespec ts = {5, 0};
    struct epoll_event out[4];
    memset(out, 0, sizeof out);
    errno = 0;
    long n = ep < 0       ? -1
             : form == 1 ? wait_form_pwait2(ep, out, 4, &ts, &mask, 8)
                         : epoll_pwait(ep, out, 4, form == 2 ? 20 : 5000, &mask);
    long err = errno;
    uint64_t count = 0;
    ssize_t r = n == 1 ? read(tfd, &count, sizeof count) : 0;
    struct itimerspec cur;
    memset(&cur, 0, sizeof cur);
    int got = tfd >= 0 ? timerfd_gettime(tfd, &cur) : -1;
    if (ep >= 0) close(ep);
    if (tfd >= 0) close(tfd);
    if (form == 2) {
        if (n != 0) fail(name, "n=%ld errno=%ld", n, err);
        else if (got != 0 || cur.it_value.tv_sec < 90)
            fail(name, "got=%ld left_sec=%ld", (long)got, (long)cur.it_value.tv_sec);
        else ok(name);
    } else if (n != 1 || out[0].events != EPOLLIN || out[0].data.u64 != 9)
        fail(name, "n=%ld errno=%ld", n, err);
    else if (r != 8 || count != 1) fail(name, "r=%ld count=%ld", (long)r, (long)count);
    else if (got != 0 || cur.it_value.tv_sec != 0 || cur.it_value.tv_nsec != 0)
        fail(name, "got=%ld left_ns=%ld", (long)got, (long)cur.it_value.tv_nsec);
    else ok(name);
}

/* Sends `fd` over SCM_RIGHTS with one byte of data, through sendmsg or
 * sendmmsg; 0 on success. */
static int send_fd_once(int sock, int fd, int use_sendmmsg) {
    char byte = 'x';
    struct iovec iov = {&byte, 1};
    union {
        struct cmsghdr align;
        char buf[CMSG_SPACE(sizeof(int))];
    } control;
    memset(&control, 0, sizeof control);
    struct mmsghdr message;
    memset(&message, 0, sizeof message);
    message.msg_hdr.msg_iov = &iov;
    message.msg_hdr.msg_iovlen = 1;
    message.msg_hdr.msg_control = control.buf;
    message.msg_hdr.msg_controllen = sizeof control.buf;
    struct cmsghdr *header = CMSG_FIRSTHDR(&message.msg_hdr);
    header->cmsg_level = SOL_SOCKET;
    header->cmsg_type = SCM_RIGHTS;
    header->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(header), &fd, sizeof fd);
    if (use_sendmmsg) return sendmmsg(sock, &message, 1, 0) == 1 ? 0 : -1;
    return sendmsg(sock, &message.msg_hdr, 0) == 1 ? 0 : -1;
}

/* Receives one descriptor sent by send_fd_once; -1 on failure. */
static int recv_fd_once(int sock) {
    char byte = 0;
    struct iovec iov = {&byte, 1};
    union {
        struct cmsghdr align;
        char buf[CMSG_SPACE(sizeof(int))];
    } control;
    memset(&control, 0, sizeof control);
    struct msghdr message;
    memset(&message, 0, sizeof message);
    message.msg_iov = &iov;
    message.msg_iovlen = 1;
    message.msg_control = control.buf;
    message.msg_controllen = sizeof control.buf;
    if (recvmsg(sock, &message, 0) != 1) return -1;
    struct cmsghdr *header = CMSG_FIRSTHDR(&message);
    if (header == NULL || header->cmsg_level != SOL_SOCKET || header->cmsg_type != SCM_RIGHTS)
        return -1;
    int fd;
    memcpy(&fd, CMSG_DATA(header), sizeof fd);
    return fd;
}

/* A timerfd received over SCM_RIGHTS is the sender's open file under a new
 * number, as for a dup: the arming made before the send is what the new
 * number reads back, a re-arming through the new number is what the original
 * reads back, and so is a disarming through the original. */
static void check_scm_rights(int use_sendmmsg) {
    const char *name = use_sendmmsg ? "scm_rights_sendmmsg" : "scm_rights_sendmsg";
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) {
        fail(name, "socketpair errno=%ld step=%ld", errno, 0);
        return;
    }
    int tfd = armed_tfd(CLOCK_MONOTONIC, 0, 100000 * MS, 0, 0);
    int rfd = -1;
    if (tfd < 0 || send_fd_once(sv[0], tfd, use_sendmmsg) != 0 || (rfd = recv_fd_once(sv[1])) < 0) {
        fail(name, "setup errno=%ld rfd=%ld", errno, rfd);
    } else {
        int64_t armed = time_left_ns(rfd);
        struct itimerspec rearm;
        memset(&rearm, 0, sizeof rearm);
        rearm.it_value.tv_sec = 50;
        errno = 0;
        long set = timerfd_settime(rfd, 0, &rearm, NULL);
        long set_errno = errno;
        int64_t rearmed = time_left_ns(tfd);
        struct itimerspec disarm;
        memset(&disarm, 0, sizeof disarm);
        long unset = timerfd_settime(tfd, 0, &disarm, NULL);
        int64_t disarmed = time_left_ns(rfd);
        if (armed <= 0 || armed > 100000 * MS)
            fail(name, "received_left_ms=%ld step=%ld", (long)(armed < 0 ? armed : armed / MS), 1);
        else if (set != 0)
            fail(name, "settime_received=%ld errno=%ld", set, set_errno);
        else if (rearmed <= 0 || rearmed > 50000 * MS)
            fail(name, "original_left_ms=%ld step=%ld", (long)(rearmed < 0 ? rearmed : rearmed / MS), 3);
        else if (unset != 0 || disarmed != 0)
            fail(name, "disarm=%ld received_left_ns=%ld", unset, (long)disarmed);
        else ok(name);
    }
    if (rfd >= 0) close(rfd);
    if (tfd >= 0) close(tfd);
    close(sv[0]);
    close(sv[1]);
}

/* preadv2 at offset -1 with one flag on a timer that expired 20 ms ago.
 * RWF_HIPRI is accepted and ignored by every kernel. Where timerfd reads
 * through read_iter, RWF_NOWAIT acts as O_NONBLOCK and RWF_DSYNC is ignored;
 * older kernels refuse every flag but RWF_HIPRI with EOPNOTSUPP. */
static void check_preadv2_expired(const char *name, int tfd_flags, int rwf) {
    int tfd = armed_tfd(CLOCK_MONOTONIC, tfd_flags, 10 * MS, 0, 0);
    sleep_ns(30 * MS);
    uint64_t count = 0;
    struct iovec whole = {&count, sizeof count};
    errno = 0;
    ssize_t r = preadv2(tfd, &whole, 1, -1, rwf);
    int err = errno;
    close(tfd);
    if (r == 8 && count == 1) ok(name);
    else if (rwf != RWF_HIPRI && r == -1 && err == EOPNOTSUPP) ok(name);
    else fail(name, "r=%ld errno_or_count=%ld", (long)r, r == 8 ? (long)count : err);
}

/* RWF_NOWAIT on a blocking timer due in 100 s does not wait, and the timer
 * stays armed. */
static void check_preadv2_nowait_pending(void) {
    int tfd = armed_tfd(CLOCK_MONOTONIC, 0, 100000 * MS, 0, 0);
    uint64_t count = 0;
    struct iovec whole = {&count, sizeof count};
    errno = 0;
    ssize_t r = preadv2(tfd, &whole, 1, -1, RWF_NOWAIT);
    int err = errno;
    struct itimerspec cur;
    memset(&cur, 0, sizeof cur);
    long got = timerfd_gettime(tfd, &cur);
    close(tfd);
    if (r != -1 || (err != EAGAIN && err != EOPNOTSUPP))
        fail("preadv2_nowait_pending", "r=%ld errno=%ld", (long)r, err);
    else if (got != 0 || cur.it_value.tv_sec < 50)
        fail("preadv2_nowait_pending", "gettime=%ld left_sec=%ld", got, (long)cur.it_value.tv_sec);
    else ok("preadv2_nowait_pending");
}

/* The blocking forms run last: a read that waits on a timer that never fires
 * would stop the program here rather than hide the cases after it. */
static void check_preadv2_flags(void) {
    check_preadv2_expired("preadv2_hipri", TFD_NONBLOCK, RWF_HIPRI);
    check_preadv2_expired("preadv2_nowait", TFD_NONBLOCK, RWF_NOWAIT);
    check_preadv2_expired("preadv2_dsync", TFD_NONBLOCK, RWF_DSYNC);
    check_preadv2_nowait_pending();
    check_preadv2_expired("preadv2_hipri_blocking", 0, RWF_HIPRI);
    check_preadv2_expired("preadv2_dsync_blocking", 0, RWF_DSYNC);
}

/* The fork and ppoll cases that check sharing and readiness only, never the
 * guest's clock; the `sharing` argument runs only these. */
static void check_sharing_cases(void) {
    check_fork_expired();
    check_fork_rearm(0);
    check_fork_rearm(1);
    check_ppoll_mixed();
    check_ppoll_ready(0);
    check_ppoll_disarmed();
    check_ppoll_ready(1);
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (argc > 1 && strcmp(argv[1], "sharing") == 0) {
        check_sharing_cases();
        printf("failures=%d\n", failures);
        return failures == 0 ? 0 : 1;
    }
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
    check_epoll_lt_rotation();
    check_epoll_lt_host_fairness();
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
    check_read_across_close("read_across_close", 0, 0);
    check_read_across_close("read_across_reuse", 0, 1);
    check_read_across_close("readv_across_close", 1, 0);
    check_read_across_close("readv_across_reuse", 1, 1);
    check_zero_length_reads();
    check_readonly_buffers();
    check_create_errors();
    check_gettime_errors();
    check_settime_errors();
    check_fine_periodic_sleep(0);
    check_fine_periodic_sleep(1);
    check_huge_interval("huge_interval", 17000000000L);
    check_huge_interval("max_interval", 0x7fffffffffffffffL);
    check_huge_relative("huge_relative", 17000000000L, 0);
    check_huge_relative("near_max_relative", 9223372035L, 999999999L);
    check_epoll_pwait_masked_ready();
    wait_form_epoll_pwait2_ready(0);
    wait_form_epoll_pwait2_ready(1);
    wait_form_epoll_pwait2_block(0);
    wait_form_epoll_pwait2_block(1);
    wait_form_epoll_pwait2_timeout();
    wait_form_epoll_pwait2_masked_ready();
    wait_form_epoll_pwait2_errors();
    wait_form_wide_ready(0);
    wait_form_wide_ready(1);
    wait_form_wide_block(0);
    wait_form_wide_block(1);
    wait_form_wide_timeout(0);
    wait_form_wide_timeout(1);
    wait_form_epoll_nested(0);
    wait_form_epoll_nested(1);
    wait_form_wait_on_epoll(0);
    wait_form_wait_on_epoll(1);
    wait_form_wait_on_epoll(2);
    wait_form_wait_on_epoll(3);
    wait_form_masked_wait(0);
    wait_form_masked_wait(1);
    wait_form_masked_wait(2);
    check_scm_rights(0);
    check_scm_rights(1);
    check_preadv2_flags();
    check_sharing_cases();
    printf("failures=%d\n", failures);
    return failures == 0 ? 0 : 1;
}
