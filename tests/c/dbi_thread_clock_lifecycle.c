/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static _Atomic int futex_word;
static _Atomic int child_failed;

static void* wait_for_parent(void* unused) {
  (void)unused;
  struct timespec deadline;
  if (clock_gettime(CLOCK_MONOTONIC, &deadline) != 0) {
    perror("clock_gettime");
    atomic_store(&child_failed, 1);
    return NULL;
  }
  deadline.tv_sec += 1;

  errno = 0;
  long result = syscall(
      SYS_futex,
      &futex_word,
      FUTEX_WAIT_BITSET_PRIVATE,
      0,
      &deadline,
      NULL,
      FUTEX_BITSET_MATCH_ANY);
  if (result != 0 && errno != EAGAIN) {
    perror("timed futex wait");
    atomic_store(&child_failed, 1);
  }
  return NULL;
}

int main(void) {
  pthread_t child;
  if (pthread_create(&child, NULL, wait_for_parent, NULL) != 0) {
    perror("pthread_create");
    return EXIT_FAILURE;
  }

  atomic_store(&futex_word, 1);
  if (syscall(SYS_futex, &futex_word, FUTEX_WAKE_PRIVATE, 1) < 0) {
    perror("futex wake");
    return EXIT_FAILURE;
  }
  if (pthread_join(child, NULL) != 0) {
    perror("pthread_join");
    return EXIT_FAILURE;
  }
  if (atomic_load(&child_failed)) {
    return EXIT_FAILURE;
  }

  puts("dbi-thread-clock-ok");
  return EXIT_SUCCESS;
}
