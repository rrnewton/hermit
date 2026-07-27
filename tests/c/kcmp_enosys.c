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
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_kcmp
#define SYS_kcmp 312
#endif

#ifndef KCMP_FILES
#define KCMP_FILES 2
#endif

int main(void) {
  pid_t self = getpid();
  errno = 0;
  long result = syscall(SYS_kcmp, self, self, KCMP_FILES, 0UL, 0UL);
  if (result != -1 || errno != ENOSYS) {
    fprintf(stderr, "kcmp returned %ld with errno %d (%s), expected ENOSYS\n",
            result, errno, strerror(errno));
    return 1;
  }
  puts("kcmp deterministically unavailable");
  return 0;
}
