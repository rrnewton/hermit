/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Two threads pass a turn back and forth through a condition variable, each
// doing a fixed amount of work per turn. Under --chaos with a PMU-backed
// --max-timeslice, a recording carries dozens of RCB-anchored preemption
// points per thread, so a test can edit a recording and replay it
// (https://github.com/rrnewton/hermit/issues/4497).

#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>

static pthread_mutex_t m = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static int turn;
static volatile unsigned long sink;

static void work(unsigned long n) {
  for (unsigned long i = 0; i < n; i++) {
    sink += i * 2654435761u;
  }
}

static void wait_for(int who) {
  pthread_mutex_lock(&m);
  while (turn != who) {
    pthread_cond_wait(&cv, &m);
  }
  pthread_mutex_unlock(&m);
}

static void pass_to(int who) {
  pthread_mutex_lock(&m);
  turn = who;
  pthread_cond_broadcast(&cv);
  pthread_mutex_unlock(&m);
}

static void* b_main(void* arg) {
  (void)arg;
  for (int round = 0; round < 6; round++) {
    wait_for(1);
    work(40000);
    pass_to(0);
  }
  return NULL;
}

int main(void) {
  pthread_t b;
  pthread_create(&b, NULL, b_main, NULL);
  for (int round = 0; round < 6; round++) {
    work(40000);
    pass_to(1);
    wait_for(0);
  }
  pthread_join(b, NULL);
  printf("done %d\n", sink != 0);
  return 0;
}
