/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// An alarm interrupts a thread blocked in external IO: select(2) or poll(2)
// on a socket that never becomes readable. Linux ends each wait with EINTR,
// even under SA_RESTART, because select and poll are never restarted after a
// handler (signal(7)). Under Hermit the wait runs outside the deterministic
// run queue, so only the scheduler's alarm can end it.
//
// Phase 1 is the plain case. Phases 2 and 3 install the handler with
// SA_RESTART, so a wait that the tracer reports as ERESTARTSYS would restart
// and never end. Phase 4 adds a sibling that sleeps past the alarm without
// blocking it, so a process-directed alarm the kernel gives to the sibling
// would leave the select waiting forever. Phase 5 adds a sibling that blocks
// the alarm and keeps the scheduler busy with short sleeps, so the wait's end
// must be committed at a deterministic turn for two runs to agree; phases 6
// to 8 repeat it.

#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

// Above 64, so glibc's select passes a large nfds straight to the kernel.
#define WAIT_FD 80
#define MAX_TICKS 5000

static volatile sig_atomic_t handled;
static volatile sig_atomic_t handler_tid;
static int ticker_done;

static pid_t current_tid(void) {
  return (pid_t)syscall(SYS_gettid);
}

static void on_alarm(int signo) {
  (void)signo;
  handled++;
  handler_tid = current_tid();
}

static void fail(const char *phase, const char *what) {
  fprintf(stderr, "%s: %s\n", phase, what);
  exit(1);
}

static void install_handler(const char *phase, int flags) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = on_alarm;
  action.sa_flags = flags;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGALRM, &action, NULL) != 0) {
    fail(phase, "sigaction failed");
  }
}

static void *sleep_past_alarm(void *arg) {
  (void)arg;
  struct timespec delay = {1, 1000};
  while (nanosleep(&delay, &delay) != 0 && errno == EINTR) {
  }
  return NULL;
}

static void *tick_with_alarm_blocked(void *arg) {
  long *ticks = arg;
  sigset_t blocked;
  sigemptyset(&blocked);
  sigaddset(&blocked, SIGALRM);
  pthread_sigmask(SIG_BLOCK, &blocked, NULL);
  struct timespec delay = {0, 1000000};
  while (!__atomic_load_n(&ticker_done, __ATOMIC_SEQ_CST) && *ticks < MAX_TICKS) {
    nanosleep(&delay, NULL);
    (*ticks)++;
  }
  return NULL;
}

enum sibling { NO_SIBLING, SLEEPING_SIBLING, TICKING_SIBLING };

static void run_phase(const char *phase, int flags, int use_poll, enum sibling sibling) {
  install_handler(phase, flags);
  int pair[2];
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair) != 0) {
    fail(phase, "socketpair failed");
  }
  if (dup2(pair[0], WAIT_FD) != WAIT_FD) {
    fail(phase, "dup2 failed");
  }
  handled = 0;
  handler_tid = 0;
  __atomic_store_n(&ticker_done, 0, __ATOMIC_SEQ_CST);
  long ticks = 0;
  pthread_t thread;
  if (sibling == SLEEPING_SIBLING &&
      pthread_create(&thread, NULL, sleep_past_alarm, NULL) != 0) {
    fail(phase, "pthread_create failed");
  }
  if (sibling == TICKING_SIBLING &&
      pthread_create(&thread, NULL, tick_with_alarm_blocked, &ticks) != 0) {
    fail(phase, "pthread_create failed");
  }

  alarm(1);
  int result;
  if (use_poll) {
    struct pollfd wait = {.fd = WAIT_FD, .events = POLLIN};
    result = poll(&wait, 1, -1);
  } else {
    fd_set readable;
    FD_ZERO(&readable);
    FD_SET(WAIT_FD, &readable);
    result = select(WAIT_FD + 1, &readable, NULL, NULL, NULL);
  }
  int saved_errno = errno;
  __atomic_store_n(&ticker_done, 1, __ATOMIC_SEQ_CST);
  if (sibling != NO_SIBLING && pthread_join(thread, NULL) != 0) {
    fail(phase, "pthread_join failed");
  }

  if (result != -1 || saved_errno != EINTR) {
    fprintf(stderr, "%s: wait returned %d errno=%d, expected -1 errno=EINTR\n", phase,
            result, saved_errno);
    exit(1);
  }
  if (handled != 1) {
    fprintf(stderr, "%s: handler ran %d times, expected once\n", phase, (int)handled);
    exit(1);
  }
  if (handler_tid != current_tid()) {
    fail(phase, "the handler did not run on the waiting thread");
  }
  printf("%s: %s=-1 errno=EINTR handler=waiter", phase, use_poll ? "poll" : "select");
  if (sibling == TICKING_SIBLING) {
    printf(" ticks=%ld", ticks);
  }
  printf("\n");
  fflush(stdout);
  close(WAIT_FD);
  close(pair[0]);
  close(pair[1]);
}

int main(void) {
  run_phase("phase 1", 0, 0, NO_SIBLING);
  run_phase("phase 2", SA_RESTART, 0, NO_SIBLING);
  run_phase("phase 3", SA_RESTART, 1, NO_SIBLING);
  run_phase("phase 4", SA_RESTART, 0, SLEEPING_SIBLING);
  // Before the fix, two runs disagreed on phase 5 about three times in ten,
  // so it repeats to make one run of an unfixed build likely to diverge.
  run_phase("phase 5", 0, 0, TICKING_SIBLING);
  run_phase("phase 6", 0, 0, TICKING_SIBLING);
  run_phase("phase 7", 0, 0, TICKING_SIBLING);
  run_phase("phase 8", 0, 0, TICKING_SIBLING);
  printf("external-io-signal-interrupt-ok\n");
  return 0;
}
