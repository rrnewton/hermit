/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A blocking `waitid` interrupted by a signal must return, exactly as Linux
 * does. The regression this guards spun instead: `handle_waitid` never
 * presented its polling thread to the scheduler as a blocking request, so
 * logical time never reached the pending alarm and the loop retried without
 * bound, burning a core.
 *
 * The handler is installed WITHOUT SA_RESTART so the wait is interrupted rather
 * than restarted, and it records that it actually ran -- a return value alone
 * would not distinguish "the signal was delivered" from "the wait ended for
 * some other reason".
 */

#define _GNU_SOURCE

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile sig_atomic_t handler_ran;

static void on_alarm(int signum) {
  (void)signum;
  handler_ran = 1;
}

int main(void) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = on_alarm;
  sigemptyset(&action.sa_mask);
  action.sa_flags = 0; /* deliberately NOT SA_RESTART */
  if (sigaction(SIGALRM, &action, NULL) != 0) {
    printf("waitid-signal-interrupt-setup-failed sigaction errno=%d\n", errno);
    return 1;
  }

  pid_t child = fork();
  if (child < 0) {
    printf("waitid-signal-interrupt-setup-failed fork errno=%d\n", errno);
    return 1;
  }
  if (child == 0) {
    /* Never exits on its own, so the parent's wait condition stays unsatisfied
     * and only the signal can end the wait. */
    for (;;) {
      pause();
    }
  }

  siginfo_t info;
  memset(&info, 0, sizeof info);
  alarm(1);
  int rc = waitid(P_PID, child, &info, WEXITED);
  int saved_errno = errno;
  alarm(0);

  printf(
      "waitid-signal-interrupt rc=%d errno=%d handler=%d\n",
      rc,
      rc < 0 ? saved_errno : 0,
      (int)handler_ran);

  int status;
  kill(child, SIGKILL);
  waitpid(child, &status, 0);
  printf("waitid-signal-interrupt-done\n");
  return 0;
}
