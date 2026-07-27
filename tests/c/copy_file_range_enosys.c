/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_copy_file_range
#define SYS_copy_file_range 326
#endif

int main(void) {
  errno = 0;
  long result = syscall(SYS_copy_file_range, -1, NULL, -1, NULL, 1, 0);
  if (result == -1 && errno == ENOSYS) {
    puts("copy_file_range deterministically unavailable");
    return 0;
  }

  fprintf(stderr,
          "copy_file_range: expected ENOSYS, got result=%ld errno=%d\n",
          result, errno);
  return 1;
}
