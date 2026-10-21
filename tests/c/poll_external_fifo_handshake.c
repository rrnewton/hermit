/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Waits for N lines from a FIFO that a process outside the container writes,
// asking for each line first, and prints the guest's clock, in nanoseconds
// since its start, when each line is complete.
//
// usage: poll_external_fifo_handshake FIFO DIR N
//
// Before line i the guest creates DIR/ready-<i>; the outside writer waits for
// that file, removes it, and only then writes the line. So in every run the
// guest is already waiting when its line arrives, however long the writer
// takes, and the bytes each poll and read see are the same. It reads one byte
// at a time, polling before each read, so its sequence of system calls does
// not depend on how much of a line had arrived. Only the time the outside
// writer took differs between runs, and that must not reach the clock.
//
// The FIFO is opened without blocking, so the writer must hold it open before
// the guest starts.

#include <fcntl.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

static long long elapsed_ns(const struct timespec* start) {
  struct timespec now;
  clock_gettime(CLOCK_REALTIME, &now);
  return (now.tv_sec - start->tv_sec) * 1000000000LL +
      (now.tv_nsec - start->tv_nsec);
}

int main(int argc, char** argv) {
  if (argc != 4) {
    fprintf(stderr, "usage: %s FIFO DIR N\n", argv[0]);
    return 2;
  }
  int fd = open(argv[1], O_RDONLY | O_NONBLOCK);
  if (fd < 0) {
    perror("open");
    return 1;
  }
  int lines = atoi(argv[3]);
  struct timespec start;
  clock_gettime(CLOCK_REALTIME, &start);
  for (int i = 0; i < lines; i++) {
    char ready[4096];
    snprintf(ready, sizeof ready, "%s/ready-%d", argv[2], i);
    int marker = open(ready, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (marker < 0) {
      perror("open marker");
      return 1;
    }
    close(marker);
    for (;;) {
      struct pollfd p = {fd, POLLIN, 0};
      if (poll(&p, 1, -1) < 0) {
        perror("poll");
        return 1;
      }
      char c;
      ssize_t n = read(fd, &c, 1);
      if (n == 1 && c == '\n') {
        break;
      }
      if (n == 0) {
        fprintf(stderr, "end of file before line %d\n", i);
        return 1;
      }
    }
    printf("line %d %lld\n", i, elapsed_ns(&start));
  }
  return 0;
}
