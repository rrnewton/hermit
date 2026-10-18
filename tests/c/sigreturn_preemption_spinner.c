/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A thread that returns from a signal handler and then runs without a
 * syscall must still be preempted (A5 step 2c,
 * https://github.com/rrnewton/hermit/issues/4485).
 *
 * The main thread signals itself K times. Iteration i's handler retires
 * about BASE + i * STEP conditional branches and returns; the main thread
 * then spins, with no syscall, until the worker acknowledges iteration i.
 * The worker spins without syscalls too, so under Hermit's sequentialized
 * scheduler each side runs only once the PMU preemption timer preempts the
 * other. With Detcore's signal-state copy on, ptrace reports each handler
 * return, and before step 2c a return within Reverie's keep margin (10005
 * branches) of the slice's timer target cancelled the event for good: that
 * iteration spun for ever. STEP is below the margin, and the cell's
 * --max-timeslice=200000 (20,000 branches) is below K * STEP, so some
 * return lands there wherever the slice ends.
 */
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

#define K 12
#define BASE 1000L
#define STEP 4001L

static volatile long req = -1;
static volatile long ack = -1;
static volatile long spin_for;
static volatile long sink;

static void handler(int sig) {
  (void)sig;
  for (volatile long j = 0; j < spin_for; j++)
    sink++;
}

static void* worker(void* arg) {
  (void)arg;
  while (ack < K - 1) {
    long r = req;
    if (r > ack)
      ack = r;
  }
  return NULL;
}

int main(void) {
  struct sigaction sa = {0};
  sa.sa_handler = handler;
  if (sigaction(SIGUSR1, &sa, NULL) != 0)
    return 2;
  pthread_t t;
  if (pthread_create(&t, NULL, worker, NULL) != 0)
    return 3;
  pid_t pid = getpid();
  pid_t tid = (pid_t)syscall(SYS_gettid);
  for (long i = 0; i < K; i++) {
    spin_for = BASE + i * STEP;
    if (syscall(SYS_tgkill, pid, tid, SIGUSR1) != 0)
      return 4;
    req = i;
    while (ack < i)
      ;
  }
  if (pthread_join(t, NULL) != 0)
    return 5;
  printf("done %d iterations\n", K);
  return 0;
}
