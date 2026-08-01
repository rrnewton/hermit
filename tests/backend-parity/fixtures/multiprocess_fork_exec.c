/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Multi-process fork+exec parity probe.
 *
 * The parent forks a fixed number of children; each child immediately
 * re-executes this same binary (via /proc/self/exe) in a worker role that
 * exits with a fixed status derived from its index. The parent then reaps
 * every child by its specific pid and sums the exit statuses.
 *
 * This exercises the full fork -> execve -> exit -> reap cycle across a
 * process tree, which is the multi-process backend contract we want held at
 * parity across ptrace/dbi/kvm. It is deliberately free of gated concerns:
 *
 *   - It never prints or compares pids (child pid *numbering* legitimately
 *     differs across backends and is scheduler-adjacent).
 *   - It reaps each child by its own known pid, so the result is independent
 *     of the order in which children are scheduled or exit.
 *   - It reports no timing, cpu, or wall-clock quantity.
 *
 * The only observable is an aggregate: fork count, that every exec succeeded,
 * the reap count, and the sum of the children's exit statuses. For WORKERS
 * children numbered 1..WORKERS the status sum is fixed and backend-independent.
 */

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#define WORKERS 4
#define WORKER_PREFIX "--worker="

static void fail(const char *message) {
  fprintf(stderr, "%s: %s\n", message, strerror(errno));
  exit(1);
}

/* Worker role: exit with the status encoded in argv[1]. */
static int run_worker(const char *arg) {
  long status = strtol(arg + strlen(WORKER_PREFIX), NULL, 10);
  _exit((int)status);
}

int main(int argc, char **argv) {
  if (argc == 2 && strncmp(argv[1], WORKER_PREFIX, strlen(WORKER_PREFIX)) == 0)
    return run_worker(argv[1]);
  if (argc != 1) {
    fprintf(stderr, "usage: %s [%sN]\n", argv[0], WORKER_PREFIX);
    return 64;
  }

  pid_t children[WORKERS];
  for (int i = 0; i < WORKERS; ++i) {
    pid_t pid = fork();
    if (pid < 0)
      fail("fork");
    if (pid == 0) {
      char worker_arg[32];
      snprintf(worker_arg, sizeof(worker_arg), WORKER_PREFIX "%d", i + 1);
      char *worker_argv[] = {argv[0], worker_arg, NULL};
      execv("/proc/self/exe", worker_argv);
      /* Only reached if execv failed. */
      _exit(127);
    }
    children[i] = pid;
  }

  int exec_ok = 1;
  int reaped = 0;
  int status_sum = 0;
  for (int i = 0; i < WORKERS; ++i) {
    int status = 0;
    if (waitpid(children[i], &status, 0) != children[i])
      fail("waitpid");
    ++reaped;
    if (!WIFEXITED(status)) {
      exec_ok = 0;
      continue;
    }
    int code = WEXITSTATUS(status);
    if (code == 127)
      exec_ok = 0;
    status_sum += code;
  }

  /* status_sum for workers 1..WORKERS is WORKERS*(WORKERS+1)/2 = 10. */
  printf("forked=%d exec=%s reaped=%d status_sum=%d\n", WORKERS,
         exec_ok ? "ok" : "fail", reaped, status_sum);
  return 0;
}
