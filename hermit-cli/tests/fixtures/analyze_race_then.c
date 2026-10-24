/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A race whose losing order cannot finish, for `hermit analyze`'s trials.
// Two threads race to claim `winner`, and the program prints which one did.
// When "b" won, it exits 0. When "a" won (the order the scheduler takes
// without chaos), it then does what argv[1] says: "spin" yields for ever.
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>

static atomic_int winner;

static void* claim(void* name) {
  int unclaimed = 0;
  atomic_compare_exchange_strong(&winner, &unclaimed, *(const char*)name);
  return NULL;
}

int main(int argc, char** argv) {
  const char* then = argc > 1 ? argv[1] : "";
  pthread_t a, b;
  pthread_create(&a, NULL, claim, "a");
  pthread_create(&b, NULL, claim, "b");
  pthread_join(a, NULL);
  pthread_join(b, NULL);
  printf("%c-first\n", winner);
  fflush(stdout);
  if (winner == 'b') {
    return 0;
  }
  if (strcmp(then, "spin") == 0) {
    for (;;) {
      sched_yield();
    }
  }
  return 1;
}
