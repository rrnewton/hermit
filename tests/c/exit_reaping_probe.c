/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * What a parent observes after a child's exit, in four shapes:
 *
 *   sigign     SIGCHLD set to SIG_IGN: the kernel auto-reaps and sends no
 *              SIGCHLD, so wait reports ECHILD and nothing is pending.
 *   nocldwait  SA_NOCLDWAIT: the kernel auto-reaps but still sends SIGCHLD,
 *              so wait reports ECHILD and SIGCHLD is pending.
 *   rawexit    the child ends with the raw exit system call; the parent
 *              collects status 7 and finds SIGCHLD pending.
 *   launched   SIGCHLD as the process was started with: the parent changes
 *              no disposition, so a launcher that left SIGCHLD ignored gets
 *              sigign's lines (execve keeps an ignored action), and one that
 *              left it at its default gets a collected status 7.
 *   adopted    SIGCHLD as the process was started with, in a subreaper
 *              (PR_SET_CHILD_SUBREAPER). Its child, with SIGCHLD at the
 *              default, starts a grandchild, sees its zombie with waitid
 *              and WNOWAIT, and exits, so Linux hands the zombie to the
 *              subreaper (forget_original_parent). With SIGCHLD ignored the
 *              subreaper reaps the adopted zombie at once, as it does its
 *              own child, so its wait reports ECHILD.
 *
 * The parent blocks SIGCHLD, and learns of the exit by reading a pipe whose
 * only write end the child holds: EOF means the child's descriptors are
 * closed. A backend that completes exits asynchronously must not let the
 * parent run again until the exit is complete, SIGCHLD included, so every
 * line below is the same in every run.
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static void fail(const char* what) {
  fprintf(stderr, "%s: %s\n", what, strerror(errno));
  exit(1);
}

static int sigchld_pending(void) {
  sigset_t pending;
  if (sigpending(&pending) != 0) {
    fail("sigpending");
  }
  return sigismember(&pending, SIGCHLD);
}

/* The adopted mode, with SIGCHLD blocked as main leaves it. */
static int run_adopted(void) {
  if (prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) != 0) {
    fail("PR_SET_CHILD_SUBREAPER");
  }
  int pipe_fds[2];
  if (pipe(pipe_fds) != 0) {
    fail("pipe");
  }
  pid_t child = fork();
  if (child < 0) {
    fail("fork");
  }
  if (child == 0) {
    close(pipe_fds[0]);
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_handler = SIG_DFL;
    if (sigaction(SIGCHLD, &action, NULL) != 0) {
      _exit(100);
    }
    pid_t grandchild = fork();
    if (grandchild < 0) {
      _exit(101);
    }
    if (grandchild == 0) {
      _exit(9);
    }
    siginfo_t info;
    memset(&info, 0, sizeof(info));
    if (waitid(P_PID, grandchild, &info, WEXITED | WNOWAIT) != 0 ||
        info.si_pid != grandchild || info.si_status != 9) {
      _exit(102);
    }
    _exit(7);
  }
  close(pipe_fds[1]);
  char byte;
  ssize_t got = read(pipe_fds[0], &byte, 1);
  close(pipe_fds[0]);
  int status = 0;
  pid_t waited = waitpid(-1, &status, 0);
  const char* wait_result = waited > 0 ? "child"
      : (waited < 0 && errno == ECHILD) ? "ECHILD"
                                        : "other";
  printf(
      "adopted read=%zd sigchld_pending=%d wait=%s status=%d\n",
      got,
      sigchld_pending(),
      wait_result,
      waited > 0 && WIFEXITED(status) ? WEXITSTATUS(status) : -1);
  return 0;
}

int main(int argc, char** argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s sigign|nocldwait|rawexit|launched|adopted\n", argv[0]);
    return 2;
  }
  const char* mode = argv[1];
  int sigign = strcmp(mode, "sigign") == 0;
  int nocldwait = strcmp(mode, "nocldwait") == 0;
  int rawexit = strcmp(mode, "rawexit") == 0;
  int launched = strcmp(mode, "launched") == 0;
  int adopted = strcmp(mode, "adopted") == 0;
  if (!sigign && !nocldwait && !rawexit && !launched && !adopted) {
    fprintf(stderr, "unknown mode %s\n", mode);
    return 2;
  }

  sigset_t chld;
  sigemptyset(&chld);
  sigaddset(&chld, SIGCHLD);
  if (sigprocmask(SIG_BLOCK, &chld, NULL) != 0) {
    fail("sigprocmask");
  }
  if (sigign || nocldwait) {
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_handler = sigign ? SIG_IGN : SIG_DFL;
    action.sa_flags = nocldwait ? SA_NOCLDWAIT : 0;
    if (sigaction(SIGCHLD, &action, NULL) != 0) {
      fail("sigaction");
    }
  }

  if (adopted) {
    return run_adopted();
  }

  for (int round = 0; round < 3; round++) {
    int pipe_fds[2];
    if (pipe(pipe_fds) != 0) {
      fail("pipe");
    }
    pid_t child = fork();
    if (child < 0) {
      fail("fork");
    }
    if (child == 0) {
      close(pipe_fds[0]);
      if (rawexit) {
        syscall(SYS_exit, 7);
      }
      _exit(7);
    }
    close(pipe_fds[1]);
    char byte;
    ssize_t got = read(pipe_fds[0], &byte, 1);
    close(pipe_fds[0]);
    int pending = sigchld_pending();
    int status = 0;
    pid_t waited = waitpid(child, &status, 0);
    const char* wait_result = waited == child ? "child"
        : (waited < 0 && errno == ECHILD) ? "ECHILD"
                                          : "other";
    printf(
        "%s round=%d read=%zd sigchld_pending=%d wait=%s status=%d\n",
        mode,
        round,
        got,
        pending,
        wait_result,
        waited == child && WIFEXITED(status) ? WEXITSTATUS(status) : -1);
    /* Consume this round's SIGCHLD, if any, before the next. */
    struct timespec zero = {0, 0};
    while (sigtimedwait(&chld, NULL, &zero) == SIGCHLD) {
    }
  }
  return 0;
}
