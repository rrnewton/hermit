/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review unsupported policy across root, fork, and exec.

#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static int call_unsupported(void) {
  if (syscall(SYS_getppid) < 0) {
    perror("getppid");
    return 1;
  }
  if (syscall(SYS_getpgrp) < 0) {
    perror("getpgrp");
    return 1;
  }
  return 0;
}

int main(int argc, char **argv) {
  if (argc == 2 && strcmp(argv[1], "after-exec") == 0) {
    if (call_unsupported() != 0) {
      return 1;
    }
    puts("dbi-unsupported-exec-ok");
    return 0;
  }

  if (argc == 2 && strcmp(argv[1], "exec-empty") == 0) {
    char *next_argv[] = {argv[0], "after-exec", NULL};
    char *next_env[] = {NULL};
    execve(argv[0], next_argv, next_env);
    perror("execve");
    return 1;
  }

  if (argc == 2 && strcmp(argv[1], "report-tamper") == 0) {
    if (call_unsupported() != 0) {
      return 1;
    }
    (void)ftruncate(199, 0);
    puts("dbi-unsupported-report-tamper-ok");
    return 0;
  }

  if (argc == 2 && (strcmp(argv[1], "fork") == 0 ||
                    strcmp(argv[1], "fork-report-tamper") == 0)) {
    pid_t child = fork();
    if (child < 0) {
      perror("fork");
      return 1;
    }
    if (child == 0) {
      if (strcmp(argv[1], "fork-report-tamper") == 0) {
        close(199);
      }
      _exit(call_unsupported());
    }
    int status = 0;
    if (waitpid(child, &status, 0) != child) {
      perror("waitpid");
      return 1;
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
      return 1;
    }
    if (strcmp(argv[1], "fork-report-tamper") == 0) {
      puts("dbi-unsupported-fork-report-tamper-ok");
    } else {
      puts("dbi-unsupported-fork-ok");
    }
    return 0;
  }

  if (argc == 2 && (strcmp(argv[1], "fork-exec") == 0 ||
                    strcmp(argv[1], "fork-setsid-exec") == 0)) {
    pid_t child = fork();
    if (child < 0) {
      perror("fork");
      return 1;
    }
    if (child == 0) {
      if (strcmp(argv[1], "fork-setsid-exec") == 0 && setsid() < 0) {
        perror("setsid");
        _exit(126);
      }

      char *next_argv[] = {argv[0], "after-exec", NULL};
      char *next_env[] = {NULL};
      execve(argv[0], next_argv, next_env);
      perror("execve");
      _exit(127);
    }
    int status = 0;
    if (waitpid(child, &status, 0) != child) {
      perror("waitpid");
      return 1;
    }
    if (strcmp(argv[1], "fork-setsid-exec") == 0) {
      puts("dbi-unsupported-fork-setsid-exec-parent-ok");
    } else {
      puts("dbi-unsupported-fork-exec-parent-ok");
    }
    return 0;
  }

  if (call_unsupported() != 0) {
    return 1;
  }
  puts("dbi-unsupported-ok");
  return 0;
}
