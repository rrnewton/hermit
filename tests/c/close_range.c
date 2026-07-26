/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Regression guest for the close_range(2) handler. Detcore determinizes
 * close_range by injecting the real syscall and pruning its own descriptor
 * table for the closed range, so this exercises both that the descriptors are
 * actually closed and that Detcore's bookkeeping stays consistent (a freshly
 * opened descriptor reuses the lowest freed number). Output is fixed, so it is
 * bitwise-identical under `hermit run --strict --verify`.
 */

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <unistd.h>

static int open_devnull(void) {
  int fd = open("/dev/null", O_RDONLY);
  if (fd < 0) {
    perror("open /dev/null");
    exit(1);
  }
  return fd;
}

int main(void) {
  /* Open three descriptors; they land at the lowest free numbers. */
  int a = open_devnull();
  int b = open_devnull();
  int c = open_devnull();

  /* Close the entire range at and above the first descriptor. */
  errno = 0;
  long r = syscall(SYS_close_range, (unsigned)a, ~0u, 0u);
  if (r != 0) {
    fprintf(stderr, "close_range returned %ld, errno %d\n", r, errno);
    return 1;
  }

  /* Every closed descriptor must now be invalid. */
  char buf[1];
  for (int fd = a; fd <= c; fd++) {
    errno = 0;
    if (read(fd, buf, sizeof(buf)) != -1 || errno != EBADF) {
      fprintf(stderr, "descriptor %d still valid after close_range\n", fd);
      return 1;
    }
  }

  /* Detcore's table was pruned, so a fresh open reuses the lowest freed fd. */
  int d = open_devnull();
  if (d != a) {
    fprintf(stderr, "reopened fd %d, expected %d\n", d, a);
    return 1;
  }
  close(d);

  puts("close_range closed the descriptor range");
  return 0;
}
