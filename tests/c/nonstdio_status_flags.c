/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static void require(int condition, const char *operation) {
  if (!condition) {
    fprintf(stderr, "%s: %s\n", operation, strerror(errno));
    exit(2);
  }
}

static void check_nonblocking(int fd, int expected, const char *operation) {
  int flags = fcntl(fd, F_GETFL);
  require(flags >= 0, "F_GETFL");
  if (!!(flags & O_NONBLOCK) != expected) {
    fprintf(stderr, "%s: fd=%d flags=%#x expected_nonblock=%d\n",
            operation, fd, flags, expected);
    exit(3);
  }
}

int main(void) {
  int pipefd[2];
  require(pipe2(pipefd, 0) == 0, "pipe2");
  require(pipefd[0] > STDERR_FILENO && pipefd[1] > STDERR_FILENO,
          "nonstdio descriptors");
  check_nonblocking(pipefd[0], 0, "initial reader");
  check_nonblocking(pipefd[1], 0, "initial writer");
  int alias = dup(pipefd[0]);
  require(alias >= 0, "dup");
  check_nonblocking(alias, 0, "initial alias");
  require(fcntl(alias, F_SETFL, O_NONBLOCK) == 0, "set nonblocking");
  check_nonblocking(alias, 1, "nonblocking alias");
  check_nonblocking(pipefd[0], 1, "nonblocking reader");
  check_nonblocking(pipefd[1], 0, "independent writer");
  char byte;
  errno = 0;
  require(read(alias, &byte, 1) == -1 && errno == EAGAIN,
          "empty nonblocking read");
  require(fcntl(pipefd[0], F_SETFL, 0) == 0, "restore blocking");
  check_nonblocking(pipefd[0], 0, "restored reader");
  check_nonblocking(alias, 0, "restored alias");
  require(write(pipefd[1], "A", 1) == 1, "write");
  require(read(alias, &byte, 1) == 1 && byte == 'A', "read");
  require(close(alias) == 0 && close(pipefd[0]) == 0 && close(pipefd[1]) == 0,
          "close");
  puts("nonstdio-status-flags-ok");
  return 0;
}
