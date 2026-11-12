/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Guest for hermit-cli/tests/cli.rs: sends and armings of stop signals
 * (SIGSTOP, SIGTSTP, SIGTTIN, SIGTTOU) that the in-guest backends cannot hold,
 * and their controls (https://github.com/rrnewton/hermit/issues/5430).
 *
 * usage: stop_send_refusal <mode>
 *
 * Every line is written with write(2), so output order is the syscall order.
 * A line the guest writes right after a send ("after-kill") witnesses that
 * the run went on past it: a refused send on the owned in-guest route ends
 * the run before it.
 *
 *   kill-stop:  kill(child, SIGSTOP) to a forked child; then "after-kill", and
 *               the child is released, waited for with WUNTRACED, continued
 *               and reaped.
 *   tstp-ign:   SIGTSTP to a child in a process group of its own (so that a
 *               default SIGTSTP is not discarded as sent to an orphaned group)
 *               whose action is SIG_IGN and which does not block it: Linux
 *               discards it when it is sent, and nothing stops.
 *   tstp-ign-blocked-then-default: the same child also blocks SIGTSTP, so
 *               Linux queues it; the child then restores SIG_DFL and unblocks
 *               it, and stops.
 *   tstp-ign-blocked-then-unblock: the same queued SIGTSTP, unblocked while
 *               the child still ignores it: nothing stops.
 *   tstp-dfl:   SIGTSTP to a child whose action is SIG_DFL: it stops.
 *   cont:       SIGCONT to a running child.
 *   leader-unblocked, leader-blocked: native controls of which task's mask
 *               decides a process-directed send. The child process ignores
 *               SIGTSTP and has a second thread that blocks it; its leader
 *               blocks it too only in leader-blocked. kill(child, SIGTSTP),
 *               then the child restores SIG_DFL and the second thread unblocks
 *               it: the process stops only if the signal was queued, which
 *               Linux decides from the leader's mask (kernel/signal.c:967 and
 *               :103-122 at v6.9).
 *   timers:     timer_create with SIGEV_SIGNAL and SIGTSTP, and with
 *               SIGEV_THREAD_ID naming the caller and SIGSTOP, each printed
 *               with its errno; the timers are never armed.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static void say(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
static void say(const char *fmt, ...) {
  char buf[256];
  va_list ap;
  va_start(ap, fmt);
  int n = vsnprintf(buf, sizeof buf, fmt, ap);
  va_end(ap);
  if (n > 0) (void)!write(1, buf, (size_t)n);
}

static void set_action(int sig, void (*handler)(int)) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = handler;
  sigaction(sig, &action, NULL);
}

static void set_blocked(int how, int sig) {
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, sig);
  sigprocmask(how, &set, NULL);
}

/* How a waited-for child ended or stopped; true when it stopped. */
static int report_wait(const char *what, pid_t child, int options) {
  int status = 0;
  pid_t waited = waitpid(child, &status, options);
  if (waited != child) {
    say("%s wait=%d errno=%d\n", what, (int)waited, errno);
  } else if (WIFSTOPPED(status)) {
    say("%s stopped=%d\n", what, WSTOPSIG(status));
    return 1;
  } else if (WIFEXITED(status)) {
    say("%s exited=%d\n", what, WEXITSTATUS(status));
  } else {
    say("%s signaled=%d\n", what, WIFSIGNALED(status) ? WTERMSIG(status) : -1);
  }
  return 0;
}

/* Continues and reaps a child that stopped. */
static void continue_stopped(pid_t child, int stopped) {
  if (!stopped) return;
  kill(child, SIGCONT);
  report_wait("last", child, 0);
}

static int mode_kill_stop(void) {
  int release[2];
  if (pipe(release)) return 3;
  pid_t child = fork();
  if (child == 0) {
    char byte;
    (void)!read(release[0], &byte, 1);
    _exit(7);
  }
  int sent = kill(child, SIGSTOP);
  int error = sent ? errno : 0;
  (void)!write(1, "after-kill\n", 11);
  say("stop kill=%d errno=%d\n", sent, error);
  (void)!write(release[1], "r", 1);
  continue_stopped(child, report_wait("first", child, WUNTRACED));
  return 0;
}

enum child_setup { IGN, IGN_BLOCKED_THEN_DEFAULT, IGN_BLOCKED_THEN_UNBLOCK, DFL };

static int mode_tstp(enum child_setup setup, int signal) {
  int ready[2], go[2];
  if (pipe(ready) || pipe(go)) return 3;
  pid_t child = fork();
  if (child == 0) {
    setpgid(0, 0);
    if (setup != DFL) set_action(SIGTSTP, SIG_IGN);
    if (setup == IGN_BLOCKED_THEN_DEFAULT || setup == IGN_BLOCKED_THEN_UNBLOCK)
      set_blocked(SIG_BLOCK, SIGTSTP);
    (void)!write(ready[1], "x", 1);
    char byte;
    (void)!read(go[0], &byte, 1);
    if (setup == IGN_BLOCKED_THEN_DEFAULT) set_action(SIGTSTP, SIG_DFL);
    if (setup == IGN_BLOCKED_THEN_DEFAULT || setup == IGN_BLOCKED_THEN_UNBLOCK)
      set_blocked(SIG_UNBLOCK, SIGTSTP);
    _exit(0);
  }
  setpgid(child, child);
  char byte;
  (void)!read(ready[0], &byte, 1);
  int sent = kill(child, signal);
  int error = sent ? errno : 0;
  (void)!write(1, "after-kill\n", 11);
  say("send signal=%d kill=%d errno=%d\n", signal, sent, error);
  if (setup == DFL && signal == SIGTSTP) {
    /* The child stops at once; it reads its go byte once continued. */
    report_wait("first", child, WUNTRACED);
    kill(child, SIGCONT);
    (void)!write(go[1], "g", 1);
    report_wait("last", child, 0);
    return 0;
  }
  (void)!write(go[1], "g", 1);
  continue_stopped(child, report_wait("first", child, WUNTRACED));
  return 0;
}

static int thread_go[2], thread_done[2];

static void *sibling(void *unused) {
  (void)unused;
  set_blocked(SIG_BLOCK, SIGTSTP);
  (void)!write(thread_done[1], "b", 1);
  char byte;
  (void)!read(thread_go[0], &byte, 1);
  set_blocked(SIG_UNBLOCK, SIGTSTP);
  return NULL;
}

static int mode_leader(int leader_blocks) {
  int ready[2], go[2];
  if (pipe(ready) || pipe(go)) return 3;
  pid_t child = fork();
  if (child == 0) {
    setpgid(0, 0);
    if (pipe(thread_go) || pipe(thread_done)) _exit(3);
    set_action(SIGTSTP, SIG_IGN);
    pthread_t thread;
    if (pthread_create(&thread, NULL, sibling, NULL)) _exit(3);
    char byte;
    (void)!read(thread_done[0], &byte, 1);
    if (leader_blocks) set_blocked(SIG_BLOCK, SIGTSTP);
    (void)!write(ready[1], "x", 1);
    (void)!read(go[0], &byte, 1);
    set_action(SIGTSTP, SIG_DFL);
    (void)!write(thread_go[1], "u", 1);
    pthread_join(thread, NULL);
    _exit(0);
  }
  setpgid(child, child);
  char byte;
  (void)!read(ready[0], &byte, 1);
  int sent = kill(child, SIGTSTP);
  say("send signal=%d kill=%d errno=%d\n", SIGTSTP, sent, sent ? errno : 0);
  (void)!write(go[1], "g", 1);
  continue_stopped(child, report_wait("first", child, WUNTRACED));
  return 0;
}

/* The kernel's struct sigevent: value, signo, notify, then a union that
 * holds the thread id for SIGEV_THREAD_ID, 64 bytes in all. */
struct kernel_sigevent {
  union sigval value;
  int signo;
  int notify;
  union {
    int pad[12];
    int tid;
  } un;
};

static void create_timer(const char *what, int notify, int signal) {
  struct kernel_sigevent event;
  memset(&event, 0, sizeof event);
  event.signo = signal;
  event.notify = notify;
  event.un.tid = (int)gettid();
  int id = -1;
  long created = syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &id);
  say("%s=%ld errno=%d\n", what, created, created ? errno : 0);
  if (created == 0) syscall(SYS_timer_delete, id);
}

static int mode_timers(void) {
  create_timer("timer-signal-tstp", SIGEV_SIGNAL, SIGTSTP);
  create_timer("timer-thread-id-stop", SIGEV_THREAD_ID, SIGSTOP);
  return 0;
}

int main(int argc, char **argv) {
  if (argc != 2) return 2;
  const char *mode = argv[1];
  if (!strcmp(mode, "kill-stop")) return mode_kill_stop();
  if (!strcmp(mode, "tstp-ign")) return mode_tstp(IGN, SIGTSTP);
  if (!strcmp(mode, "tstp-ign-blocked-then-default"))
    return mode_tstp(IGN_BLOCKED_THEN_DEFAULT, SIGTSTP);
  if (!strcmp(mode, "tstp-ign-blocked-then-unblock"))
    return mode_tstp(IGN_BLOCKED_THEN_UNBLOCK, SIGTSTP);
  if (!strcmp(mode, "tstp-dfl")) return mode_tstp(DFL, SIGTSTP);
  if (!strcmp(mode, "cont")) return mode_tstp(DFL, SIGCONT);
  if (!strcmp(mode, "leader-unblocked")) return mode_leader(0);
  if (!strcmp(mode, "leader-blocked")) return mode_leader(1);
  if (!strcmp(mode, "timers")) return mode_timers();
  return 2;
}
