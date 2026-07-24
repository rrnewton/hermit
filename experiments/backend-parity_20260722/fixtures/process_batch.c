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
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

static void fail(const char *operation) {
  perror(operation);
  exit(1);
}

static void require_exit(pid_t child, int expected) {
  int status = 0;
  if (syscall(SYS_wait4, child, &status, 0, NULL) != child) {
    fail("wait4");
  }
  if (!WIFEXITED(status) || WEXITSTATUS(status) != expected) {
    exit(2);
  }
}

int main(void) {
  static const char success[] = "process-ok\n";

  pid_t fork_child = (pid_t)syscall(SYS_fork);
  if (fork_child < 0) {
    fail("fork");
  }
  if (fork_child == 0) {
    syscall(SYS_exit, 7);
    __builtin_unreachable();
  }
  require_exit(fork_child, 7);

  pid_t vfork_child = vfork();
  if (vfork_child < 0) {
    fail("vfork");
  }
  if (vfork_child == 0) {
    _exit(8);
  }
  siginfo_t info = {0};
  if (syscall(SYS_waitid, P_PID, vfork_child, &info, WEXITED, NULL) != 0) {
    fail("waitid");
  }
  if (info.si_code != CLD_EXITED || info.si_status != 8) {
    return 3;
  }

  pid_t clone_child = (pid_t)syscall(SYS_clone, SIGCHLD, NULL, NULL, NULL, 0);
  if (clone_child < 0) {
    fail("clone");
  }
  if (clone_child == 0) {
    syscall(SYS_exit, 9);
    __builtin_unreachable();
  }
  require_exit(clone_child, 9);

  pid_t exec_child = (pid_t)syscall(SYS_fork);
  if (exec_child < 0) {
    fail("fork for execve");
  }
  if (exec_child == 0) {
    char *const arguments[] = {"/bin/true", NULL};
    syscall(SYS_execve, arguments[0], arguments, environ);
    syscall(SYS_exit_group, 127);
    __builtin_unreachable();
  }
  require_exit(exec_child, 0);

  if (syscall(SYS_write, STDOUT_FILENO, success, sizeof(success) - 1) !=
      (long)(sizeof(success) - 1)) {
    fail("write");
  }
  return 0;
}
