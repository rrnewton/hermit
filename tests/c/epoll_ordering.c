/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Multi-fd epoll_wait ordering guest.
 *
 * Registers several pipe read-ends with a single epoll instance and makes them
 * all readable *before* calling epoll_wait, so the kernel reports every fd in
 * one shot. The fds are registered (and made ready) in a deliberately scrambled
 * order, and each is tagged with an `epoll_event.data.u64` equal to its logical
 * id. epoll_wait(2) promises nothing about the order of the returned events, so
 * the raw kernel order is host-timing dependent; Hermit determinizes it by
 * sorting on the caller-supplied `data`. This guest prints the tags in exactly
 * the order epoll_wait returned them, letting the harness assert that Hermit
 * yields the canonical (ascending) order on every run.
 */

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <unistd.h>

#define NFDS 8
#define ARRAY_SIZE(values) (sizeof(values) / sizeof((values)[0]))

static void fail_errno(const char* operation) {
  fprintf(stderr, "%s failed: %s\n", operation, strerror(errno));
  exit(1);
}

int main(void) {
  const int epoll_fd = epoll_create1(EPOLL_CLOEXEC);
  if (epoll_fd < 0) {
    fail_errno("epoll_create1");
  }

  int read_fd[NFDS];
  int write_fd[NFDS];
  for (int i = 0; i < NFDS; ++i) {
    int pipefd[2];
    if (pipe2(pipefd, O_CLOEXEC | O_NONBLOCK) != 0) {
      fail_errno("pipe2");
    }
    read_fd[i] = pipefd[0];
    write_fd[i] = pipefd[1];
  }

  /*
   * Register and arm the fds in a scrambled order that matches neither the
   * ascending tag order nor the fd order, so a correct result cannot be an
   * accident of registration order.
   */
  static const int scrambled[NFDS] = {5, 2, 7, 0, 4, 1, 6, 3};
  for (size_t k = 0; k < ARRAY_SIZE(scrambled); ++k) {
    const int id = scrambled[k];
    struct epoll_event event = {
        .events = EPOLLIN,
        .data.u64 = (uint64_t)id,
    };
    if (epoll_ctl(epoll_fd, EPOLL_CTL_ADD, read_fd[id], &event) != 0) {
      fail_errno("epoll_ctl");
    }
    if (write(write_fd[id], "x", 1) != 1) {
      fail_errno("write");
    }
  }

  struct epoll_event events[NFDS + 4];
  const int ready = epoll_wait(epoll_fd, events, ARRAY_SIZE(events), 1000);
  if (ready < 0) {
    fail_errno("epoll_wait");
  }
  if (ready != NFDS) {
    fprintf(stderr, "epoll_wait returned %d events, expected %d\n", ready, NFDS);
    exit(1);
  }

  for (int i = 0; i < ready; ++i) {
    printf("%s%llu", i == 0 ? "" : " ", (unsigned long long)events[i].data.u64);
  }
  printf("\n");
  printf("epoll_ordering success\n");
  return 0;
}
