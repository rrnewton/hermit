/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static void require(int condition, const char *message) {
  if (!condition) {
    fprintf(stderr, "wait-later-child failure: %s\n", message);
    exit(90);
  }
}

static ssize_t read_retry(int fd, void *buffer, size_t length) {
  ssize_t result;
  do {
    result = read(fd, buffer, length);
  } while (result < 0 && errno == EINTR);
  return result;
}

static int same_mask(const sigset_t *a, const sigset_t *b) {
  int same = 1;
  for (int signal = 1; signal <= 64; ++signal) {
    int left = sigismember(a, signal), right = sigismember(b, signal);
    require(left >= 0 && right >= 0, "query defined Linux mask membership");
    if (left != right)
      same = 0;
  }
  return same;
}

int main(void) {
  struct sigaction requested, original, final;
  sigset_t mask, after;
  memset(&requested, 0, sizeof(requested));
  requested.sa_handler = SIG_DFL;
  require(sigemptyset(&requested.sa_mask) == 0 &&
              sigaction(SIGCHLD, &requested, NULL) == 0,
          "default non-auto-reap SIGCHLD");
  memset(&original, 0, sizeof(original));
  require(sigaction(SIGCHLD, NULL, &original) == 0 &&
              original.sa_handler == SIG_DFL &&
              (original.sa_flags & SA_NOCLDWAIT) == 0,
          "read exact default action");
  require(sigemptyset(&mask) == 0 && sigaddset(&mask, SIGUSR1) == 0 &&
              sigaddset(&mask, SIGUSR2) == 0 &&
              sigprocmask(SIG_SETMASK, &mask, NULL) == 0,
          "unblock SIGCHLD while retaining unrelated mask members");

  int lifetime[2];
  require(pipe(lifetime) == 0, "child lifetime pipe");
  pid_t first = fork();
  require(first >= 0, "first fork");
  if (first == 0) {
    require(close(lifetime[0]) == 0, "first closes read endpoint");
    char release = 0;
    require(read_retry(STDIN_FILENO, &release, 1) == 1 && release == 'A',
            "host releases first child after actual parent WaitChild park");
    /* This process alone retains the writer. B requires its real exit-side
     * descriptor close, not parent guest progress or a synthetic signal. */
    _exit(31);
  }
  pid_t second = fork();
  require(second >= 0, "second fork");
  if (second == 0) {
    require(close(lifetime[1]) == 0, "second closes writer");
    char byte = 0;
    require(read_retry(lifetime[0], &byte, 1) == 0, "first child's real exit closes writer");
    require(close(lifetime[0]) == 0, "second closes read endpoint");
    _exit(47);
  }
  require(close(lifetime[0]) == 0 && close(lifetime[1]) == 0,
          "parent retains neither child pipe endpoint");
  printf("wait-later-child ready parent=%ld first=%ld second=%ld\n",
         (long)getpid(), (long)first, (long)second);
  require(fflush(stdout) == 0, "publish fixture logical identities");
  int status = 0;
  require(waitpid(second, &status, 0) == second && WIFEXITED(status) &&
              WEXITSTATUS(status) == 47,
          "parent waiting on later child progresses across first child's exit");
  require(waitpid(first, &status, 0) == first && WIFEXITED(status) &&
              WEXITSTATUS(status) == 31,
          "first child remains exactly reapable");
  memset(&after, 0, sizeof(after));
  memset(&final, 0, sizeof(final));
  require(sigprocmask(SIG_SETMASK, NULL, &after) == 0 && same_mask(&mask, &after),
          "complete guest mask survives real Detcore signal yields");
  require(sigaction(SIGCHLD, NULL, &final) == 0 &&
              final.sa_handler == original.sa_handler &&
              final.sa_flags == original.sa_flags &&
              final.sa_restorer == original.sa_restorer &&
              same_mask(&final.sa_mask, &original.sa_mask),
          "complete defined signal action survives publication controls");
  puts("wait-later-child completed second=47 first=31");
  return 0;
}
