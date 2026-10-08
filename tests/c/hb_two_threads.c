/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Two threads of one process each write one line to stdout: a pthread worker
 * writes "worker\n" (7 bytes) and the main thread writes "main!\n" (6 bytes)
 * and then joins it. hermit-cli/tests/cli.rs uses it to check that a
 * --happens-before edge between two threads of one process (the worker's
 * dettid differs from its process's detpid) reverses their order.
 */
#include <pthread.h>
#include <unistd.h>

static void *worker(void *arg) {
  (void)arg;
  if (write(1, "worker\n", 7) != 7) {
    _exit(2);
  }
  return 0;
}

int main(void) {
  pthread_t thread;
  if (pthread_create(&thread, 0, worker, 0) != 0) {
    return 3;
  }
  if (write(1, "main!\n", 6) != 6) {
    return 2;
  }
  return pthread_join(thread, 0) == 0 ? 0 : 4;
}
