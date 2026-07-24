/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

static void fail(const char *operation) {
  perror(operation);
  _exit(1);
}

int main(void) {
  static const char success[] = "memory-ok\n";
  long page = sysconf(_SC_PAGESIZE);
  if (page <= 0) {
    fail("sysconf");
  }

  void *initial_break = (void *)syscall(SYS_brk, 0);
  void *grown_break = (void *)((uintptr_t)initial_break + (uintptr_t)page);
  if ((void *)syscall(SYS_brk, grown_break) != grown_break) {
    fail("brk grow");
  }
  if ((void *)syscall(SYS_brk, initial_break) != initial_break) {
    fail("brk restore");
  }

  void *mapping =
      (void *)syscall(SYS_mmap, NULL, (size_t)page * 2, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (mapping == MAP_FAILED) {
    fail("mmap");
  }
  ((volatile unsigned char *)mapping)[0] = 0x5a;
  if (syscall(SYS_mprotect, mapping, (size_t)page * 2, PROT_READ) != 0) {
    fail("mprotect");
  }
  void *remapped = (void *)syscall(SYS_mremap, mapping, (size_t)page * 2,
                                   (size_t)page * 3, MREMAP_MAYMOVE);
  if (remapped == MAP_FAILED) {
    fail("mremap");
  }
  if (((volatile unsigned char *)remapped)[0] != 0x5a) {
    return 2;
  }
  if (syscall(SYS_munmap, remapped, (size_t)page * 3) != 0) {
    fail("munmap");
  }
  if (syscall(SYS_write, STDOUT_FILENO, success, sizeof(success) - 1) !=
      (long)(sizeof(success) - 1)) {
    fail("write");
  }
  return 0;
}
