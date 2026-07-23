/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <stdio.h>
#include <unistd.h>

int main(void) {
  int first[2];
  int second[2];

  if (pipe(first) != 0) {
    perror("first pipe");
    return 1;
  }

  const int expected_reuse = first[1];
  if (close(expected_reuse) != 0) {
    perror("close pipe write end");
    return 1;
  }

  if (pipe(second) != 0) {
    perror("second pipe");
    return 1;
  }

  if (second[0] != expected_reuse) {
    fprintf(stderr, "expected fd %d to be reused, got %d\n", expected_reuse,
            second[0]);
    return 1;
  }

  return close(first[0]) != 0 || close(second[0]) != 0 ||
         close(second[1]) != 0;
}
