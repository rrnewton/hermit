/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Prints the signals the process started with ignored, as rt_sigaction
 * reports them, one line: "ignored:" and each such signal's number.
 *
 * It asks the kernel with the raw system call (glibc's sigaction refuses
 * signals 32 and 33), and for each ignored signal it also makes one getpid
 * call, so that its system calls, not only its output, depend on the set. A
 * replay that started the program with another set would make other calls
 * than its recording.
 */

#define _GNU_SOURCE
#include <signal.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

/* The kernel's struct sigaction on x86_64. */
struct kernel_sigaction {
  void (*handler)(int);
  unsigned long flags;
  void (*restorer)(void);
  unsigned long mask;
};

int main(void) {
  char line[512] = "ignored:";
  int used = 8;
  for (int signal = 1; signal <= 64; signal++) {
    struct kernel_sigaction old;
    if (syscall(SYS_rt_sigaction, signal, NULL, &old, sizeof(old.mask)) != 0) {
      continue;
    }
    if (old.handler == SIG_IGN) {
      syscall(SYS_getpid);
      used += snprintf(line + used, sizeof(line) - used, " %d", signal);
    }
  }
  puts(line);
  return 0;
}
