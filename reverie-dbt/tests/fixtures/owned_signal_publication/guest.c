/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

/* The explicitly selected prototype Tool completes these tagged operations. */
#define PROBE_TAG UINT64_C(0x4f535050)
#define READY_REPLY 31337L
#define COMPLETE_REPLY 41414L
#define FIRST_STATUS 31
#define SECOND_STATUS 47
#define CHILD_REPLY_BASE 4411392L

static void require(int condition, const char *message) {
  if (!condition) {
    fprintf(stderr, "owned signal publication guest failure: %s\n", message);
    exit(90);
  }
}

static ssize_t read_retry(int fd, void *buffer, size_t size) {
  ssize_t result;
  do {
    result = read(fd, buffer, size);
  } while (result < 0 && errno == EINTR);
  return result;
}

static void await_release(char expected) {
  char byte = 0;
  require(read_retry(STDIN_FILENO, &byte, 1) == 1 && byte == expected,
          "independent host release names this child");
}

static void announce_child(long role) {
  long reply = syscall(SYS_getuid, PROBE_TAG, (uint64_t)(role + 2),
                       (long)getpid(), (long)getppid());
  require(reply == CHILD_REPLY_BASE + role,
          "child identity marker receives exact independent reply");
}

static void current_mask(sigset_t *mask) {
  memset(mask, 0, sizeof(*mask));
  require(sigprocmask(SIG_SETMASK, NULL, mask) == 0, "read guest mask");
}

static void current_action(struct sigaction *action) {
  memset(action, 0, sizeof(*action));
  require(sigaction(SIGCHLD, NULL, action) == 0, "read SIGCHLD action");
}

static int same_membership(const sigset_t *left, const sigset_t *right) {
  int equal = 1;
  /* Every defined Linux x86-64 signal, including libc-reserved 32 and 33. */
  for (int signal = 1; signal <= 64; ++signal) {
    int a = sigismember(left, signal);
    int b = sigismember(right, signal);
    require(a >= 0 && b >= 0, "query every defined mask membership");
    if (a != b)
      equal = 0;
  }
  return equal;
}

static void unchanged(const sigset_t *mask, const struct sigaction *action) {
  sigset_t actual_mask;
  struct sigaction actual_action;
  current_mask(&actual_mask);
  current_action(&actual_action);
  require(same_membership(mask, &actual_mask), "complete guest mask preserved");
  require(action->sa_handler == actual_action.sa_handler &&
              action->sa_flags == actual_action.sa_flags &&
              action->sa_restorer == actual_action.sa_restorer &&
              same_membership(&action->sa_mask, &actual_action.sa_mask),
          "complete defined SIGCHLD action preserved");
}

static void observe_and_reap(pid_t child, int code) {
  siginfo_t info;
  memset(&info, 0, sizeof(info));
  require(waitid(P_PID, (id_t)child, &info, WEXITED | WNOWAIT) == 0 &&
              info.si_pid == child && info.si_code == CLD_EXITED &&
              info.si_status == code,
          "real parent observes exact child/status without reaping");
  int status = 0;
  require(waitpid(child, &status, 0) == child && WIFEXITED(status) &&
              WEXITSTATUS(status) == code,
          "exact child reap and status");
}

int main(void) {
  sigset_t unblocked, blocked;
  struct sigaction requested, action;
  require(sigemptyset(&unblocked) == 0 &&
              sigaddset(&unblocked, SIGUSR1) == 0 &&
              sigaddset(&unblocked, SIGUSR2) == 0,
          "prepare unrelated blocked signals");
  blocked = unblocked;
  require(sigaddset(&blocked, SIGCHLD) == 0 &&
              sigprocmask(SIG_SETMASK, &blocked, NULL) == 0,
          "block SIGCHLD before creating children");
  memset(&requested, 0, sizeof(requested));
  requested.sa_handler = SIG_DFL;
  require(sigemptyset(&requested.sa_mask) == 0 &&
              sigaction(SIGCHLD, &requested, NULL) == 0,
          "default SIGCHLD without auto-reap");
  current_action(&action);
  require(action.sa_handler == SIG_DFL &&
              (action.sa_flags & SA_NOCLDWAIT) == 0,
          "literal default non-auto-reap disposition");

  int first_lifetime[2];
  require(pipe(first_lifetime) == 0, "create child lifetime pipe");
  pid_t first = fork();
  require(first >= 0, "fork first real child");
  if (first == 0) {
    require(close(first_lifetime[0]) == 0, "first closes read end");
    announce_child(1);
    await_release('A');
    /* Only this child owns the writer after the parent's closes below. */
    _exit(FIRST_STATUS);
  }
  pid_t second = fork();
  require(second >= 0, "fork second real child");
  if (second == 0) {
    require(close(first_lifetime[1]) == 0, "second closes writer");
    /* Announce before waiting for A's EOF, so authenticating B cannot depend
     * on releasing A or resolving the parent's still-borrowed marker. */
    announce_child(2);
    char byte = 0;
    require(read_retry(first_lifetime[0], &byte, 1) == 0,
            "first child's real writer lifetime ends before second release");
    require(close(first_lifetime[0]) == 0, "second closes read end");
    /* This cannot consume A's byte before the first child has read it/exited. */
    await_release('B');
    _exit(SECOND_STATUS);
  }
  require(close(first_lifetime[0]) == 0 && close(first_lifetime[1]) == 0,
          "parent retains neither child lifetime endpoint");

  long ready = syscall(SYS_getuid, PROBE_TAG, UINT64_C(1), (long)first,
                       (long)second, (long)getpid());
  require(ready == READY_REPLY, "ready marker receives exact ordinary reply");
  observe_and_reap(first, FIRST_STATUS);
  sigset_t pending;
  memset(&pending, 0, sizeof(pending));
  require(sigpending(&pending) == 0 && sigismember(&pending, SIGCHLD) == 1,
          "first notification remains pending after reap while blocked");
  unchanged(&blocked, &action);
  require(sigprocmask(SIG_SETMASK, &unblocked, NULL) == 0,
          "lawfully unblock first authentic notification");

  /* The host releases B only after A's real Tool callback has armed its
   * interval. B's capture cannot depend on A completing or recursively entering
   * a Tool. The selected Tool checks both callbacks before replying here. */
  long completed = syscall(SYS_getuid, PROBE_TAG, UINT64_C(2), (long)first,
                           (long)second, (long)getpid());
  require(completed == COMPLETE_REPLY,
          "post-signal marker receives exact ordinary reply");
  unchanged(&unblocked, &action);
  observe_and_reap(second, SECOND_STATUS);
  unchanged(&unblocked, &action);
  printf("owned signal publication completed first=%d second=%d\n", FIRST_STATUS,
         SECOND_STATUS);
  return 0;
}
