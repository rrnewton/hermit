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
#include <sys/syscall.h>
#include <unistd.h>

extern char **environ;

int main(void) {
  char *const arguments[] = {"/bin/true", NULL};

  errno = 0;
  if (syscall(SYS_execveat, AT_FDCWD, arguments[0], arguments, environ, 0) !=
      -1)
    return 2;
  if (errno != ENOSYS)
    return 3;
  if (puts("execveat unsupported") == EOF)
    return 4;
  return 0;
}
