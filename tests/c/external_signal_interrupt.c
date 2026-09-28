/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Guest for hermit-cli/tests/external_signal_interrupt.rs
 * (https://github.com/rrnewton/hermit/issues/3146).
 *
 * usage: external_signal_interrupt <futex|select|wait4|waitid>
 *            <external|process|thread|timer> [restart] [timed]
 *
 * The main thread installs a SIGUSR1 and SIGALRM handler (SA_RESTART only
 * with `restart`), prints READY, and blocks in the chosen call until the
 * signal arrives:
 *   external: from outside Hermit; the harness signals this process.
 *   process:  from a forked guest process that stays alive after kill().
 *             For wait4/waitid it is the waited child, blocked on a pipe.
 *   thread:   from a sibling thread with pthread_kill(). The thread then
 *             sets the futex word and wakes the futex, so an SA_RESTART
 *             futex wait that Linux restarts still returns.
 *   timer:    SIGALRM from a 100 ms ITIMER_REAL, which Hermit's scheduler
 *             delivers itself.
 * `timed` gives the futex wait a relative timeout far beyond the signal.
 *
 * Output is one deterministic RESULT line after the call returns, followed
 * by DONE once every helper has been reaped. */
#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
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

static void *thread_sender(void *arg) {
  (void)arg;
  sigset_t block;
  sigemptyset(&block);
  sigaddset(&block, SIGUSR1);
  pthread_sigmask(SIG_BLOCK, &block, NULL);
  sleep_ms(100);
  if (pthread_kill(main_thread, SIGUSR1) != 0) _exit(91);
  sleep_ms(100);
  __atomic_store_n(&futex_word, 1, __ATOMIC_SEQ_CST);
  syscall(SYS_futex, &futex_word, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0);
  return NULL;
}

int main(int argc, char **argv) {
  if (argc < 3) {
    say("usage: external_signal_interrupt <futex|select|wait4|waitid> "
        "<external|process|thread|timer> [restart] [timed]\n");
    return 2;
  }
  const char *call = argv[1];
  const char *sender = argv[2];
  int restart = 0, timed = 0;
  for (int i = 3; i < argc; i++) {
    if (!strcmp(argv[i], "restart")) restart = 1;
    else if (!strcmp(argv[i], "timed")) timed = 1;
    else return 2;
  }
  int is_futex = !strcmp(call, "futex");
  int is_wait = !strcmp(call, "wait4") || !strcmp(call, "waitid");
  int from_process = !strcmp(sender, "process");
  int from_thread = !strcmp(sender, "thread");
  int from_timer = !strcmp(sender, "timer");
  if (!is_futex && !is_wait && strcmp(call, "select")) return 2;
  if (!from_process && !from_thread && !from_timer && strcmp(sender, "external")) return 2;
  if (is_wait && !from_process) return 2;

  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_handler = on_usr1;
  sigemptyset(&sa.sa_mask);
  sa.sa_flags = restart ? SA_RESTART : 0;
  if (sigaction(SIGUSR1, &sa, NULL) != 0) return 3;
  if (sigaction(SIGALRM, &sa, NULL) != 0) return 3;

  int pfd[2];
  if (pipe(pfd) != 0) return 3;
  pid_t parent = getpid();
  pid_t child = -1;
  pthread_t thread;
  if (from_process) {
    child = fork();
    if (child < 0) return 3;
    if (child == 0) {
      signal(SIGUSR1, SIG_IGN);
      close(pfd[1]);
      sleep_ms(100);
      kill(parent, SIGUSR1);
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
  errno = 0;
  if (is_futex) {
    struct timespec timeout = {10, 0};
    ret = syscall(SYS_futex, &futex_word, FUTEX_WAIT_PRIVATE, 0, timed ? &timeout : NULL,
                  NULL, 0);
  } else if (!strcmp(call, "select")) {
    int sp[2];
    if (pipe(sp) != 0) return 3;
    fd_set rf;
    FD_ZERO(&rf);
    FD_SET(sp[0], &rf);
    ret = select(sp[0] + 1, &rf, NULL, NULL, NULL);
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

  char buf[160];
  snprintf(buf, sizeof buf, "RESULT call=%s ret=%ld errno=%s handler=%d\n", call,
           ret < 0 ? -1L : ret, ret < 0 ? strerrorname_np(err) : "none", (int)handled);
  say(buf);
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
