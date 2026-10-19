/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// select and pselect6 on more than 64 descriptors (the waited-on pipe is at
// descriptor 100), which Detcore leaves to the kernel as an external wait.
// Under DBT it runs through Detcore's restartable path
// (https://github.com/rrnewton/hermit/issues/4483): the wait mask, the
// timeout, STICKY_TIMEOUTS and the write-back are Detcore's. Each line is
// what the guest observes, natively and under Hermit alike.
//
// With an argument the guest instead waits while a helper thread forks. Under
// Hermit's DBT backend DynamoRIO's fork synch then interrupts the wait with
// its own signal on every retry, and Detcore must restart it each time; the
// test bounds the synch with DynamoRIO's -synch_all_threads_max_loops, so it
// lasts a few seconds instead of about 50:
//   fork         select with no timeout; the helper forks after 0.2 s, reaps
//                the child and writes
//   fork-sticky  STICKY_TIMEOUTS select with a 2 s timeout; the helper waits
//                3 s, then forks, reaps the child and writes, so the timeout
//                has expired, and is left as written, before the data arrives

#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/personality.h>
#include <sys/select.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define WAIT_FD 100

static int data_pipe[2];
static pid_t waiter_tid;
static volatile sig_atomic_t handled;

static void on_usr1(int signal) {
  (void)signal;
  handled++;
}

static void report(const char* label, long result, int error) {
  printf("%s: r=%ld", label, result);
  if (result < 0)
    printf(" errno=%s", strerror(error));
  printf("\n");
}

static void fresh_pipe(void) {
  if (pipe(data_pipe) != 0 || dup2(data_pipe[0], WAIT_FD) < 0)
    _exit(90);
  close(data_pipe[0]);
}

static void drain_and_close(void) {
  close(WAIT_FD);
  close(data_pipe[1]);
}

static long wait_select(struct timeval* tv) {
  fd_set fds;
  FD_ZERO(&fds);
  FD_SET(WAIT_FD, &fds);
  return syscall(SYS_select, WAIT_FD + 1, &fds, NULL, NULL, tv);
}

static long wait_pselect6(struct timespec* ts) {
  fd_set fds;
  FD_ZERO(&fds);
  FD_SET(WAIT_FD, &fds);
  return syscall(SYS_pselect6, WAIT_FD + 1, &fds, NULL, NULL, ts, NULL);
}

// The helper writes to the pipe, or signals the waiting thread, once the
// waiter has been in its wait for a while.
enum action { WRITE, SIGNAL_CHLD, SIGNAL_USR1_THEN_WRITE };
static enum action helper_action;

static void* helper(void* argument) {
  (void)argument;
  usleep(200000);
  switch (helper_action) {
    case WRITE:
      break;
    case SIGNAL_CHLD:
      syscall(SYS_tgkill, getpid(), waiter_tid, SIGCHLD);
      usleep(200000);
      break;
    case SIGNAL_USR1_THEN_WRITE:
      syscall(SYS_tgkill, getpid(), waiter_tid, SIGUSR1);
      usleep(200000);
      break;
  }
  if (write(data_pipe[1], "x", 1) != 1)
    _exit(91);
  return NULL;
}

// The helper installs the SIGUSR1 handler while the waiter waits with SIGUSR1
// ignored, then signals the waiter; nothing is ever written.
static void* install_then_signal(void* argument) {
  (void)argument;
  usleep(200000);
  signal(SIGUSR1, on_usr1);
  syscall(SYS_tgkill, getpid(), waiter_tid, SIGUSR1);
  return NULL;
}

static void with_handler_installed_during_the_wait(void) {
  fresh_pipe();
  signal(SIGUSR1, SIG_IGN);
  waiter_tid = (pid_t)syscall(SYS_gettid);
  pthread_t thread;
  if (pthread_create(&thread, NULL, install_then_signal, NULL) != 0)
    _exit(92);
  struct timeval tv = {2, 0};
  errno = 0;
  long result = wait_select(&tv);
  int error = errno;
  pthread_join(thread, NULL);
  report("select, SIGUSR1 ignored, then caught and sent during the wait", result, error);
  drain_and_close();
}

static void with_helper(const char* label, enum action action) {
  fresh_pipe();
  helper_action = action;
  waiter_tid = (pid_t)syscall(SYS_gettid);
  pthread_t thread;
  if (pthread_create(&thread, NULL, helper, NULL) != 0)
    _exit(92);
  errno = 0;
  long result = wait_select(NULL);
  int error = errno;
  pthread_join(thread, NULL);
  report(label, result, error);
  drain_and_close();
}

static useconds_t fork_delay;

static void* fork_helper(void* argument) {
  (void)argument;
  usleep(fork_delay);
  pid_t child = fork();
  if (child < 0)
    _exit(94);
  if (child == 0)
    _exit(3);
  int status;
  if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 3)
    _exit(96);
  if (write(data_pipe[1], "x", 1) != 1)
    _exit(91);
  return NULL;
}

static int through_a_fork(int sticky) {
  fresh_pipe();
  if (sticky) {
    int persona = personality(0xffffffff);
    if (persona == -1 || personality(persona | STICKY_TIMEOUTS) == -1)
      return 93;
  }
  fork_delay = sticky ? 3000000 : 200000;
  pthread_t thread;
  if (pthread_create(&thread, NULL, fork_helper, NULL) != 0)
    _exit(92);
  struct timeval tv = {2, 0};
  errno = 0;
  long result = wait_select(sticky ? &tv : NULL);
  int error = errno;
  pthread_join(thread, NULL);
  if (sticky) {
    report("sticky select during a fork, 2 s timeout", result, error);
    printf("sticky select timeout after: %ld.%06ld\n", (long)tv.tv_sec, (long)tv.tv_usec);
  } else {
    report("select during a fork, then data", result, error);
  }
  drain_and_close();
  return 0;
}

int main(int argc, char** argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  if (argc == 2 && strcmp(argv[1], "fork") == 0)
    return through_a_fork(0);
  if (argc == 2 && strcmp(argv[1], "fork-sticky") == 0)
    return through_a_fork(1);
  if (argc != 1)
    return 95;
  signal(SIGUSR1, on_usr1);

  // Data arrives during the wait.
  with_helper("select, data arrives", WRITE);
  // A default-ignored SIGCHLD does not end the wait.
  with_helper("select, SIGCHLD at its default, then data", SIGNAL_CHLD);
  // A caught signal ends it with EINTR, its handler run once.
  with_helper("select, caught SIGUSR1", SIGNAL_USR1_THEN_WRITE);
  printf("SIGUSR1 handled: %d\n", (int)handled);
  // A handler installed by another thread during the wait, then a signal, ends
  // it too: the action is the one in force when the signal arrives.
  with_handler_installed_during_the_wait();
  printf("SIGUSR1 handled: %d\n", (int)handled);

  // Timeouts expire, and the time left is written back as Linux writes it.
  fresh_pipe();
  struct timeval tv = {0, 300000};
  errno = 0;
  long result = wait_select(&tv);
  report("select, 0.3 s timeout", result, errno);
  printf("select time left: %ld.%06ld\n", (long)tv.tv_sec, (long)tv.tv_usec);
  struct timespec ts = {0, 300000000};
  errno = 0;
  result = wait_pselect6(&ts);
  report("pselect6, 0.3 s timeout", result, errno);
  printf("pselect6 time left: %ld.%09ld\n", (long)ts.tv_sec, (long)ts.tv_nsec);
  struct timeval folded = {0, 1300000};
  errno = 0;
  result = wait_select(&folded);
  report("select, usec 1300000", result, errno);
  struct timeval negative = {0, -1};
  errno = 0;
  result = wait_select(&negative);
  report("select, usec -1", result, errno);

  // Under STICKY_TIMEOUTS the kernel leaves the timeout as it was.
  int persona = personality(0xffffffff);
  if (persona == -1 || personality(persona | STICKY_TIMEOUTS) == -1)
    return 93;
  struct timeval sticky_tv = {0, 300000};
  errno = 0;
  result = wait_select(&sticky_tv);
  report("sticky select, 0.3 s timeout", result, errno);
  printf("sticky select timeout after: %ld.%06ld\n", (long)sticky_tv.tv_sec,
         (long)sticky_tv.tv_usec);
  struct timespec sticky_ts = {0, 300000000};
  errno = 0;
  result = wait_pselect6(&sticky_ts);
  report("sticky pselect6, 0.3 s timeout", result, errno);
  printf("sticky pselect6 timeout after: %ld.%09ld\n", (long)sticky_ts.tv_sec,
         (long)sticky_ts.tv_nsec);
  drain_and_close();
  return 0;
}
