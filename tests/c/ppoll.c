/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Exercises ppoll(2) so that record/replay must capture and reconstruct its
// outputs (the per-fd `revents` and the ready count) instead of consulting live
// fd readiness during replay. Covers: a ready pipe (POLLIN), a not-ready pipe
// with a positive timeout, and the nfds==0 pure-sleep case.

#define _GNU_SOURCE

#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

static void check(int condition, const char* message) {
  if (!condition) {
    perror(message);
    exit(EXIT_FAILURE);
  }
}

int main(void) {
  int fds[2];
  check(pipe(fds) == 0, "pipe");

  // Case 1: data available -> POLLIN is reported and ppoll returns 1.
  check(write(fds[1], "x", 1) == 1, "write");
  struct pollfd ready = {.fd = fds[0], .events = POLLIN, .revents = 0};
  struct timespec timeout = {.tv_sec = 5, .tv_nsec = 0};
  int rc = ppoll(&ready, 1, &timeout, NULL);
  check(rc == 1, "ppoll ready count");
  check((ready.revents & POLLIN) != 0, "ppoll POLLIN revents");

  char byte = 0;
  check(read(fds[0], &byte, 1) == 1, "read");

  // Case 2: nothing available and a short timeout -> ppoll returns 0 with no
  // revents set.
  struct pollfd notready = {.fd = fds[0], .events = POLLIN, .revents = 0};
  struct timespec brief = {.tv_sec = 0, .tv_nsec = 10 * 1000 * 1000};
  rc = ppoll(&notready, 1, &brief, NULL);
  check(rc == 0, "ppoll timeout count");
  check(notready.revents == 0, "ppoll timeout revents");

  // Case 3: nfds == 0 -> ppoll is a pure sleep and returns 0.
  rc = ppoll(NULL, 0, &brief, NULL);
  check(rc == 0, "ppoll nfds=0");

  check(close(fds[0]) == 0, "close read");
  check(close(fds[1]) == 0, "close write");

  puts("ppoll-ok");
  return EXIT_SUCCESS;
}
