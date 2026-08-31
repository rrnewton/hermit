/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <sys/eventfd.h>
#include <sys/socket.h>
#include <unistd.h>

#include <stdio.h>

static int print_fdinfo(const char *name, int fd) {
  char path[64];
  if (snprintf(path, sizeof(path), "/proc/self/fdinfo/%d", fd) < 0) {
    return 1;
  }
  FILE *file = fopen(path, "r");
  if (file == NULL) {
    return 1;
  }
  printf("[%s]\n", name);
  for (int byte; (byte = fgetc(file)) != EOF;) {
    putchar(byte);
  }
  return fclose(file) != 0;
}

int main(void) {
  int pipe_fds[2];
  int socket_fds[2];
  int event_fd = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
  if (pipe(pipe_fds) != 0 ||
      socketpair(AF_UNIX, SOCK_STREAM, 0, socket_fds) != 0 || event_fd < 0) {
    return 1;
  }

  return print_fdinfo("pipe", pipe_fds[0]) ||
         print_fdinfo("socket", socket_fds[0]) ||
         print_fdinfo("eventfd", event_fd);
}
