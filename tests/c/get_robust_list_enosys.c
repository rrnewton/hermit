/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <stddef.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_get_robust_list
#define SYS_get_robust_list 274
#endif

int main(void) {
  void *head = NULL;
  size_t len = 0;

  errno = 0;
  long result = syscall(SYS_get_robust_list, 1, &head, &len);
  if (result == -1 && errno == ENOSYS) {
    puts("get_robust_list deterministically unavailable");
    return 0;
  }

  fprintf(stderr,
          "get_robust_list: expected ENOSYS, got result=%ld errno=%d\n",
          result, errno);
  return 1;
}
