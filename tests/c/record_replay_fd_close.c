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
#include <string.h>
#include <sys/epoll.h>
#include <sys/mman.h>
#include <unistd.h>

static int fail(const char* operation) {
  fprintf(stderr, "%s failed: %s\n", operation, strerror(errno));
  return 1;
}

int main(void) {
  const int memfd = memfd_create("record-replay-fd-close", MFD_CLOEXEC);
  if (memfd < 0) {
    return fail("memfd_create");
  }
  if (close(memfd) != 0) {
    return fail("close(memfd)");
  }

  const int epoll_fd = epoll_create1(EPOLL_CLOEXEC);
  if (epoll_fd < 0) {
    return fail("epoll_create1");
  }
  if (epoll_fd != memfd) {
    fprintf(
        stderr,
        "closed descriptor was not reused: memfd=%d epoll=%d\n",
        memfd,
        epoll_fd);
    return 1;
  }
  if (fcntl(epoll_fd, F_GETFD) < 0) {
    return fail("fcntl(epoll_fd, F_GETFD)");
  }
  if (close(epoll_fd) != 0) {
    return fail("close(epoll_fd)");
  }

  printf("descriptor %d reused after close\n", epoll_fd);
  return 0;
}
