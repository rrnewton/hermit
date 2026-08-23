/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <pthread.h>
#include <stdio.h>

enum { THREADS = 4 };

static void* thread_main(void* argument) {
  int* value = argument;
  *value += 1;
  return NULL;
}

/* Each thread adds 1 to its own slot, so the total is the sum of the seeds plus
   one per thread: (0+1+2+3) + 4. Printed but not asserted, a thread that never
   ran its body left the total lower and the guest still exited 0 -- and under
   --verify both runs lower it identically, so the comparison still matched. */
#define EXPECTED_TOTAL 10

int main(void) {
  pthread_t threads[THREADS];
  int values[THREADS] = {0, 1, 2, 3};
  int total = 0;
  int created = 0;

  for (int index = 0; index < THREADS; ++index) {
    if (pthread_create(&threads[index], NULL, thread_main, &values[index]) != 0) {
      break;
    }
    created++;
  }
  /* Join every thread that WAS created, including on the failure path.
     Returning straight out of main used to leave already-running threads
     writing into `values`, which lives in this frame -- the frame being torn
     down around them. That is undefined, and which way it breaks depends on the
     scheduler interleaving, which is the one thing this project exists to pin
     down. Join first, then report. */
  for (int index = 0; index < created; ++index) {
    if (pthread_join(threads[index], NULL) != 0) {
      return 2;
    }
    total += values[index];
  }
  if (created != THREADS) {
    fprintf(stderr, "pthread_lifecycle created %d of %d threads\n", created,
            THREADS);
    return 1;
  }

  printf("threads=%d total=%d\n", THREADS, total);

  /* Route a behavioural failure into the exit status. */
  if (total != EXPECTED_TOTAL) {
    fprintf(stderr, "pthread_lifecycle total %d, expected %d\n", total,
            EXPECTED_TOTAL);
    return 1;
  }
  return 0;
}
