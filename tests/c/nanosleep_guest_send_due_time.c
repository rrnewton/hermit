/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A guest-sent caught signal that interrupts a short nanosleep reports the
 * remaining time from when the send was due, not from whenever the
 * scheduler next runs. Child C parks in a 150 ms nanosleep, child B in a
 * 100 ms one: both deadlines fall before the first scheduler pass after
 * the kill, which Detcore's default --max-timeslice (200 ms of virtual
 * time) bounds. The parent signals C with a caught SIGUSR1 and then
 * computes in the same turn, so the pass that can wake C comes after both
 * deadlines. Linux
 * interrupted C's sleep at the send, before its deadline, so it returns
 * EINTR with a positive remaining time; it never returns EINTR with no time
 * left (do_nanosleep returns 0 then). B simply expires.
 *
 * Each child reports through its exit status only and the parent prints
 * both outcomes in a fixed order, so the output does not depend on which
 * child happened to print first or on any pid.
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

/* Child exit codes. */
#define CHILD_SLEPT_THROUGH 0
#define CHILD_EINTR_TIME_LEFT 10
#define CHILD_EINTR_NO_TIME_LEFT 20
#define CHILD_SLEPT_THROUGH_HANDLED 30
#define CHILD_SETUP_FAILED 40

static volatile sig_atomic_t handled = 0;

static void on_usr1(int signo) {
  (void)signo;
  handled++;
}

static void sleep_child(int wfd, long ns, int caught) {
  if (caught) {
    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = on_usr1;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGUSR1, &sa, NULL) != 0) {
      _exit(CHILD_SETUP_FAILED);
    }
  }
  char ready = 'r';
  if (write(wfd, &ready, 1) != 1) {
    _exit(CHILD_SETUP_FAILED);
  }
  const struct timespec length = {ns / 1000000000L, ns % 1000000000L};
  struct timespec rem = {0, 0};
  int result = nanosleep(&length, &rem);
  int error = errno;
  int left = rem.tv_sec > 0 || rem.tv_nsec > 0;
  if (result == 0 && handled == 0) {
    _exit(CHILD_SLEPT_THROUGH);
  }
  if (result == 0 && handled == 1) {
    _exit(CHILD_SLEPT_THROUGH_HANDLED);
  }
  if (result == -1 && error == EINTR && handled == 1 && left) {
    _exit(CHILD_EINTR_TIME_LEFT);
  }
  if (result == -1 && error == EINTR && handled == 1 && !left) {
    _exit(CHILD_EINTR_NO_TIME_LEFT);
  }
  _exit(CHILD_SETUP_FAILED);
}

static int reap(pid_t pid) {
  int status = 0;
  if (waitpid(pid, &status, 0) != pid) {
    perror("waitpid");
    exit(2);
  }
  if (!WIFEXITED(status)) {
    return -1;
  }
  return WEXITSTATUS(status);
}

int main(void) {
  setvbuf(stdout, NULL, _IOLBF, 0);
  int ready_pipe[2];
  if (pipe(ready_pipe) != 0) {
    perror("pipe");
    return 1;
  }
  pid_t b = fork();
  if (b < 0) {
    perror("fork");
    return 1;
  }
  if (b == 0) {
    close(ready_pipe[0]);
    sleep_child(ready_pipe[1], 100000000, 1);
  }
  pid_t c = fork();
  if (c < 0) {
    perror("fork");
    return 1;
  }
  if (c == 0) {
    close(ready_pipe[0]);
    sleep_child(ready_pipe[1], 150000000, 1);
  }
  close(ready_pipe[1]);
  char ready[2];
  size_t got = 0;
  while (got < 2) {
    ssize_t n = read(ready_pipe[0], ready + got, 2 - got);
    if (n <= 0) {
      fprintf(stderr, "the children never got ready\n");
      return 1;
    }
    got += (size_t)n;
  }
  /* Both children reach their sleeps meanwhile. */
  const struct timespec tiny = {0, 1000};
  nanosleep(&tiny, NULL);
  if (kill(c, SIGUSR1) != 0) {
    perror("kill");
    return 1;
  }
  /* Stay in this turn well past both deadlines, so the next scheduler pass
   * that can wake C computes its remaining time after its deadline unless
   * it uses the send's due time. */
  volatile unsigned long x = 0;
  for (long i = 0; i < 200000000L; i++) {
    x += (unsigned long)i;
  }
  (void)x;
  int b_status = reap(b);
  int c_status = reap(c);
  printf(
      "B: nanosleep %s\n",
      b_status == CHILD_SLEPT_THROUGH         ? "returned 0, rem positive no, handler ran 0"
          : "unexpected outcome");
  printf(
      "C: nanosleep %s\n",
      c_status == CHILD_EINTR_TIME_LEFT          ? "EINTR, rem positive yes, handler ran 1"
          : c_status == CHILD_EINTR_NO_TIME_LEFT ? "EINTR, rem positive no, handler ran 1"
          : c_status == CHILD_SLEPT_THROUGH_HANDLED
              ? "returned 0, rem positive no, handler ran 1"
              : "unexpected outcome");
  puts("parent: done");
  return 0;
}
