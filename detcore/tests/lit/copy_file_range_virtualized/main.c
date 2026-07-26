/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-787)

// Regression for PR-787. copy_file_range(2) copies bytes between two regular
// files; under Hermit's stable-filesystem model the copy is deterministic, so
// Detcore forwards it (PassThrough) instead of fail-closing under --strict (see
// detcore/src/syscall_classification.rs). The guest creates its own source and
// destination, copies a fixed 16-byte slice, and unlinks both so a --verify
// second run starts from identical filesystem state (see the sibling .lit
// files). Left Unsupported, copy_file_range aborts modern coreutils cp/mv and
// language-runtime io.Copy fast paths under --strict.
// RUN: %me | FileCheck %s

#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
  const char *src = "cfr_src.txt";
  const char *dst = "cfr_dst.txt";
  int in = open(src, O_CREAT | O_TRUNC | O_RDWR, 0644);
  if (in < 0) {
    perror("open src");
    return 2;
  }
  if (write(in, "0123456789ABCDEFghij", 20) != 20) {
    perror("write src");
    return 2;
  }
  int out = open(dst, O_CREAT | O_TRUNC | O_RDWR, 0644);
  if (out < 0) {
    perror("open dst");
    return 2;
  }
  off_t off_in = 0, off_out = 0;
  ssize_t n = syscall(SYS_copy_file_range, in, &off_in, out, &off_out, 16, 0);
  printf("copy_file_range n=%zd off_in=%lld\n", n, (long long)off_in);
  close(in);
  close(out);
  unlink(src);
  unlink(dst);
  return 0;
}

// CHECK: copy_file_range n=16 off_in=16
