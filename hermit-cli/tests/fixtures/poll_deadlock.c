/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * The main thread reads a pipe whose only writer first waits on a futex that
 * no thread ever wakes: natively both threads wait forever.
 */

#define _GNU_SOURCE
#include <linux/futex.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

static uint32_t never_woken;
static int pipe_fds[2];

static void* writer(void* unused) {
  (void)unused;
  /* Waits while the word is 0; nothing changes it or wakes it. */
  syscall(SYS_futex, &never_woken, FUTEX_WAIT_PRIVATE, 0, NULL, NULL, 0);
  if (write(pipe_fds[1], "x", 1) != 1) {
    perror("write");
  }
  return NULL;
}

int main(void) {
  pthread_t thread;
  char byte;

  if (pipe(pipe_fds) != 0) {
    perror("pipe");
    return 1;
  }
  if (pthread_create(&thread, NULL, writer, NULL) != 0) {
    fputs("pthread_create failed\n", stderr);
    return 1;
  }
  printf("ready\n");
  fflush(stdout);
  ssize_t got = read(pipe_fds[0], &byte, 1);
  printf("UNREACHABLE: read returned %zd\n", got);
  return 0;
}
