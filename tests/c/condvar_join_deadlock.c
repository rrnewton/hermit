/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A guest that deadlocks: main prints a marker, then joins a thread that
// waits on a condition variable nobody signals. Natively the process hangs
// forever; under Hermit the scheduler proves the hang and the run ends with
// the guest-deadlock outcome. The marker shows the output written before the
// deadlock is kept.
#include <pthread.h>
#include <stdio.h>

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t never_signaled = PTHREAD_COND_INITIALIZER;
static int signaled;

static void* waiter(void* arg) {
  (void)arg;
  pthread_mutex_lock(&lock);
  while (!signaled) {
    pthread_cond_wait(&never_signaled, &lock);
  }
  pthread_mutex_unlock(&lock);
  return NULL;
}

int main(void) {
  pthread_t thread;
  if (pthread_create(&thread, NULL, waiter, NULL) != 0) {
    return 2;
  }
  printf("main: joining a thread that waits forever\n");
  fflush(stdout);
  pthread_join(thread, NULL);
  printf("UNREACHABLE: the condition variable was signaled\n");
  return 0;
}
