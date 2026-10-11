/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A parent forks a child that exits at once, then waits in pause() for its
 * SIGCHLD handler to run, as `while (!flag) pause();` loops do. The child's
 * exit, and so the SIGCHLD the scheduler sends, can come while the parent's
 * pause is filed but not yet parked; Linux returns from pause either way
 * (https://github.com/rrnewton/hermit/issues/4126).
 */
#define _GNU_SOURCE
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile sig_atomic_t got_chld;

static void on_chld(int sig) {
  (void)sig;
  got_chld = 1;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = on_chld;
  sigaction(SIGCHLD, &action, NULL);
  pid_t child = fork();
  if (child == 0) {
    _exit(0);
  }
  while (!got_chld) {
    pause();
  }
  int status = 0;
  waitpid(child, &status, 0);
  printf("SIGCHLD handled: %d, child exit status: %d\n", (int)got_chld,
         WIFEXITED(status) ? WEXITSTATUS(status) : -1);
  return 0;
}
