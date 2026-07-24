/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <unistd.h>

static volatile sig_atomic_t deliveries;

static void handle_signal(int signal_number) {
  (void)signal_number;
  ++deliveries;
}

static void fail(const char* operation) {
  perror(operation);
  exit(1);
}

int main(void) {
  static const char success[] = "signal-ok\n";
  struct sigaction action = {0};
  sigset_t blocked;

  action.sa_handler = handle_signal;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGUSR1, &action, NULL) != 0) {
    fail("sigaction");
  }
  sigemptyset(&blocked);
  sigaddset(&blocked, SIGUSR1);
  if (sigprocmask(SIG_BLOCK, &blocked, NULL) != 0) {
    fail("sigprocmask block");
  }
  if (syscall(SYS_tgkill, getpid(), syscall(SYS_gettid), SIGUSR1) != 0) {
    fail("tgkill");
  }
  if (deliveries != 0) {
    return 2;
  }
  if (sigprocmask(SIG_UNBLOCK, &blocked, NULL) != 0) {
    fail("sigprocmask unblock");
  }
  if (deliveries != 1) {
    return 3;
  }
  if (kill(getpid(), SIGUSR1) != 0) {
    fail("kill");
  }
  if (deliveries != 2) {
    return 4;
  }
  if (syscall(SYS_write, STDOUT_FILENO, success, sizeof(success) - 1) !=
      (long)(sizeof(success) - 1)) {
    fail("write");
  }
  return 0;
}
