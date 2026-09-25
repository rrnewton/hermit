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
 * `pause_alarm_interrupt.c` is the neighbouring case: `pause(2)` is modelled
 * as an indefinite sleep, not as an `rt_sigsuspend` blocker, so it does not
 * reach this path. In `signal_determinism.c itimer-delivery` the timer fires
 * before the suspend begins, so the signal is already pending and it does not
 * reach this path either.
 */

#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static volatile sig_atomic_t handler_runs = 0;

static void on_alarm(int signo) {
  (void)signo;
  handler_runs++;
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

  puts("SIGSUSPEND_ALARM_WAKE_OK");
  return 0;
}
