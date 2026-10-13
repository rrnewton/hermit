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

static volatile sig_atomic_t handlers;
static volatile sig_atomic_t handler_failure;
static volatile sig_atomic_t handler_depth;
static int release_b = -1;

static void on_child(int signal) {
  int saved_errno = errno;
  sigset_t mask;
  ++handler_depth;
  if (signal != SIGCHLD || handler_depth != 1 ||
      sigprocmask(SIG_SETMASK, NULL, &mask) != 0 ||
      sigismember(&mask, SIGCHLD) != 1 ||
      sigismember(&mask, SIGUSR1) != 1 ||
      sigismember(&mask, SIGUSR2) != 1)
    handler_failure = 1;
  ++handlers;
  /* A has already been dequeued into this real application handler. B's
   * later exit cannot coalesce with A's active frame. Only this handler owns
   * the release writer, and no EOF or duplicate byte is a successful release. */
  if (handlers == 1 && write(release_b, "B", 1) != 1)
    handler_failure = 1;
  --handler_depth;
  errno = saved_errno;
}

static void require(int condition, const char *message) {
  if (!condition) {
    fprintf(stderr, "wait-interruption failure: %s\n", message);
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
    require(left >= 0 && right >= 0, "defined Linux mask membership");
    if (left != right)
      same = 0;
  }
  return same;
}

int main(int argc, char **argv) {
  require(argc == 2 &&
              (strcmp(argv[1], "interrupt") == 0 ||
               strcmp(argv[1], "restart") == 0),
          "literal interrupt/restart mode");
  int restart = strcmp(argv[1], "restart") == 0;
  struct sigaction requested, original, final;
  sigset_t mask, after;
  memset(&requested, 0, sizeof(requested));
  requested.sa_handler = on_child;
  requested.sa_flags = restart ? SA_RESTART : 0;
  require(sigemptyset(&requested.sa_mask) == 0 &&
              sigaddset(&requested.sa_mask, SIGUSR1) == 0 &&
              sigaction(SIGCHLD, &requested, NULL) == 0 &&
              sigaction(SIGCHLD, NULL, &original) == 0,
          "install and read stable caught action");
  require(original.sa_handler == on_child &&
              (original.sa_flags & (SA_NOCLDWAIT | SA_NODEFER | SA_RESETHAND)) == 0 &&
              !!(original.sa_flags & SA_RESTART) == restart,
          "literal supported persistent handler flags");
  require(sigemptyset(&mask) == 0 && sigaddset(&mask, SIGUSR2) == 0 &&
              sigprocmask(SIG_SETMASK, &mask, NULL) == 0,
          "unblocked SIGCHLD and unrelated guest mask");

  int channel[2];
  require(pipe(channel) == 0, "dedicated handler-to-B pipe");
  pid_t first = fork();
  require(first >= 0, "first fork");
  if (first == 0) {
    require(close(channel[0]) == 0 && close(channel[1]) == 0,
            "A owns no B release endpoint");
    char byte = 0;
    require(read_retry(STDIN_FILENO, &byte, 1) == 1 && byte == 'A',
            "A is released only after the real WaitChild(B) park");
    _exit(31);
  }
  pid_t second = fork();
  require(second >= 0, "second fork");
  if (second == 0) {
    require(close(STDIN_FILENO) == 0 && close(channel[1]) == 0,
            "B cannot consume A's input or own its release writer");
    char byte = 0;
    require(read_retry(channel[0], &byte, 1) == 1 && byte == 'B',
            "only A's actual handler releases B");
    require(close(channel[0]) == 0, "B closes its reader");
    _exit(47);
  }
  require(close(STDIN_FILENO) == 0 && close(channel[0]) == 0,
          "parent owns only B's handler writer");
  release_b = channel[1];
  printf("wait-interruption ready parent=%ld first=%ld second=%ld\n",
         (long)getpid(), (long)first, (long)second);
  require(fflush(stdout) == 0, "publish exact logical identities");

  int status = 0;
  errno = 0;
  pid_t result = waitpid(second, &status, 0);
  int first_errno = errno;
  if (restart) {
    require(result == second && handlers > 0,
            "original SA_RESTART wait resumes only after the actual handler");
  } else {
    require(result == -1 && first_errno == EINTR && handlers > 0,
            "original flags0 wait returns EINTR after the actual handler");
    do {
      result = waitpid(second, &status, 0);
    } while (result < 0 && errno == EINTR);
  }
  require(result == second && WIFEXITED(status) && WEXITSTATUS(status) == 47,
          "exact B result after its handler dependency");
  do {
    result = waitpid(first, &status, 0);
  } while (result < 0 && errno == EINTR);
  require(result == first && WIFEXITED(status) && WEXITSTATUS(status) == 31,
          "exact A remains reapable");
  require(sigprocmask(SIG_SETMASK, NULL, &after) == 0 && same_mask(&mask, &after),
          "handler return restores the complete guest mask");
  require(sigaction(SIGCHLD, NULL, &final) == 0 &&
              final.sa_handler == original.sa_handler &&
              final.sa_flags == original.sa_flags &&
              final.sa_restorer == original.sa_restorer &&
              same_mask(&final.sa_mask, &original.sa_mask),
          "capture and handler return preserve the complete defined action");
  require(handlers == 2 && handler_failure == 0 && handler_depth == 0,
          "two causally distinct real handlers, depth one, exact mask and write");
  require(close(release_b) == 0, "parent closes handler writer");
  puts(restart ? "wait-interruption completed restart second=47 first=31 handlers=2"
               : "wait-interruption completed interrupt second=47 first=31 handlers=2");
  return 0;
}
