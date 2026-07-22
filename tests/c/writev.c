/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Exercises vectored writes (writev) for record/replay coverage. The three
 * segments are gathered into a single write to stdout, so replay must reproduce
 * the exact same bytes for `record --verify` to succeed. */

#include <stdio.h>
#include <string.h>
#include <sys/uio.h>
#include <unistd.h>

int main(void) {
  char part0[] = "Hello, ";
  char part1[] = "vectored ";
  char part2[] = "world!\n";

  struct iovec iov[3];
  iov[0].iov_base = part0;
  iov[0].iov_len = strlen(part0);
  iov[1].iov_base = part1;
  iov[1].iov_len = strlen(part1);
  iov[2].iov_base = part2;
  iov[2].iov_len = strlen(part2);

  size_t total = iov[0].iov_len + iov[1].iov_len + iov[2].iov_len;

  ssize_t written = writev(STDOUT_FILENO, iov, 3);
  if (written != (ssize_t)total) {
    fprintf(stderr, "writev returned %zd, expected %zu\n", written, total);
    return 1;
  }
  return 0;
}
