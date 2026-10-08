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
 * report lost or nondeterministic:
 *
 * 1. The sibling's `tgkill` did not arm the waiter in the scheduler's
 *    `rt_sigsuspend` pool for the release barrier, which waits for that
 *    thread's own report. With nothing else runnable, the scheduler reported
 *    a terminal deadlock before the report arrived, and the run ended with
 *    exit 125 and no verify result. The scheduler now arms the waiter when a
 *    sibling's signal can wake it.
 * 2. Still open. Detcore reads the suspend mask with an injected probe and
 *    passes the real `rt_sigsuspend` its own copy of that mask, so the real
 *    call runs injected from Reverie's private page instead of in place. A
 *    signal that the thread's mask outside the call does not block, like the
 *    ignored SIGALRM and SIGCHLD here, then stops the tracee either before
 *    that instruction (ERESTARTSYS) or inside it (ERESTARTNOHAND), depending
 *    on host timing, and strict verify diverges at the `rt_sigsuspend`
 *    record although the guest's output is the same either way.
 *
 * Defect 1 lost a host-timing race only some of the time: on a tree without
 * its fix, 5 of 13 strict verify runs deadlocked with each phase run once,
 * and 5 of 6 with 25 rounds (the sixth completed and diverged by defect 2).
 * The phases therefore repeat ROUNDS times in one run, so that a single CI
 * run meets the race many times.
 *
 * The raw phases pass `rt_sigsuspend(2)` a kernel-sized 64-bit mask through
 * `syscall(2)`, so no C library copy stands between the kernel and a word
 * another thread stores to:
 *
 *   raw-admitted: the word admits SIGUSR1, and a sibling's SIGUSR1 ends the
 *                 wait with EINTR and one handler run on the waiting thread.
 *   raw-blocked:  a sibling stores a word that blocks SIGUSR1 before the
 *                 waiter enters. Its SIGUSR1 must stay pending without ending
 *                 the wait, and its SIGUSR2 a second later ends it.
 *   raw-race:     a sibling stores the blocking word and sends SIGUSR1 while
 *                 the waiter is entering the call, and sends SIGUSR2 a second
 *                 later. Linux installs whichever word the kernel copies, so
 *                 either signal may end the wait; the phase prints only what
 *                 holds either way, and the run must not hang.
 *   raw-stale:    as raw-race, but the sibling waits a second after its store
 *                 before it sends SIGUSR1, and another before SIGUSR2.
 *
 * Detcore reads the word in the waiter's turn and passes the real call its
 * own copy of it, which never overlaps the caller's buffer, so the kernel
 * installs the mask that Detcore read and recorded even when a sibling stores
 * to the word afterwards. The scheduler decides from that recorded mask
 * whether a sibling's signal can wake the waiter, and when it can, it runs no
 * other guest thread until the waiter's own report arrives. A store therefore
 * changes the wait only if it lands before Detcore's read; raw-race and
 * raw-stale accept either outcome, and the run must not hang. The raw phases
 * keep SIGUSR1 and SIGUSR2 blocked outside the call, so neither signal can
 * stop the waiter before the real call starts, and defect 2 does not reach
 * them.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define ROUNDS 25
#ifndef RAW_ROUNDS
#define RAW_ROUNDS ROUNDS
#endif

#define SIGNAL_BIT(sig) (UINT64_C(1) << ((sig) - 1))

static volatile sig_atomic_t usr1_runs = 0;
static volatile sig_atomic_t usr1_on_waiter = 0;
static volatile sig_atomic_t usr2_runs = 0;
static volatile sig_atomic_t usr2_on_waiter = 0;
static pid_t waiter_tid;

/* The raw phases' temporary mask and their handshakes with the sibling. */
static _Atomic uint64_t raw_mask;
static atomic_int sibling_ready;
static atomic_int waiter_entering;

static void on_usr1(int signo) {
  (void)signo;
  usr1_runs++;
  usr1_on_waiter = (pid_t)syscall(SYS_gettid) == waiter_tid;
}

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

static void sleep_one_second(void) {
  struct timespec one_second = {1, 0};
  nanosleep(&one_second, NULL);
}

static void* send_usr1(void* arg) {
  (void)arg;
  sleep_one_second();
  syscall(SYS_tgkill, getpid(), waiter_tid, SIGUSR1);
  return NULL;
}

static void* block_usr1_before_wait(void* arg) {
  (void)arg;
  atomic_store(&raw_mask, SIGNAL_BIT(SIGUSR1));
  atomic_store(&sibling_ready, 1);
  sleep_one_second();
  syscall(SYS_tgkill, getpid(), waiter_tid, SIGUSR1);
  sleep_one_second();
  syscall(SYS_tgkill, getpid(), waiter_tid, SIGUSR2);
  return NULL;
}

/* Yields rather than spinning: under Hermit a spinning thread runs until the
 * precise timer preempts it, and a strict verify refuses a run in which that
 * timer overshot its target. */
static void wait_for_waiter_entering(void) {
  while (!atomic_load(&waiter_entering)) {
    sched_yield();
  }
}

static void* block_usr1_during_wait(void* arg) {
  (void)arg;
  atomic_store(&sibling_ready, 1);
  wait_for_waiter_entering();
  atomic_store(&raw_mask, SIGNAL_BIT(SIGUSR1));
  syscall(SYS_tgkill, getpid(), waiter_tid, SIGUSR1);
  sleep_one_second();
  syscall(SYS_tgkill, getpid(), waiter_tid, SIGUSR2);
  return NULL;
}

static void* block_usr1_after_entry(void* arg) {
  (void)arg;
  atomic_store(&sibling_ready, 1);
  wait_for_waiter_entering();
  atomic_store(&raw_mask, SIGNAL_BIT(SIGUSR1));
  sleep_one_second();
  syscall(SYS_tgkill, getpid(), waiter_tid, SIGUSR1);
  sleep_one_second();
  syscall(SYS_tgkill, getpid(), waiter_tid, SIGUSR2);
  return NULL;
}

/* Lets the main thread's pending SIGUSR1 and SIGUSR2 run their handlers. */
static int run_pending_handlers(void) {
  sigset_t both;
  sigemptyset(&both);
  sigaddset(&both, SIGUSR1);
  sigaddset(&both, SIGUSR2);
  return sigprocmask(SIG_UNBLOCK, &both, NULL) != 0 ||
      sigprocmask(SIG_BLOCK, &both, NULL) != 0;
}

/* Returns 0 on success. */
static int raw_phase(const char* name, void* (*sibling_main)(void*)) {
  usr1_runs = 0;
  usr1_on_waiter = 0;
  usr2_runs = 0;
  usr2_on_waiter = 0;
  atomic_store(&raw_mask, 0);
  atomic_store(&sibling_ready, 0);
  atomic_store(&waiter_entering, 0);

  pthread_t thread;
  if (pthread_create(&thread, NULL, sibling_main, NULL) != 0) {
    puts("SIGSUSPEND_SIBLING_PTHREAD_CREATE_FAILED");
    return 1;
  }
  if (sibling_main != send_usr1) {
    while (!atomic_load(&sibling_ready)) {
      sched_yield();
    }
  }
  atomic_store(&waiter_entering, 1);
  errno = 0;
  long rc = syscall(SYS_rt_sigsuspend, (void*)&raw_mask, sizeof(uint64_t));
  int eintr = rc == -1 && errno == EINTR;
  int usr1_in_wait = usr1_runs;
  int usr2_in_wait = usr2_runs;

  if (pthread_join(thread, NULL) != 0) {
    puts("SIGSUSPEND_SIBLING_PTHREAD_JOIN_FAILED");
    return 1;
  }
  sigset_t pending;
  sigemptyset(&pending);
  if (sigpending(&pending) != 0) {
    puts("SIGSUSPEND_SIBLING_SIGPENDING_FAILED");
    return 1;
  }
  int usr1_pending = sigismember(&pending, SIGUSR1);
  int usr2_pending = sigismember(&pending, SIGUSR2);
  if (run_pending_handlers() != 0) {
    puts("SIGSUSPEND_SIBLING_SIGPROCMASK_FAILED");
    return 1;
  }
  int on_waiter = usr1_on_waiter && (usr2_runs == 0 || usr2_on_waiter);

  if (sibling_main == block_usr1_during_wait ||
      sibling_main == block_usr1_after_entry) {
    /* Either signal may end the wait, and the other one stays pending until
     * the handlers run above. */
    printf(
        "%s rc=%ld eintr=%d woke_once=%d left_pending=%d usr1_runs=%d "
        "usr2_runs=%d on_waiter=%d\n",
        name,
        rc,
        eintr,
        usr1_in_wait + usr2_in_wait == 1,
        usr1_pending + usr2_pending,
        (int)usr1_runs,
        (int)usr2_runs,
        on_waiter);
    return !(rc == -1 && eintr && usr1_in_wait + usr2_in_wait == 1 &&
             usr1_pending + usr2_pending == 1 && usr1_runs == 1 &&
             usr2_runs == 1 && on_waiter);
  }
  printf(
      "%s rc=%ld eintr=%d usr1_in_wait=%d usr2_in_wait=%d usr1_pending=%d "
      "usr2_pending=%d usr1_runs=%d usr2_runs=%d on_waiter=%d\n",
      name,
      rc,
      eintr,
      usr1_in_wait,
      usr2_in_wait,
      usr1_pending,
      usr2_pending,
      (int)usr1_runs,
      (int)usr2_runs,
      on_waiter);
  if (sibling_main == send_usr1) {
    return !(rc == -1 && eintr && usr1_in_wait == 1 && usr2_in_wait == 0 &&
             usr1_pending == 0 && usr2_pending == 0 && usr1_runs == 1 &&
             usr2_runs == 0 && on_waiter);
  }
  return !(rc == -1 && eintr && usr1_in_wait == 0 && usr2_in_wait == 1 &&
           usr1_pending == 1 && usr2_pending == 0 && usr1_runs == 1 &&
           usr2_runs == 1 && on_waiter);
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
  sa.sa_handler = on_usr1;
  if (sigaction(SIGUSR1, &sa, NULL) != 0) {
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
  /* The raw phases leave SIGUSR1 pending between the wait and their check. */
  sigaddset(&usr2, SIGUSR1);
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
  for (int round = 0; round < RAW_ROUNDS; round++) {
    if (raw_phase("raw-admitted", send_usr1) != 0 ||
        raw_phase("raw-blocked", block_usr1_before_wait) != 0 ||
        raw_phase("raw-race", block_usr1_during_wait) != 0 ||
        raw_phase("raw-stale", block_usr1_after_entry) != 0) {
      puts("SIGSUSPEND_SIBLING_SIGNAL_WAKE_FAILED");
      return 1;
    }
  }
  puts("SIGSUSPEND_SIBLING_SIGNAL_WAKE_OK");
  return 0;
}
