/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * `sigsuspend(2)` with a pending timer signal that only the suspend mask
 * unblocks. The thread blocks SIGALRM, arms `alarm(1)`, and then waits in
 * `sigsuspend` with an empty mask, so the alarm cannot fire before the wait
 * begins and the wait can end only through that delivery.
 *
 * Under Detcore the alarm is the scheduler's own timed event: `fire_alarm`
 * signals a thread that is parked in the scheduler's `rt_sigsuspend` pool. The
 * scheduler must take the thread out of that pool and wait for the thread's
 * own report of the interrupted syscall. It must not fabricate that report.
 * Before that was fixed, debug builds failed the assertion
 * `signal_guest: thread should be parked in the scheduler` here, and the run
 * exited 125 with no verify result.
 *
 * The second phase gives the alarm a target that cannot take it. The main
 * thread, which armed the alarm, waits in `sigsuspend` with SIGALRM still
 * blocked, while a worker waits with a mask that admits it. The alarm is
 * process-directed, so Linux delivers it to the worker, and the worker wakes
 * the main thread with SIGUSR1. The scheduler must choose the worker itself.
 * Before it did, the scheduler signalled the main thread, the host kernel
 * rerouted the signal, and a scheduler with nothing else to run could report
 * a terminal deadlock before the worker's report arrived. Under `--verify`
 * that happened on every run.
 *
 * `pause_alarm_interrupt.c` is the neighbouring case: `pause(2)` is modelled
 * as an indefinite sleep, not as an `rt_sigsuspend` blocker, so it does not
 * reach this path. In `signal_determinism.c itimer-delivery` the timer fires
 * before the suspend begins, so the signal is already pending and it does not
 * reach this path either.
 */

#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static volatile sig_atomic_t handler_runs = 0;

static void on_alarm(int signo) {
  (void)signo;
  handler_runs++;
}

static pthread_t main_thread;
static pthread_t worker_thread;
static volatile sig_atomic_t rerouted_alarm_runs = 0;
static volatile sig_atomic_t rerouted_alarm_on_worker = 0;
static volatile sig_atomic_t wake_runs = 0;

static void on_rerouted_alarm(int signo) {
  (void)signo;
  rerouted_alarm_runs++;
  rerouted_alarm_on_worker = pthread_equal(pthread_self(), worker_thread);
}

static void on_wake(int signo) {
  (void)signo;
  wake_runs++;
}

/* Waits with SIGALRM admitted and SIGUSR1 blocked, then wakes the main thread
 * with SIGUSR1. Both signals stay blocked outside the suspend. */
static void* rerouted_alarm_worker(void* arg) {
  (void)arg;
  sigset_t admit_alarm;
  sigemptyset(&admit_alarm);
  sigaddset(&admit_alarm, SIGUSR1);
  int returns = 0;
  while (rerouted_alarm_runs == 0 && returns < 4) {
    sigsuspend(&admit_alarm);
    returns++;
  }
  pthread_kill(main_thread, SIGUSR1);
  return NULL;
}

/* Returns 0 on success. */
static int rerouted_alarm_phase(void) {
  struct sigaction sa;
  memset(&sa, 0, sizeof(sa));
  sigemptyset(&sa.sa_mask);
  sa.sa_handler = on_rerouted_alarm;
  if (sigaction(SIGALRM, &sa, NULL) != 0) {
    puts("SIGSUSPEND_ALARM_SIGACTION_FAILED");
    return 1;
  }
  sa.sa_handler = on_wake;
  if (sigaction(SIGUSR1, &sa, NULL) != 0) {
    puts("SIGSUSPEND_ALARM_SIGACTION_FAILED");
    return 1;
  }

  sigset_t both;
  sigemptyset(&both);
  sigaddset(&both, SIGALRM);
  sigaddset(&both, SIGUSR1);
  if (sigprocmask(SIG_BLOCK, &both, NULL) != 0) {
    puts("SIGSUSPEND_ALARM_SIGPROCMASK_FAILED");
    return 1;
  }

  main_thread = pthread_self();
  if (pthread_create(&worker_thread, NULL, rerouted_alarm_worker, NULL) != 0) {
    puts("SIGSUSPEND_ALARM_PTHREAD_CREATE_FAILED");
    return 1;
  }
  alarm(1);

  sigset_t admit_wake;
  sigemptyset(&admit_wake);
  sigaddset(&admit_wake, SIGALRM);
  int returns = 0;
  while (wake_runs == 0 && returns < 4) {
    sigsuspend(&admit_wake);
    returns++;
  }
  if (pthread_join(worker_thread, NULL) != 0) {
    puts("SIGSUSPEND_ALARM_PTHREAD_JOIN_FAILED");
    return 1;
  }

  printf(
      "rerouted alarm_runs=%d alarm_on_worker=%d wake_runs=%d main_returns=%d\n",
      (int)rerouted_alarm_runs,
      (int)rerouted_alarm_on_worker,
      (int)wake_runs,
      returns);
  return !(rerouted_alarm_runs == 1 && rerouted_alarm_on_worker &&
           wake_runs == 1 && returns == 1);
}

int main(void) {
  struct sigaction sa;
  memset(&sa, 0, sizeof(sa));
  sa.sa_handler = on_alarm;
  sigemptyset(&sa.sa_mask);
  if (sigaction(SIGALRM, &sa, NULL) != 0) {
    puts("SIGSUSPEND_ALARM_SIGACTION_FAILED");
    return 1;
  }

  sigset_t alarm_only;
  sigset_t empty;
  sigemptyset(&alarm_only);
  sigaddset(&alarm_only, SIGALRM);
  sigemptyset(&empty);
  if (sigprocmask(SIG_BLOCK, &alarm_only, NULL) != 0) {
    puts("SIGSUSPEND_ALARM_SIGPROCMASK_FAILED");
    return 1;
  }

  alarm(1);

  /* SIGALRM stays blocked outside the suspend, so every return must come from
   * a delivery inside it; one delivery is expected. */
  int returns = 0;
  int eintr_returns = 0;
  while (handler_runs == 0 && returns < 4) {
    errno = 0;
    int rc = sigsuspend(&empty);
    returns++;
    if (rc == -1 && errno == EINTR) {
      eintr_returns++;
    }
  }

  sigset_t after;
  sigemptyset(&after);
  int mask_restored = sigprocmask(SIG_SETMASK, NULL, &after) == 0 &&
      sigismember(&after, SIGALRM) == 1;

  /* Report booleans and counts rather than strerror(3) so the output cannot
   * depend on the ambient locale. */
  printf(
      "sigsuspend returns=%d eintr=%d handler_runs=%d mask_restored=%d\n",
      returns,
      eintr_returns,
      (int)handler_runs,
      mask_restored);

  if (returns != 1 || eintr_returns != 1 || handler_runs != 1 ||
      !mask_restored) {
    puts("SIGSUSPEND_ALARM_WAKE_FAILED");
    return 1;
  }

  if (rerouted_alarm_phase() != 0) {
    puts("SIGSUSPEND_ALARM_WAKE_FAILED");
    return 1;
  }

  puts("SIGSUSPEND_ALARM_WAKE_OK");
  return 0;
}
