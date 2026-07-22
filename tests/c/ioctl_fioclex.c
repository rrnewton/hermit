/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/ioctl.h>
#include <unistd.h>

static void check(int condition, const char *message) {
  if (!condition) {
    perror(message);
    exit(EXIT_FAILURE);
  }
}

int main(void) {
  int fd = open("/dev/null", O_RDONLY);
  check(fd >= 0, "open");

  check(ioctl(fd, FIOCLEX) == 0, "ioctl(FIOCLEX)");
  int flags = fcntl(fd, F_GETFD);
  check(flags >= 0, "fcntl(F_GETFD)");
  check((flags & FD_CLOEXEC) != 0, "FIOCLEX did not set FD_CLOEXEC");

  check(ioctl(fd, FIONCLEX) == 0, "ioctl(FIONCLEX)");
  flags = fcntl(fd, F_GETFD);
  check(flags >= 0, "fcntl(F_GETFD)");
  check((flags & FD_CLOEXEC) == 0, "FIONCLEX did not clear FD_CLOEXEC");

  check(close(fd) == 0, "close");
  puts("fioclex-ok");
  return EXIT_SUCCESS;
}
