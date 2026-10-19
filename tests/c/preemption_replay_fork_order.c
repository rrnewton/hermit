/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// The parent prints P and the child prints C, each at once, so which line
// comes first shows which of the two the scheduler ran first: a replayed
// preemption record that gives the child a lower priority than the parent
// from its first instruction prints P first
// (https://github.com/rrnewton/hermit/issues/4497).

#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
  pid_t child = fork();
  if (child < 0) {
    return 2;
  }
  if (child == 0) {
    if (write(1, "C\n", 2) != 2) {
      _exit(3);
    }
    _exit(0);
  }
  if (write(1, "P\n", 2) != 2) {
    return 4;
  }
  int status = 0;
  if (waitpid(child, &status, 0) != child) {
    return 5;
  }
  return !WIFEXITED(status) || WEXITSTATUS(status) != 0;
}
