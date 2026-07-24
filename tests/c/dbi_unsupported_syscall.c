/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review the raw unsupported-syscall DBI fixture.

#define _GNU_SOURCE
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
  if (syscall(SYS_getppid) < 0) {
    perror("getppid");
    return 1;
  }
  puts("dbi-unsupported-ok");
  return 0;
}
