/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * `sigsuspend(2)` ended by a sibling thread's `tgkill(2)`, while a signal
 * that the process ignores arrives first. The main thread keeps SIGUSR2
 * blocked, catches it only inside `sigsuspend` with an empty mask, and a
 * sibling sends it three seconds later. Before that, a signal whose
 * disposition discards it reaches the waiting thread:
 *
 *   ignored-alarm: SIGALRM is SIG_IGN and `alarm(1)` fires during the wait.
 *   ignored-chld:  SIGCHLD keeps SIG_DFL, which ignores it, and a forked child
 *                  exits after one second.
 *
 * Linux discards the ignored signal without ending the wait, so the wait ends
 * only with the sibling's SIGUSR2: -1 with EINTR and one handler run on the
 * waiting thread.
 *
 * Under ptrace, even an ignored signal stops the tracee, so the waiting
 * thread reports its interrupted syscall to Detcore. Two defects made that
 * report nondeterministic or lost:
 *
 * 1. The sibling's `tgkill` only marked the scheduler's `rt_sigsuspend` pool
 *    entry as released. With nothing else runnable, the scheduler reported a
 *    terminal deadlock or advanced virtual time before the waiter's report
 *    arrived, and the run ended with exit 125 and no verify result. Measured
 *    on main at 59967740ec: ignored-alarm hung in 4 of 4 runs and ignored-chld
 *    in 3 of 4.
 * 2. Detcore injected probe syscalls (`rt_sigprocmask` to read the suspend
 *    mask, `rt_sigpending`) ahead of the real `rt_sigsuspend`, so it ran from
 *    Reverie's private page instead of in place. The ignored signal then
 *    landed either before that instruction (ERESTARTSYS, no signal event) or
 *    inside it (ERESTARTNOHAND plus a signal event), depending on host timing.
 *    With only the first defect fixed, strict verify of ignored-alarm diverged
 *    in 7 of 10 runs.
 *
 * Both defects lose a host-timing race only some of the time: measured with
 * each restored and the two phases run once, a strict verify caught the first
 * in 3 of 9 runs and the second in 3 of 8. The phases therefore repeat ROUNDS
 * times in one run, so a single CI run meets either race many times. In ten
 * harness verify runs per setting, the lost requeue failed 1 run at 1 round, 5
 * at 5 and 9 at 25; the injected probes failed 2, 7 and 10. The fixed tree
 * passed all 30 runs.
 *
 * `sigsuspend_alarm_wake.c` covers the scheduler's own alarm waking a
 * `sigsuspend` whose mask admits a caught SIGALRM; this guest covers the wake
 * that comes from another guest thread.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define ROUNDS 25

static volatile sig_atomic_t usr2_runs = 0;
static volatile sig_atomic_t usr2_on_waiter = 0;
static pid_t waiter_tid;

static void on_usr2(int signo) {
  (void)signo;
  usr2_runs++;
  usr2_on_waiter = (pid_t)syscall(SYS_gettid) == waiter_tid;
}

/* Sleeps past the ignored signal, then wakes the waiting thread. */
static void* sibling(void* arg) {
  (void)arg;
  struct timespec three_seconds = {3, 0};
  nanosleep(&three_seconds, NULL);
  syscall(SYS_tgkill, getpid(), waiter_tid, SIGUSR2);
  return NULL;
}

/* Returns 0 on success. */
static int phase(const char* name, int fork_child) {
  usr2_runs = 0;
  usr2_on_waiter = 0;
  pid_t child = -1;
  if (fork_child) {
    child = fork();
    if (child < 0) {
      puts("SIGSUSPEND_SIBLING_FORK_FAILED");
      return 1;
    }
    if (child == 0) {
      struct timespec one_second = {1, 0};
      nanosleep(&one_second, NULL);
      _exit(7);
    }
  }

  pthread_t thread;
  if (pthread_create(&thread, NULL, sibling, NULL) != 0) {
    puts("SIGSUSPEND_SIBLING_PTHREAD_CREATE_FAILED");
    return 1;
  }
  if (!fork_child) {
    alarm(1);
  }

  sigset_t empty;
  sigemptyset(&empty);
  errno = 0;
  int rc = sigsuspend(&empty);
  int eintr = rc == -1 && errno == EINTR;

  if (pthread_join(thread, NULL) != 0) {
    puts("SIGSUSPEND_SIBLING_PTHREAD_JOIN_FAILED");
    return 1;
  }
  int child_status = -1;
  if (fork_child) {
    int status = 0;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status)) {
      puts("SIGSUSPEND_SIBLING_WAITPID_FAILED");
      return 1;
    }
    child_status = WEXITSTATUS(status);
  }

  /* Report booleans and counts rather than strerror(3) so the output cannot
   * depend on the ambient locale. */
  printf(
      "%s rc=%d eintr=%d usr2_runs=%d usr2_on_waiter=%d child_status=%d\n",
      name,
      rc,
      eintr,
      (int)usr2_runs,
      (int)usr2_on_waiter,
      child_status);
  return !(rc == -1 && eintr && usr2_runs == 1 && usr2_on_waiter &&
           child_status == (fork_child ? 7 : -1));
}

int main(void) {
  struct sigaction sa;
  memset(&sa, 0, sizeof(sa));
  sigemptyset(&sa.sa_mask);
  sa.sa_handler = on_usr2;
  if (sigaction(SIGUSR2, &sa, NULL) != 0) {
    puts("SIGSUSPEND_SIBLING_SIGACTION_FAILED");
    return 1;
  }
  sa.sa_handler = SIG_IGN;
  if (sigaction(SIGALRM, &sa, NULL) != 0) {
    puts("SIGSUSPEND_SIBLING_SIGACTION_FAILED");
    return 1;
  }

  sigset_t usr2;
  sigemptyset(&usr2);
  sigaddset(&usr2, SIGUSR2);
  if (sigprocmask(SIG_BLOCK, &usr2, NULL) != 0) {
    puts("SIGSUSPEND_SIBLING_SIGPROCMASK_FAILED");
    return 1;
  }
  waiter_tid = (pid_t)syscall(SYS_gettid);

  for (int round = 0; round < ROUNDS; round++) {
    if (phase("ignored-alarm", 0) != 0 || phase("ignored-chld", 1) != 0) {
      puts("SIGSUSPEND_SIBLING_SIGNAL_WAKE_FAILED");
      return 1;
    }
  }
  puts("SIGSUSPEND_SIBLING_SIGNAL_WAKE_OK");
  return 0;
}
