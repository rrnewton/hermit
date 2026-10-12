/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Guest for hermit-cli/tests/cli.rs: pids inside a nested PID namespace
 * (https://github.com/rrnewton/hermit/issues/4155,
 * https://github.com/rrnewton/hermit/issues/4090).
 *
 * usage: pid_namespace_nested <mode>
 *
 * The outer process clones a nested PID namespace's init with
 * clone(CLONE_NEWPID); the init runs one mode, prints one line, and the outer
 * process prints the init's exit status. Natively it needs
 * `unshare --user --map-root-user`.
 *   self_kill:    the init sends kill(getpid(), SIGUSR1) with a handler: rc 0,
 *                 and the handler ran.
 *   fork_wait:    the init forks; fork's return, the child's getpid and
 *                 getppid, and waitpid(child) with its status.
 *   wait_any:     the init forks and waits with waitpid(-1).
 *   sibling:      the init forks two children (pids 2 and 3) and sends
 *                 SIGUSR1 to pid 3, whose handler exits 7. Under Hermit the
 *                 outer process's own pid is 3 in the container's namespace,
 *                 so a pid read as a container pid signals the wrong process.
 *   init_sigkill: the init sends kill(1, SIGKILL) and kill(getpid(), SIGTERM)
 *                 with default dispositions: Linux drops both and answers 0.
 *   orphan:       the init forks a child that forks a grandchild and exits;
 *                 the grandchild's getppid() becomes 1, the nested init, which
 *                 reaps it with waitpid(-1).
 *   outer:        the init sends kill(<the outer process's pid>, 0): no task of
 *                 its namespace has that pid, so ESRCH.
 *   tgkill:       the init starts a thread and sends tgkill(getpid(), <its
 *                 tid>, SIGUSR1), which runs the handler on that thread.
 *   init_leaves_zombie: the init forks a child that exits, and exits without
 *                 waiting for it; only a zombie is left in the namespace.
 *   init_dies:    the init forks a child that waits forever, and exits; Linux
 *                 kills the child (zap_pid_namespace) at a moment the host
 *                 chooses, which Hermit reports as a determinism loss.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile sig_atomic_t ran, ran_tid;
static void on_usr1(int sig) {
  (void)sig;
  ran = 1;
  ran_tid = (int)syscall(SYS_gettid);
}
static void on_usr1_exit(int sig) {
  (void)sig;
  _exit(7);
}
static void handle(int sig, void (*fn)(int)) {
  struct sigaction a;
  memset(&a, 0, sizeof a);
  a.sa_handler = fn;
  if (sigaction(sig, &a, NULL)) _exit(90);
}
static const char *err(int rc) { return rc ? strerrorname_np(errno) : "0"; }

static void *idle(void *arg) {
  volatile int *tid = arg;
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGUSR1);
  pthread_sigmask(SIG_UNBLOCK, &set, NULL);
  *tid = (int)syscall(SYS_gettid);
  while (!ran) {
    struct timespec ms = {0, 1000000};
    nanosleep(&ms, NULL);
  }
  return NULL;
}

static int outer_pid;

static int init_main(const char *mode) {
  if (!strcmp(mode, "self_kill")) {
    handle(SIGUSR1, on_usr1);
    int rc = kill(getpid(), SIGUSR1);
    printf("self_kill pid=%d rc=%s ran=%d\n", getpid(), err(rc), ran);
  } else if (!strcmp(mode, "fork_wait")) {
    pid_t c = fork();
    if (c == 0) {
      printf("child pid=%d ppid=%d\n", getpid(), getppid());
      fflush(stdout);
      _exit(5);
    }
    int st;
    pid_t w = waitpid(c, &st, 0);
    printf("fork_wait fork=%d waitpid=%d status=%d\n", c, w, WEXITSTATUS(st));
  } else if (!strcmp(mode, "wait_any")) {
    pid_t c = fork();
    if (c == 0) _exit(6);
    int st;
    pid_t w = waitpid(-1, &st, 0);
    printf("wait_any fork=%d waitpid=%d status=%d\n", c, w, WEXITSTATUS(st));
  } else if (!strcmp(mode, "sibling")) {
    int ready[2];
    if (pipe(ready)) _exit(94);
    pid_t a = fork();
    if (a == 0) {
      for (;;) pause();
    }
    pid_t b = fork();
    if (b == 0) {
      handle(SIGUSR1, on_usr1_exit);
      char byte = 1;
      if (write(ready[1], &byte, 1) != 1) _exit(95);
      for (;;) pause();
    }
    char byte;
    if (read(ready[0], &byte, 1) != 1) _exit(96);
    int rc = kill(3, SIGUSR1);
    int st;
    pid_t w = waitpid(b, &st, 0);
    kill(a, SIGKILL);
    waitpid(a, NULL, 0);
    printf("sibling a=%d b=%d kill3=%s waited=%d status=%d\n", a, b, err(rc), w,
           WIFEXITED(st) ? WEXITSTATUS(st) : -1);
  } else if (!strcmp(mode, "init_sigkill")) {
    int rc1 = kill(1, SIGKILL);
    int rc2 = kill(getpid(), SIGTERM);
    printf("init_sigkill kill1=%s term=%s alive=1\n", err(rc1), err(rc2));
  } else if (!strcmp(mode, "orphan")) {
    int fds[2];
    if (pipe(fds)) _exit(91);
    pid_t c = fork();
    if (c == 0) {
      pid_t g = fork();
      if (g == 0) {
        close(fds[0]);
        while (getppid() != 1) usleep(1000);
        int ppid = getppid();
        if (write(fds[1], &ppid, sizeof ppid) != sizeof ppid) _exit(92);
        _exit(0);
      }
      _exit(0);
    }
    close(fds[1]);
    waitpid(c, NULL, 0);
    int ppid = -1;
    if (read(fds[0], &ppid, sizeof ppid) != sizeof ppid) ppid = -2;
    int st;
    pid_t w = waitpid(-1, &st, 0);
    printf("orphan grandchild_ppid=%d reaped=%d\n", ppid, w);
  } else if (!strcmp(mode, "outer")) {
    int rc = kill(outer_pid, 0);
    printf("outer kill0=%s\n", err(rc));
  } else if (!strcmp(mode, "tgkill")) {
    handle(SIGUSR1, on_usr1);
    volatile int tid = 0;
    pthread_t t;
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    pthread_sigmask(SIG_BLOCK, &set, NULL);
    if (pthread_create(&t, NULL, idle, (void *)&tid)) _exit(93);
    while (!tid) usleep(1000);
    int rc = (int)syscall(SYS_tgkill, getpid(), tid, SIGUSR1);
    pthread_join(t, NULL);
    printf("tgkill thread=%d rc=%s on_thread=%d\n", tid, err(rc), ran_tid == tid);
  } else if (!strcmp(mode, "init_leaves_zombie")) {
    int fds[2];
    if (pipe(fds)) _exit(97);
    pid_t c = fork();
    if (c == 0) _exit(0);
    /* Wait until the child has exited, without reaping it. */
    siginfo_t info;
    memset(&info, 0, sizeof info);
    waitid(P_PID, c, &info, WEXITED | WNOWAIT);
    printf("init_leaves_zombie child=%d\n", c);
  } else if (!strcmp(mode, "init_dies")) {
    pid_t c = fork();
    if (c == 0) {
      for (;;) pause();
    }
    printf("init_dies child=%d\n", c);
  } else {
    return 64;
  }
  fflush(stdout);
  return 0;
}

int main(int argc, char **argv) {
  const char *mode = argc > 1 ? argv[1] : "self_kill";
  outer_pid = getpid();
  fflush(stdout);
  long c = syscall(SYS_clone, (long)(CLONE_NEWPID | SIGCHLD), 0L, 0L, 0L, 0L);
  if (c < 0) {
    printf("clone(CLONE_NEWPID) %s\n", strerrorname_np(errno));
    return 0;
  }
  if (c == 0) _exit(init_main(mode));
  int st;
  if (waitpid((pid_t)c, &st, 0) != (pid_t)c) return 81;
  printf("init exit=%d\n", WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st));
  return 0;
}
