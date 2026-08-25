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
#include <pthread.h>
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

static int live_sibling_signal(int mode) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = on_signal;
  sigemptyset(&action.sa_mask);
  action.sa_flags = mode == 1 ? SA_RESTART : 0;
  if (sigaction(SIGUSR1, &action, NULL) != 0 ||
      sigaction(SIGUSR2, &action, NULL) != 0) {
    printf("waitid-live-sibling-setup-failed sigaction errno=%d\n", errno);
    return 1;
  }

  sigset_t preserve_blocked;
  sigset_t original_mask;
  sigemptyset(&preserve_blocked);
  sigaddset(&preserve_blocked, SIGUSR2);
  if (mode == 2) {
    sigaddset(&preserve_blocked, SIGUSR1);
  }
  if (sigprocmask(SIG_BLOCK, &preserve_blocked, &original_mask) != 0) {
    printf("waitid-live-sibling-setup-failed sigprocmask errno=%d\n", errno);
    return 1;
  }

  pid_t target = fork();
  if (target < 0) {
    printf("waitid-live-sibling-setup-failed target-fork errno=%d\n", errno);
    sigprocmask(SIG_SETMASK, &original_mask, NULL);
    return 1;
  }
  if (target == 0) {
    if (mode != 0) {
      const struct timespec delay = {.tv_sec = 0, .tv_nsec = 100 * 1000 * 1000};
      nanosleep(&delay, NULL);
      _exit(29);
    }
    for (;;) {
      pause();
    }
  }

  pid_t parent = getpid();
  pid_t signaler = fork();
  if (signaler < 0) {
    printf("waitid-live-sibling-setup-failed signaler-fork errno=%d\n", errno);
    kill(target, SIGKILL);
    waitpid(target, NULL, 0);
    sigprocmask(SIG_SETMASK, &original_mask, NULL);
    return 1;
  }
  if (signaler == 0) {
    const struct timespec delay = {.tv_sec = 0, .tv_nsec = 20 * 1000 * 1000};
    nanosleep(&delay, NULL);
    if (kill(parent, SIGUSR2) != 0 || kill(parent, SIGUSR1) != 0) {
      _exit(0);
    }
    /* Stay live after the independently generated signals. The wait must
     * not rely on sender exit or SIGCHLD to make progress. */
    for (;;) {
      pause();
    }
  }

  siginfo_t info;
  memset(&info, 0, sizeof info);
  errno = 0;
  int rc = waitid(P_PID, target, &info, WEXITED);
  int saved_errno = errno;
  sigset_t current_mask;
  sigprocmask(SIG_BLOCK, NULL, &current_mask);
  sigset_t expected_mask = original_mask;
  sigaddset(&expected_mask, SIGUSR2);
  if (mode == 2) {
    sigaddset(&expected_mask, SIGUSR1);
  }
  int mask_preserved = 1;
  for (int signum = 1; signum < NSIG; ++signum) {
    if (sigismember(&current_mask, signum) !=
        sigismember(&expected_mask, signum)) {
      mask_preserved = 0;
      break;
    }
  }
  int sender_live = kill(signaler, 0) == 0;

  int handler_before_restore = handler_ran;
  if (mode == 1) {
    printf(
        "waitid-live-sibling-restart rc=%d errno=%d handler=%d pid-match=%d code=%d status=%d mask-preserved=%d sender-live=%d\n",
        rc,
        rc < 0 ? saved_errno : 0,
        handler_before_restore,
        info.si_pid == target,
        info.si_code,
        info.si_status,
        mask_preserved,
        sender_live);
  } else if (mode == 2) {
    printf(
        "waitid-live-sibling-blocked rc=%d errno=%d handler=%d pid-match=%d code=%d status=%d mask-preserved=%d sender-live=%d\n",
        rc,
        rc < 0 ? saved_errno : 0,
        handler_before_restore,
        info.si_pid == target,
        info.si_code,
        info.si_status,
        mask_preserved,
        sender_live);
  } else {
    printf(
        "waitid-live-sibling rc=%d errno=%d handler=%d mask-preserved=%d sender-live=%d\n",
        rc,
        rc < 0 ? saved_errno : 0,
        handler_before_restore,
        mask_preserved,
        sender_live);
  }

  kill(signaler, SIGKILL);
  kill(target, SIGKILL);
  waitpid(signaler, NULL, 0);
  waitpid(target, NULL, 0);
  sigprocmask(SIG_SETMASK, &original_mask, NULL);
  printf("waitid-live-sibling-done\n");

  if (!mask_preserved || !sender_live ||
      (mode == 2 ? handler_before_restore : !handler_before_restore)) {
    return 2;
  }
  if (mode != 0) {
    return rc == 0 && info.si_pid == target && info.si_code == CLD_EXITED &&
                   info.si_status == 29
               ? 0
               : 3;
  }
  return rc == -1 && saved_errno == EINTR ? 0 : 4;
}

/*
 * A sibling THREAD, not a sibling process. `pthread_kill` lowers to `tgkill`,
 * which is a different Detcore handler from `kill`: an earlier revision of the
 * waitid wakeup notified the scheduler only from `handle_kill`, so a
 * thread-directed signal to a waiter parked on a child that never exits was
 * recorded nowhere and the wait hung forever. Every fixture above sends from a
 * forked PROCESS, which is why none of them could see it.
 */
static pthread_t waitid_waiter_thread;

static void *thread_signaler(void *unused) {
  (void)unused;
  const struct timespec delay = {.tv_sec = 0, .tv_nsec = 150 * 1000 * 1000};
  nanosleep(&delay, NULL);
  pthread_kill(waitid_waiter_thread, SIGUSR1);
  return NULL;
}

static int live_sibling_thread_signal(void) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = on_signal;
  sigemptyset(&action.sa_mask);
  action.sa_flags = 0; /* deliberately NOT SA_RESTART */
  if (sigaction(SIGUSR1, &action, NULL) != 0) {
    printf("waitid-thread-sibling-setup-failed sigaction errno=%d\n", errno);
    return 1;
  }

  pid_t target = fork();
  if (target < 0) {
    printf("waitid-thread-sibling-setup-failed fork errno=%d\n", errno);
    return 1;
  }
  if (target == 0) {
    for (;;) {
      pause();
    }
  }

  waitid_waiter_thread = pthread_self();
  pthread_t signaler;
  if (pthread_create(&signaler, NULL, thread_signaler, NULL) != 0) {
    printf("waitid-thread-sibling-setup-failed pthread_create errno=%d\n", errno);
    kill(target, SIGKILL);
    return 1;
  }

  siginfo_t info;
  memset(&info, 0, sizeof info);
  errno = 0;
  int rc = waitid(P_PID, target, &info, WEXITED);
  int captured = errno;

  printf("waitid-thread-sibling rc=%d errno=%d handler=%d\n", rc,
         rc < 0 ? captured : 0, handler_ran);

  pthread_join(signaler, NULL);
  kill(target, SIGKILL);
  int status = 0;
  waitpid(target, &status, 0);
  printf("waitid-thread-sibling-done\n");
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
  if (argc == 2 && strcmp(argv[1], "--live-sibling-signal") == 0) {
    return live_sibling_signal(0);
  }
  if (argc == 2 && strcmp(argv[1], "--live-sibling-signal-restart") == 0) {
    return live_sibling_signal(1);
  }
  if (argc == 2 && strcmp(argv[1], "--live-sibling-signal-blocked") == 0) {
    return live_sibling_signal(2);
  }
  if (argc == 2 && strcmp(argv[1], "--live-sibling-thread-signal") == 0) {
    return live_sibling_thread_signal();
  }
  fprintf(stderr, "usage: %s [--child-ready-wins|--signal-restart|--live-sibling-signal|--live-sibling-signal-restart|--live-sibling-signal-blocked|--live-sibling-thread-signal]\n", argv[0]);
  return 64;
}
