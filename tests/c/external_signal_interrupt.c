/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Guest for hermit-cli/tests/external_signal_interrupt.rs
 * (https://github.com/rrnewton/hermit/issues/3146).
 *
 * usage: external_signal_interrupt <futex|select|rawselect|poll|epoll|wait4|waitid>
 *            <external|process|thread|timer|exit> [restart] [timed]
 *            [ignored|blocked|winch|ign2caught|caught2ign|chldlate|chldign]
 *
 * `select` is glibc's, which issues pselect6 with no mask; `rawselect` is the
 * select system call itself.
 *
 * The main thread installs a SIGUSR1 and SIGALRM handler (SA_RESTART only
 * with `restart`), prints READY, and blocks in the chosen call until the
 * signal arrives:
 *   external: from outside Hermit; the harness signals this process.
 *   process:  from a forked guest process that stays alive after kill().
 *             For wait4/waitid it is the waited child, blocked on a pipe.
 *   thread:   from a sibling thread with pthread_kill(). The thread waits,
 *             boundedly, for the handler to run (exiting 93 with
 *             WAKER_TIMEOUT if it never does), then sets the futex word and
 *             wakes the futex, so an SA_RESTART futex wait that Linux
 *             restarts still returns.
 *   timer:    SIGALRM from a 100 ms ITIMER_REAL, which Hermit's scheduler
 *             delivers itself.
 *   exit:     SIGCHLD, caught from the start, from a forked child that exits
 *             after 100 ms. Hermit's scheduler delivers the child-exit SIGCHLD;
 *             the kernel also posts its own at a host-timed moment.
 * `timed` gives the futex wait a relative timeout far beyond the signal.
 *
 * The last three options send a signal that must NOT end the wait: SIGUSR1
 * with SIG_IGN (`ignored`), SIGUSR1 blocked by the waiter (`blocked`), or
 * SIGWINCH, which is ignored by default (`winch`). The call then has a 300 ms
 * timeout, the sender does not wake the futex, and an ELAPSED line reports the
 * CLOCK_MONOTONIC time the call took, so the wait must run to its deadline.
 *
 * The next four options change a disposition while the waiter is already
 * parked; they need the `thread` sender. The sibling thread blocks SIGUSR1 and
 * SIGCHLD, so only the waiter can take the signal, and after 100 ms it:
 *   ign2caught: installs the SIGUSR1 handler over SIG_IGN, then pthread_kill.
 *   caught2ign: sets SIGUSR1 to SIG_IGN over the handler, then pthread_kill.
 *   chldlate:   installs a SIGCHLD handler over SIG_DFL, then forks a child
 *               that exits at once.
 *   chldign:    resets a caught SIGCHLD to SIG_DFL (ignored by default), then
 *               forks a child that exits at once.
 * ign2caught and chldlate must end the wait with EINTR near 100 ms; the
 * sibling wakes the futex (and writes the pipe) only after the handler ran.
 * caught2ign and chldign must not end it: a `timed` wait then returns at its
 * original 300 ms deadline, and an untimed one is ended by the sibling's
 * FUTEX_WAKE (or pipe write) 200 ms after the signal.
 *
 * `exit` and these four options also print the ELAPSED line.
 *
 * Output is one deterministic RESULT line after the call returns, followed
 * by DONE once every helper has been reaped. */
#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/resource.h>
#include <sys/select.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t handled = 0;
static uint32_t futex_word = 0;
static pthread_t main_thread;
static int sent_signal = SIGUSR1;
static int quiet = 0;
/* A disposition change while the waiter is parked (see the usage comment). */
enum flip { FLIP_NONE, IGN2CAUGHT, CAUGHT2IGN, CHLDLATE, CHLDIGN };
static enum flip flip = FLIP_NONE;
static int flip_timed = 0;
/* Write end of the pipe a readiness call waits on. */
static int ready_write_fd = -1;

#define QUIET_TIMEOUT_MS 300
#define WAKER_POLLS 5000

static void on_usr1(int sig) {
  (void)sig;
  handled += 1;
}

static void say(const char *s) {
  size_t left = strlen(s);
  while (left > 0) {
    ssize_t n = write(1, s, left);
    if (n < 0 && errno == EINTR) continue;
    if (n <= 0) _exit(90);
    s += n;
    left -= (size_t)n;
  }
}

static void sleep_ms(long ms) {
  struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
  while (nanosleep(&ts, &ts) != 0 && errno == EINTR) {
  }
}

static void set_handler(int sig, void (*handler)(int)) {
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_handler = handler;
  sigemptyset(&sa.sa_mask);
  if (sigaction(sig, &sa, NULL) != 0) _exit(94);
}

/* End the waiter's call the way a waker would: set and wake the futex word and
 * make the readiness pipe readable. */
static void wake_waiter(void) {
  __atomic_store_n(&futex_word, 1, __ATOMIC_SEQ_CST);
  syscall(SYS_futex, &futex_word, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0);
  if (write(ready_write_fd, "x", 1) != 1) _exit(95);
}

static void wait_for_handler(void) {
  int polls = 0;
  while (handled == 0) {
    if (++polls > WAKER_POLLS) {
      say("WAKER_TIMEOUT\n");
      _exit(93);
    }
    sleep_ms(1);
  }
}

/* The sibling for the disposition-change options. */
static void flip_sender(void) {
  pid_t child = -1;
  switch (flip) {
    case IGN2CAUGHT:
      set_handler(SIGUSR1, on_usr1);
      if (pthread_kill(main_thread, SIGUSR1) != 0) _exit(91);
      break;
    case CAUGHT2IGN:
      set_handler(SIGUSR1, SIG_IGN);
      if (pthread_kill(main_thread, SIGUSR1) != 0) _exit(91);
      break;
    case CHLDLATE:
    case CHLDIGN:
      set_handler(SIGCHLD, flip == CHLDLATE ? on_usr1 : SIG_DFL);
      child = fork();
      if (child < 0) _exit(96);
      if (child == 0) _exit(0);
      break;
    case FLIP_NONE:
      _exit(97);
  }
  if (flip == IGN2CAUGHT || flip == CHLDLATE) {
    /* Wake only after the handler ran, so a wait the signal did not end is
     * not rescued by this wake. */
    wait_for_handler();
    sleep_ms(100);
    wake_waiter();
  } else {
    sleep_ms(200);
    if (!flip_timed) wake_waiter();
  }
  if (child > 0) {
    int st;
    while (waitpid(child, &st, 0) < 0 && errno == EINTR) {
    }
  }
}

static void *thread_sender(void *arg) {
  (void)arg;
  sigset_t block;
  sigemptyset(&block);
  sigaddset(&block, SIGUSR1);
  if (flip != FLIP_NONE) sigaddset(&block, SIGCHLD);
  pthread_sigmask(SIG_BLOCK, &block, NULL);
  sleep_ms(100);
  if (flip != FLIP_NONE) {
    flip_sender();
    return NULL;
  }
  if (pthread_kill(main_thread, sent_signal) != 0) _exit(91);
  if (quiet) return NULL;
  /* Wake only after the handler ran, so a wait that the signal did not
   * interrupt cannot be rescued by this wake. */
  int polls = 0;
  while (handled == 0) {
    if (++polls > WAKER_POLLS) {
      say("WAKER_TIMEOUT\n");
      _exit(93);
    }
    sleep_ms(1);
  }
  sleep_ms(100);
  __atomic_store_n(&futex_word, 1, __ATOMIC_SEQ_CST);
  syscall(SYS_futex, &futex_word, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0);
  return NULL;
}

int main(int argc, char **argv) {
  if (argc < 3) {
    say("usage: external_signal_interrupt <futex|select|rawselect|poll|epoll|wait4|waitid> "
        "<external|process|thread|timer|exit> [restart] [timed] "
        "[ignored|blocked|winch|ign2caught|caught2ign|chldlate|chldign]\n");
    return 2;
  }
  const char *call = argv[1];
  const char *sender = argv[2];
  int restart = 0, timed = 0, ignored = 0, blocked = 0;
  for (int i = 3; i < argc; i++) {
    if (!strcmp(argv[i], "restart")) restart = 1;
    else if (!strcmp(argv[i], "timed")) timed = 1;
    else if (!strcmp(argv[i], "ignored")) ignored = quiet = 1;
    else if (!strcmp(argv[i], "blocked")) blocked = quiet = 1;
    else if (!strcmp(argv[i], "winch")) {
      sent_signal = SIGWINCH;
      quiet = 1;
    } else if (!strcmp(argv[i], "ign2caught")) flip = IGN2CAUGHT;
    else if (!strcmp(argv[i], "caught2ign")) flip = CAUGHT2IGN;
    else if (!strcmp(argv[i], "chldlate")) flip = CHLDLATE;
    else if (!strcmp(argv[i], "chldign")) flip = CHLDIGN;
    else return 2;
  }
  if (ignored + blocked + (sent_signal == SIGWINCH) + (flip != FLIP_NONE) > 1) return 2;
  flip_timed = timed;
  int is_futex = !strcmp(call, "futex");
  int is_wait = !strcmp(call, "wait4") || !strcmp(call, "waitid");
  int from_process = !strcmp(sender, "process");
  int from_thread = !strcmp(sender, "thread");
  int from_timer = !strcmp(sender, "timer");
  int from_exit = !strcmp(sender, "exit");
  int is_poll = !strcmp(call, "poll");
  int is_epoll = !strcmp(call, "epoll");
  int is_rawselect = !strcmp(call, "rawselect");
  if (!is_futex && !is_wait && !is_poll && !is_epoll && !is_rawselect && strcmp(call, "select"))
    return 2;
  if (!from_process && !from_thread && !from_timer && !from_exit && strcmp(sender, "external"))
    return 2;
  if (is_wait && !from_process) return 2;
  if (quiet && (is_wait || !(from_process || from_thread))) return 2;
  if (flip != FLIP_NONE && (is_wait || !from_thread || restart)) return 2;
  if (from_exit && restart) return 2;
  int report_elapsed = quiet || flip != FLIP_NONE || from_exit;

  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_handler = on_usr1;
  sigemptyset(&sa.sa_mask);
  sa.sa_flags = restart ? SA_RESTART : 0;
  if ((from_exit || flip == CHLDIGN) && sigaction(SIGCHLD, &sa, NULL) != 0) return 3;
  if (ignored) sa.sa_handler = SIG_IGN;
  struct sigaction usr1 = sa;
  if (flip == IGN2CAUGHT) usr1.sa_handler = SIG_IGN;
  if (sigaction(SIGUSR1, &usr1, NULL) != 0) return 3;
  if (sigaction(SIGALRM, &sa, NULL) != 0) return 3;
  if (blocked) {
    sigset_t mask;
    sigemptyset(&mask);
    sigaddset(&mask, SIGUSR1);
    if (sigprocmask(SIG_BLOCK, &mask, NULL) != 0) return 3;
  }

  int pfd[2];
  if (pipe(pfd) != 0) return 3;
  int sp[2];
  if (pipe(sp) != 0) return 3;
  ready_write_fd = sp[1];
  pid_t parent = getpid();
  pid_t child = -1;
  pthread_t thread;
  if (from_exit) {
    child = fork();
    if (child < 0) return 3;
    if (child == 0) {
      sleep_ms(100);
      _exit(0);
    }
  }
  if (from_process) {
    child = fork();
    if (child < 0) return 3;
    if (child == 0) {
      signal(SIGUSR1, SIG_IGN);
      close(pfd[1]);
      sleep_ms(100);
      kill(parent, sent_signal);
      /* Stay alive: the waiter must not depend on this process exiting. */
      char c;
      ssize_t r = read(pfd[0], &c, 1);
      _exit(r == 0 ? 7 : 8);
    }
  }
  close(pfd[0]);
  main_thread = pthread_self();
  if (from_thread && pthread_create(&thread, NULL, thread_sender, NULL) != 0) return 3;

  say("READY\n");
  if (from_timer) {
    struct itimerval it;
    memset(&it, 0, sizeof it);
    it.it_value.tv_usec = 100000;
    if (setitimer(ITIMER_REAL, &it, NULL) != 0) return 3;
  }

  long ret;
  int err;
  int epfd = -1;
  if (is_epoll) {
    struct epoll_event ev = {.events = EPOLLIN, .data = {.fd = sp[0]}};
    epfd = epoll_create1(0);
    if (epfd < 0 || epoll_ctl(epfd, EPOLL_CTL_ADD, sp[0], &ev) != 0) return 3;
  }
  struct timespec start;
  clock_gettime(CLOCK_MONOTONIC, &start);
  errno = 0;
  /* A must-not-wake wait, or a timed disposition-change wait, has the short
   * timeout; a readiness wait is otherwise unbounded. */
  int bounded = quiet || (flip != FLIP_NONE && timed);
  if (is_futex) {
    struct timespec timeout = {10, 0};
    if (quiet || flip != FLIP_NONE) timeout = (struct timespec){0, QUIET_TIMEOUT_MS * 1000000L};
    ret = syscall(SYS_futex, &futex_word, FUTEX_WAIT_PRIVATE, 0,
                  timed || quiet ? &timeout : NULL, NULL, 0);
  } else if (!strcmp(call, "select") || is_rawselect) {
    fd_set rf;
    FD_ZERO(&rf);
    FD_SET(sp[0], &rf);
    struct timeval tv = {0, QUIET_TIMEOUT_MS * 1000L};
    if (is_rawselect)
      ret = syscall(SYS_select, sp[0] + 1, &rf, NULL, NULL, bounded ? &tv : NULL);
    else
      ret = select(sp[0] + 1, &rf, NULL, NULL, bounded ? &tv : NULL);
  } else if (is_poll) {
    struct pollfd pfd1 = {.fd = sp[0], .events = POLLIN};
    ret = poll(&pfd1, 1, bounded ? QUIET_TIMEOUT_MS : -1);
  } else if (is_epoll) {
    struct epoll_event ev;
    ret = epoll_wait(epfd, &ev, 1, bounded ? QUIET_TIMEOUT_MS : -1);
  } else if (!strcmp(call, "wait4")) {
    int st = 0;
    struct rusage ru;
    ret = wait4(child, &st, 0, &ru);
  } else {
    siginfo_t si;
    memset(&si, 0, sizeof si);
    ret = waitid(P_PID, child, &si, WEXITED);
  }
  err = errno;
  struct timespec end;
  clock_gettime(CLOCK_MONOTONIC, &end);

  char buf[160];
  snprintf(buf, sizeof buf, "RESULT call=%s ret=%ld errno=%s handler=%d\n", call,
           ret < 0 ? -1L : ret, ret < 0 ? strerrorname_np(err) : "none", (int)handled);
  say(buf);
  if (report_elapsed) {
    long elapsed_ms = (end.tv_sec - start.tv_sec) * 1000L +
                      (end.tv_nsec - start.tv_nsec) / 1000000L;
    snprintf(buf, sizeof buf, "ELAPSED ms=%ld\n", elapsed_ms);
    say(buf);
  }
  if (from_thread) pthread_join(thread, NULL);
  if (child > 0) {
    close(pfd[1]);
    int st;
    while (waitpid(child, &st, 0) < 0 && errno == EINTR) {
    }
  }
  say("DONE\n");
  return 0;
}
