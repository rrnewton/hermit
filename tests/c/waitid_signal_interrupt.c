/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Blocking `waitid` has three signal-sensitive contracts covered here: an
 * interrupt must make progress and run its handler, SA_RESTART must resume the
 * wait, and an already-ready child status must win when SIGCHLD is pending at
 * the same scheduling boundary. Together they reject both the original spin
 * and a tempting signal-first repair that changes Linux precedence.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t handler_ran;

static void on_signal(int signum) {
  (void)signum;
  handler_ran = 1;
}

static int signal_interrupt(void) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = on_signal;
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

static int child_ready_wins(void) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = on_signal;
  sigemptyset(&action.sa_mask);
  action.sa_flags = 0; /* deliberately NOT SA_RESTART */
  if (sigaction(SIGCHLD, &action, NULL) != 0) {
    printf("waitid-ready-wins-setup-failed sigaction errno=%d\n", errno);
    return 1;
  }

  pid_t child = fork();
  if (child < 0) {
    printf("waitid-ready-wins-setup-failed fork errno=%d\n", errno);
    return 1;
  }
  if (child == 0) {
    // Give the parent time to enter waitid. The resulting child exit makes
    // SIGCHLD pending at the same scheduler boundary at which the child's
    // wait status becomes observable.
    const struct timespec delay = {.tv_sec = 0, .tv_nsec = 20 * 1000 * 1000};
    struct timespec remaining = delay;
    while (nanosleep(&remaining, &remaining) != 0 && errno == EINTR) {
    }
    _exit(23);
  }

  siginfo_t info;
  memset(&info, 0, sizeof info);
  errno = 0;
  int rc = waitid(P_PID, child, &info, WEXITED);
  int saved_errno = errno;

  printf(
      "waitid-ready-wins rc=%d errno=%d handler=%d pid-match=%d code=%d status=%d\n",
      rc,
      rc < 0 ? saved_errno : 0,
      (int)handler_ran,
      info.si_pid == child,
      info.si_code,
      info.si_status);

  if (rc != 0) {
    // The rejected implementation reaches this path: it observes SIGCHLD and
    // returns EINTR without first polling the now-ready child status.
    waitpid(child, NULL, 0);
    return 2;
  }
  if (info.si_pid != child || info.si_code != CLD_EXITED ||
      info.si_status != 23) {
    return 3;
  }
  return 0;
}

static int signal_restart(void) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = on_signal;
  sigemptyset(&action.sa_mask);
  action.sa_flags = SA_RESTART;
  if (sigaction(SIGALRM, &action, NULL) != 0) {
    printf("waitid-signal-restart-setup-failed sigaction errno=%d\n", errno);
    return 1;
  }

  pid_t child = fork();
  if (child < 0) {
    printf("waitid-signal-restart-setup-failed fork errno=%d\n", errno);
    return 1;
  }
  if (child == 0) {
    sleep(2);
    _exit(17);
  }

  siginfo_t info;
  memset(&info, 0, sizeof info);
  alarm(1);
  int rc = waitid(P_PID, child, &info, WEXITED);
  int saved_errno = errno;
  alarm(0);
  printf(
      "waitid-signal-restart rc=%d errno=%d handler=%d pid-match=%d code=%d status=%d\n",
      rc,
      rc < 0 ? saved_errno : 0,
      (int)handler_ran,
      info.si_pid == child,
      info.si_code,
      info.si_status);

  if (rc != 0) {
    kill(child, SIGKILL);
    waitpid(child, NULL, 0);
    return 2;
  }
  if (!handler_ran || info.si_pid != child || info.si_code != CLD_EXITED ||
      info.si_status != 17) {
    return 3;
  }
  return 0;
}

int main(int argc, char **argv) {
  if (argc == 1) {
    return signal_interrupt();
  }
  if (argc == 2 && strcmp(argv[1], "--child-ready-wins") == 0) {
    return child_ready_wins();
  }
  if (argc == 2 && strcmp(argv[1], "--signal-restart") == 0) {
    return signal_restart();
  }
  fprintf(stderr, "usage: %s [--child-ready-wins|--signal-restart]\n", argv[0]);
  return 64;
}
