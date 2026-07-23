/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A deliberate data race: many threads increment a shared counter WITHOUT
 * synchronization. Run natively, lost updates make the final total vary from
 * run to run (and it depends on the OS thread interleaving). Under Hermit the
 * scheduler is deterministic, so every run prints the same total.
 *
 * This is a demonstration of scheduling nondeterminism, not correct code.
 */

#include <pthread.h>
#include <stdio.h>

enum { THREADS = 8, ITERATIONS = 200000 };

/* Deliberately non-atomic and unsynchronized to expose the race. */
static volatile long shared_counter = 0;

static void* worker(void* unused) {
  (void)unused;
  for (int i = 0; i < ITERATIONS; i++) {
    /* Read-modify-write without a lock: updates are lost under contention. */
    long value = shared_counter;
    shared_counter = value + 1;
  }
  return NULL;
}

int main(void) {
  pthread_t threads[THREADS];
  for (int t = 0; t < THREADS; t++) {
    if (pthread_create(&threads[t], NULL, worker, NULL) != 0) {
      return 1;
    }
  }
  for (int t = 0; t < THREADS; t++) {
    pthread_join(threads[t], NULL);
  }
  printf("counter=%ld\n", shared_counter);
  return 0;
}
