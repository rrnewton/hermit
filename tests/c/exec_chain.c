/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

#ifndef CHAIN_STAGE
#error "CHAIN_STAGE must select exec-chain stage 1, 2, or 3"
#endif

static int write_stage(const char* message, size_t length) {
  return write(STDOUT_FILENO, message, length) == (ssize_t)length ? 0 : 1;
}

int main(int argc, char** argv) {
  (void)argv;
#if CHAIN_STAGE == 1
  if (argc != 3 || write_stage("chain a\n", 8) != 0) {
    return 1;
  }
  char* const arguments[] = {
      argv[1],
      argv[2],
      NULL,
  };
  execv(argv[1], arguments);
  perror("exec chain b");
  return 1;
#elif CHAIN_STAGE == 2
  if (argc != 2 || write_stage("chain b\n", 8) != 0) {
    return 1;
  }
  char* const arguments[] = {
      argv[1],
      NULL,
  };
  execv(argv[1], arguments);
  perror("exec chain c");
  return 1;
#elif CHAIN_STAGE == 3
  if (argc != 1 || write_stage("chain c\n", 8) != 0) {
    return 1;
  }
  return 0;
#else
#error "CHAIN_STAGE must select exec-chain stage 1, 2, or 3"
#endif
}
