/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>

/*
  For some reason this triggers registry divergence under trace/replay usecase
  in hermit-verify when detlogs are taken for consideration

  The theory behind why "printf" is doing it is unclear but it looks to be
  connected with jemalloc behavior as the divergence is happening on jemalloc
  thread for 'gettimeofday' syscall
*/

void* thread1(void* vargp) {
  printf("thread 1\n");
  return NULL;
}

int main() {
  enum { EXPECTED_CHECKS = 2 };
  int ok = 0;
  pthread_t thread;

  if (pthread_create(&thread, NULL, thread1, NULL) == 0) {
    ok++;
  }
  if (pthread_join(thread, NULL) == 0) {
    ok++;
  }

  return ok == EXPECTED_CHECKS ? EXIT_SUCCESS : EXIT_FAILURE;
}
