/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A periodic ITIMER_REAL keeps expiring while two workers sit in select(2)
// calls that have already returned. Each worker selects on its own socket,
// which holds a byte before the call starts, so the call returns 1 at once:
// select reports a ready descriptor even when a signal is pending. Hermit runs
// a select over more than 64 descriptors as a blocking host call outside the
// deterministic run queue, and while the main thread keeps the run queue busy
// the scheduler does not requeue a worker whose call returned on its own.
//
// The main thread blocks SIGALRM, so only a worker can take an expiry. Hermit
// sends an expiry to one such worker alone and records that copy. Linux keeps
// one pending standard signal per process, so every further expiry before a
// thread dequeues it merges with it. The worker dequeues its copy only when it
// next returns to user mode, after the scheduler grants its continuation, so
// until then every further expiry must send nothing: a second copy sent to the
// other worker would run the handler twice where Linux runs it once. For each
// expiry merged that way the scheduler writes an INFO record, which
// `--verify-strict` compares, so the merge is reached at the same point in both
// runs. The handler blocks SIGALRM in the mask its thread returns to, so each
// worker takes one signal and no more, and the main thread disarms the timer
// after a fixed number of yields, so the run has a bounded number of expiries.

#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <ucontext.h>
#include <unistd.h>

// Above 64, so Hermit runs each select as a blocking host call instead of
// polling the descriptor set inside the run queue.
#define FIRST_FD 80
#define WORKERS 2
// Yields after both workers announce their select, so that each select is
// committed before the timer is armed.
#define SETTLE_YIELDS 16
// Work between arming the timer and the first yield, so virtual time runs
// several periods past the first expiry: the expiries after it are then
// overdue at the passes that follow.
#define SPIN_ITERATIONS 2000000L
// Yields with the timer armed; each scheduler pass handles one overdue expiry.
#define ARMED_YIELDS 32

struct worker {
  int index;
  int fd;
  int result;
  int saved_errno;
  int ready;
  int handled;
};

static volatile sig_atomic_t handled[WORKERS];
static volatile sig_atomic_t handled_elsewhere;
static __thread int worker_index = -1;
static int selecting;

static void on_alarm(int signo, siginfo_t *info, void *context) {
  (void)signo;
  (void)info;
  if (worker_index >= 0 && worker_index < WORKERS) {
    handled[worker_index]++;
  } else {
    handled_elsewhere++;
  }
  // The kernel restores this mask when the handler returns, so the thread
  // takes no further SIGALRM: one signal per worker, whatever the timer does
  // afterwards.
  ucontext_t *interrupted = context;
  sigaddset(&interrupted->uc_sigmask, SIGALRM);
}

static void fail(const char *what) {
  fprintf(stderr, "timer-copy-coalesce: %s (errno %d)\n", what, errno);
  exit(1);
}

static void alarm_set(sigset_t *set) {
  sigemptyset(set);
  sigaddset(set, SIGALRM);
}

static void *worker_main(void *arg) {
  struct worker *worker = arg;
  worker_index = worker->index;
  sigset_t alarm_only;
  alarm_set(&alarm_only);
  if (pthread_sigmask(SIG_UNBLOCK, &alarm_only, NULL) != 0) {
    fail("worker could not unblock SIGALRM");
  }
  fd_set readable;
  FD_ZERO(&readable);
  FD_SET(worker->fd, &readable);
  __atomic_add_fetch(&selecting, 1, __ATOMIC_SEQ_CST);
  worker->result = select(worker->fd + 1, &readable, NULL, NULL, NULL);
  worker->saved_errno = errno;
  worker->ready = worker->result == 1 && FD_ISSET(worker->fd, &readable);
  // A copy sent to this thread alone has been taken by now: the kernel ran
  // the handler on the return from select, before this line. Block SIGALRM
  // in case no signal came.
  if (pthread_sigmask(SIG_BLOCK, &alarm_only, NULL) != 0) {
    fail("worker could not block SIGALRM");
  }
  worker->handled = handled[worker->index];
  return NULL;
}

static void spin(long iterations) {
  volatile long sink = 0;
  for (long i = 0; i < iterations; i++) {
    sink += i;
  }
}

int main(void) {
  sigset_t alarm_only;
  alarm_set(&alarm_only);
  // The workers inherit this mask and unblock SIGALRM themselves.
  if (pthread_sigmask(SIG_BLOCK, &alarm_only, NULL) != 0) {
    fail("main could not block SIGALRM");
  }
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_sigaction = on_alarm;
  action.sa_flags = SA_SIGINFO;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGALRM, &action, NULL) != 0) {
    fail("sigaction failed");
  }

  int pairs[WORKERS][2];
  struct worker workers[WORKERS];
  pthread_t threads[WORKERS];
  for (int i = 0; i < WORKERS; i++) {
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, pairs[i]) != 0) {
      fail("socketpair failed");
    }
    if (write(pairs[i][1], "x", 1) != 1) {
      fail("write failed");
    }
    if (dup2(pairs[i][0], FIRST_FD + i) != FIRST_FD + i) {
      fail("dup2 failed");
    }
    memset(&workers[i], 0, sizeof workers[i]);
    workers[i].index = i;
    workers[i].fd = FIRST_FD + i;
  }
  for (int i = 0; i < WORKERS; i++) {
    if (pthread_create(&threads[i], NULL, worker_main, &workers[i]) != 0) {
      fail("pthread_create failed");
    }
  }
  while (__atomic_load_n(&selecting, __ATOMIC_SEQ_CST) < WORKERS) {
    sched_yield();
  }
  for (int i = 0; i < SETTLE_YIELDS; i++) {
    sched_yield();
  }

  struct itimerval every = {.it_interval = {0, 1}, .it_value = {0, 1}};
  if (setitimer(ITIMER_REAL, &every, NULL) != 0) {
    fail("setitimer failed");
  }
  spin(SPIN_ITERATIONS);
  for (int i = 0; i < ARMED_YIELDS; i++) {
    sched_yield();
  }
  struct itimerval off;
  memset(&off, 0, sizeof off);
  if (setitimer(ITIMER_REAL, &off, NULL) != 0) {
    fail("setitimer could not disarm the timer");
  }

  int total = 0;
  for (int i = 0; i < WORKERS; i++) {
    if (pthread_join(threads[i], NULL) != 0) {
      fail("pthread_join failed");
    }
    if (workers[i].result != 1 || !workers[i].ready) {
      fprintf(stderr,
              "timer-copy-coalesce: worker %d select returned %d errno=%d, "
              "expected 1 with its descriptor ready\n",
              i, workers[i].result, workers[i].saved_errno);
      return 1;
    }
    total += workers[i].handled;
    printf("worker %d: select=1 ready handler-runs=%d\n", i, workers[i].handled);
  }
  if (handled_elsewhere != 0) {
    fprintf(stderr, "timer-copy-coalesce: the handler ran on the main thread\n");
    return 1;
  }
  printf("handler runs on workers: %d\n", total);
  printf("timer-copy-coalesce-ok\n");
  return 0;
}
