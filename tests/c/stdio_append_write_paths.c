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
#include <sys/sendfile.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <unistd.h>

#ifndef RWF_NOAPPEND
#define RWF_NOAPPEND 0x00000020
#endif

static void fail(const char *what) {
  fprintf(stderr, "%s: %s\n", what, strerror(errno));
  exit(2);
}

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr,
            "usage: %s sendfile|sendfile-pipe|pwrite|pwritev|pwritev2|pwritev2-noappend\n",
            argv[0]);
    return 2;
  }

  char input_name[] = "/tmp/stdio-append-write-paths-XXXXXX";
  int input = mkstemp(input_name);
  if (input < 0)
    fail("mkstemp");
  if (unlink(input_name) != 0)
    fail("unlink");
  if (write(input, "guest\n", 6) != 6 || lseek(input, 0, SEEK_SET) != 0)
    fail("prepare input");

  int before = fcntl(STDOUT_FILENO, F_GETFL);
  if (before < 0)
    fail("fcntl(F_GETFL)");
  if (fcntl(STDOUT_FILENO, F_SETFL, before | O_APPEND) != 0)
    fail("fcntl(F_SETFL, O_APPEND)");
  int after = fcntl(STDOUT_FILENO, F_GETFL);
  if (after < 0)
    fail("fcntl(F_GETFL after set)");

  errno = 0;
  ssize_t result;
  off_t input_offset = 0;
  struct iovec iov = {.iov_base = "guest\n", .iov_len = 6};
  if (strcmp(argv[1], "sendfile") == 0 ||
      strcmp(argv[1], "sendfile-pipe") == 0) {
    result = sendfile(STDOUT_FILENO, input, &input_offset, 6);
  } else if (strcmp(argv[1], "pwrite") == 0) {
    result = pwrite(STDOUT_FILENO, "guest\n", 6, 0);
  } else if (strcmp(argv[1], "pwritev") == 0) {
    result = pwritev(STDOUT_FILENO, &iov, 1, 0);
  } else if (strcmp(argv[1], "pwritev2") == 0) {
    result = syscall(SYS_pwritev2, STDOUT_FILENO, &iov, 1, 0, 0, 0);
  } else if (strcmp(argv[1], "pwritev2-noappend") == 0) {
    result = syscall(SYS_pwritev2, STDOUT_FILENO, &iov, 1, 0, 0,
                     RWF_NOAPPEND);
  } else {
    fprintf(stderr, "unknown operation: %s\n", argv[1]);
    return 2;
  }
  int operation_errno = errno;

  fprintf(stderr, "op=%s append=%d result=%zd errno=%d input_offset=%ld\n",
          argv[1], (after & O_APPEND) != 0, result, operation_errno,
          (long)input_offset);
  if (close(input) != 0)
    fail("close");

  if ((after & O_APPEND) == 0)
    return 1;
  if (strcmp(argv[1], "sendfile") == 0)
    return !(result == -1 && operation_errno == EINVAL && input_offset == 0);
  if (strcmp(argv[1], "sendfile-pipe") == 0)
    return !(result == 6 && operation_errno == 0 && input_offset == 6);
  return !(result == 6 && operation_errno == 0);
}
