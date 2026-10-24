/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Polls a FIFO that a process outside the container holds open and never
// writes, and prints the guest's clock, in nanoseconds since its start, at
// each of three poll(fd, 1, 300) timeouts: "poll <n> <ret> <ns>".
//
// usage: poll_external_fifo FIFO timeout
//
// The FIFO is opened without blocking, so the outside process must hold it
// open before the guest starts. Once it is open the guest prints "ready".

#include <fcntl.h>
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static long long elapsed_ns(const struct timespec* start) {
  struct timespec now;
  clock_gettime(CLOCK_REALTIME, &now);
  return (now.tv_sec - start->tv_sec) * 1000000000LL +
      (now.tv_nsec - start->tv_nsec);
}

int main(int argc, char** argv) {
  if (argc != 3 || strcmp(argv[2], "timeout") != 0) {
    fprintf(stderr, "usage: %s FIFO timeout\n", argv[0]);
    return 2;
  }
  int fd = open(argv[1], O_RDONLY | O_NONBLOCK);
  if (fd < 0) {
    perror("open");
    return 1;
  }
  printf("ready\n");
  fflush(stdout);
  struct timespec start;
  clock_gettime(CLOCK_REALTIME, &start);
  for (int i = 0; i < 3; i++) {
    struct pollfd p = {fd, POLLIN, 0};
    int ret = poll(&p, 1, 300);
    printf("poll %d %d %lld\n", i, ret, elapsed_ns(&start));
  }
  return 0;
}
