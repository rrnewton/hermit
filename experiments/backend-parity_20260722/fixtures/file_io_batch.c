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
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

static void fail(const char* operation) {
  perror(operation);
  exit(1);
}

int main(int argc, char** argv) {
  static const char expected[] = "backend parity fixture\n";
  static const char success[] = "file-io-ok\n";
  struct stat fd_stat;
  struct stat path_stat;
  char buffer[sizeof(expected)] = {0};

  if (argc != 2) {
    return 2;
  }

  long fd = syscall(SYS_openat, AT_FDCWD, argv[1], O_RDONLY | O_CLOEXEC, 0);
  if (fd < 0) {
    fail("openat");
  }
  if (syscall(SYS_fstat, fd, &fd_stat) != 0) {
    fail("fstat");
  }
  if (syscall(SYS_newfstatat, AT_FDCWD, argv[1], &path_stat, 0) != 0) {
    fail("newfstatat");
  }
  if (fd_stat.st_size != path_stat.st_size) {
    return 3;
  }
  if (syscall(SYS_access, argv[1], R_OK) != 0) {
    fail("access");
  }
  if (syscall(SYS_faccessat, AT_FDCWD, argv[1], R_OK) != 0) {
    fail("faccessat");
  }
  if (syscall(SYS_lseek, fd, 0, SEEK_SET) != 0) {
    fail("lseek");
  }
  if (syscall(SYS_read, fd, buffer, sizeof(expected) - 1) !=
      (long)(sizeof(expected) - 1)) {
    fail("read");
  }
  if (memcmp(buffer, expected, sizeof(expected)) != 0) {
    return 4;
  }
  if (syscall(SYS_close, fd) != 0) {
    fail("close");
  }
  if (syscall(SYS_write, STDOUT_FILENO, success, sizeof(success) - 1) !=
      (long)(sizeof(success) - 1)) {
    fail("write");
  }
  return 0;
}
