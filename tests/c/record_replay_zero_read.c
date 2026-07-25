/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>

int main(void) {
  const int fd = open("/etc/hosts", O_RDONLY);
  if (fd < 0) {
    perror("open");
    return 1;
  }

  if (read(fd, NULL, 0) != 0) {
    perror("zero-length read");
    return 2;
  }
  if (pread(fd, NULL, 0, 0) != 0) {
    perror("zero-length pread");
    return 3;
  }

  puts("zero-length reads preserved");
  return close(fd) != 0;
}
