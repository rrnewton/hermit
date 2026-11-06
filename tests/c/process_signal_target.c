/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Which thread of a multithreaded process takes a process-directed signal
 * that the kernel itself generates: a child's exit SIGCHLD, an ITIMER_REAL
 * expiry and a POSIX timer's SIGEV_SIGNAL expiry (SIGSHARE S3b). Linux
 * queues such a signal on the process's shared queue and complete_signal
 * (kernel/signal.c) wakes the thread it names, if that thread wants it (it
 * does not block the signal, and is running or has no signal pending), and
 * otherwise the first thread that does, walking the thread list from the
 * process's curr_target; with none, the signal waits until a thread unblocks
 * it. The thread named is the child's parent thread for SIGCHLD (the thread
 * that forked it), and the leader for a timer.
 *
 * The process has the main thread M and two workers, W1 and W2, created in
 * that order. Each worker waits in read() on a pipe of its own, and M, when
 * it leaves the signal unblocked, in poll() on its own (bounded at 200 ms),
 * so no waiting thread has a signal pending and Linux would wake it; the
 * handler (no SA_RESTART) writes its thread's name to a report pipe, which M
 * reads once the event is over. Each mode prints one line, the same natively
 * and under Hermit:
 *
 *   chld-creator  SIGCHLD caught and unblocked everywhere; M forks the child
 *                 and waits in poll(): M is named and takes it.
 *                 "receiver=M interrupted=M".
 *   chld-walk     the same, with M and W1 blocking SIGCHLD: the walk from
 *                 curr_target (M) passes W1 and reaches W2.
 *                 "receiver=W2 interrupted=W2".
 *   chld-worker   W1 forks the child and then waits in read(); every thread
 *                 leaves SIGCHLD unblocked: W1, the child's parent thread, is
 *                 named and takes it. "receiver=W1 interrupted=W1".
 *   chld-queued   every thread blocks SIGCHLD: it stays pending in the shared
 *                 queue (M's sigpending shows it) until W2 unblocks it, and
 *                 W2 takes it. "pending=1 receiver=W2".
 *   alrm-leader   a one-shot ITIMER_REAL with SIGALRM caught and unblocked
 *                 everywhere: the leader M is named and takes it.
 *                 "receiver=M interrupted=M".
 *   alrm-walk     the same, with M and W1 blocking SIGALRM: W2 takes it.
 *                 "receiver=W2 interrupted=W2".
 *   alrm-queued   every thread blocks SIGALRM: it stays pending until W1
 *                 unblocks it, and W1 takes it. "pending=1 receiver=W1".
 *   posix-walk    a one-shot CLOCK_MONOTONIC POSIX timer whose SIGEV_SIGNAL
 *                 is SIGUSR1, caught; M and W1 block SIGUSR1: W2 takes it.
 *                 "receiver=W2 interrupted=W2".
 *   alrm-ignored-blocked
 *                 SIGALRM ignored and blocked in every thread: Linux queues an
 *                 ignored signal that is blocked (sig_ignored), so after the
 *                 one-shot expiry it is pending; M then installs a handler and
 *                 W2 unblocks it and takes it. "pending=1 receiver=W2".
 *   alrm-ignored-unblocked
 *                 SIGALRM ignored, and unblocked in W2 alone: W2 does not block
 *                 it, so Linux discards it as it is generated and nothing is
 *                 pending; M then installs a handler and W2 takes nothing.
 *                 "pending=0 receiver=none".
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

enum { M, W1, W2, THREADS };
static const char* const NAMES[THREADS] = {"M", "W1", "W2"};

static __thread int self = M;
static int wake_pipes[THREADS][2];
static int report_pipe[2];
static int interrupted_by = -1;
static int signo;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cond = PTHREAD_COND_INITIALIZER;
static int ready;
static int fork_now;
static pid_t forked;

static void die(const char* what) {
  fprintf(stderr, "%s: %s\n", what, strerror(errno));
  _exit(99);
}

static void on_signal(int sig) {
  (void)sig;
  int saved = errno;
  char who = (char)self;
  if (write(report_pipe[1], &who, 1) != 1) {
    _exit(98);
  }
  errno = saved;
}

static void set_action(int sig, void (*handler)(int)) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = handler;
  if (sigaction(sig, &action, NULL) != 0) {
    die("sigaction");
  }
}

static void mask(int how, int sig) {
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, sig);
  if (pthread_sigmask(how, &set, NULL) != 0) {
    die("pthread_sigmask");
  }
}

static void sleep_ms(long ms) {
  struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
  while (nanosleep(&ts, &ts) != 0 && errno == EINTR) {
  }
}

static pid_t fork_child(void) {
  pid_t child = fork();
  if (child < 0) {
    die("fork");
  }
  if (child == 0) {
    sleep_ms(20);
    _exit(0);
  }
  return child;
}

/* Wait in read() on this thread's own pipe until released; a handler that
 * interrupts the read is noted. */
static void wait_for_release(void) {
  char byte;
  for (;;) {
    ssize_t n = read(wake_pipes[self][0], &byte, 1);
    if (n == 1) {
      return;
    }
    if (n < 0 && errno == EINTR) {
      pthread_mutex_lock(&lock);
      if (interrupted_by < 0) {
        interrupted_by = self;
      }
      pthread_mutex_unlock(&lock);
      continue;
    }
    die("read");
  }
}

static void release(int thread) {
  char byte = 'r';
  if (write(wake_pipes[thread][1], &byte, 1) != 1) {
    die("write");
  }
}

/* Each worker sets its own mask (a thread's mask is its own), reports ready,
 * and waits; W1 forks the child first in chld-worker. */
static void* worker(void* arg) {
  self = (int)(long)arg;
  const char* mode = getenv("PST_MODE");
  int block = 0;
  if (!strcmp(mode, "chld-walk") || !strcmp(mode, "alrm-walk") || !strcmp(mode, "posix-walk")) {
    block = self == W1;
  } else if (!strcmp(mode, "chld-queued") || !strcmp(mode, "alrm-queued") ||
             !strcmp(mode, "alrm-ignored-blocked")) {
    block = 1;
  } else if (!strcmp(mode, "alrm-ignored-unblocked")) {
    block = self != W2;
  }
  if (block) {
    mask(SIG_BLOCK, signo);
  } else {
    mask(SIG_UNBLOCK, signo);
  }
  pthread_mutex_lock(&lock);
  ready++;
  pthread_cond_broadcast(&cond);
  while (self == W1 && !strcmp(mode, "chld-worker") && !fork_now) {
    pthread_cond_wait(&cond, &lock);
  }
  pthread_mutex_unlock(&lock);
  if (self == W1 && !strcmp(mode, "chld-worker")) {
    forked = fork_child();
  }
  wait_for_release();
  /* The queued modes: the worker named by the mode unblocks the signal
   * when M releases it the first time, and waits again. */
  if (block && ((self == W2 && (!strcmp(mode, "chld-queued") ||
                                !strcmp(mode, "alrm-ignored-blocked"))) ||
                (self == W1 && !strcmp(mode, "alrm-queued")))) {
    mask(SIG_UNBLOCK, signo);
    wait_for_release();
  }
  if (self == W2 && !strcmp(mode, "alrm-ignored-unblocked")) {
    wait_for_release();
  }
  return NULL;
}

static int shared_pending(void) {
  sigset_t set;
  if (sigpending(&set) != 0) {
    die("sigpending");
  }
  return sigismember(&set, signo);
}

/* The name of the thread whose handler ran, if one did, read without
 * blocking once the event is over. */
static const char* receiver(void) {
  char who;
  ssize_t n = read(report_pipe[0], &who, 1);
  if (n == 1 && who >= 0 && who < THREADS) {
    return NAMES[(int)who];
  }
  return "none";
}

int main(int argc, char** argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s MODE\n", argv[0]);
    return 2;
  }
  const char* mode = argv[1];
  if (setenv("PST_MODE", mode, 1) != 0) {
    die("setenv");
  }
  int chld = !strncmp(mode, "chld-", 5);
  int posix = !strcmp(mode, "posix-walk");
  int ignored = !strncmp(mode, "alrm-ignored-", 13);
  signo = chld ? SIGCHLD : posix ? SIGUSR1 : SIGALRM;
  for (int i = 0; i < THREADS; i++) {
    if (pipe(wake_pipes[i]) != 0) {
      die("pipe");
    }
  }
  if (pipe2(report_pipe, O_NONBLOCK) != 0) {
    die("pipe2");
  }
  set_action(signo, ignored ? SIG_IGN : on_signal);
  int main_blocks = strcmp(mode, "chld-creator") && strcmp(mode, "chld-worker") &&
      strcmp(mode, "alrm-leader");
  if (main_blocks) {
    mask(SIG_BLOCK, signo);
  }
  pthread_t threads[THREADS];
  for (long i = W1; i < THREADS; i++) {
    if (pthread_create(&threads[i], NULL, worker, (void*)i) != 0) {
      die("pthread_create");
    }
  }
  pthread_mutex_lock(&lock);
  while (ready != THREADS - 1) {
    pthread_cond_wait(&cond, &lock);
  }
  pthread_mutex_unlock(&lock);
  /* Let every worker reach its read(). */
  sleep_ms(10);
  timer_t timer;
  if (!strcmp(mode, "chld-worker")) {
    pthread_mutex_lock(&lock);
    fork_now = 1;
    pthread_cond_broadcast(&cond);
    pthread_mutex_unlock(&lock);
  } else if (chld) {
    forked = fork_child();
  } else if (posix) {
    struct sigevent event;
    memset(&event, 0, sizeof(event));
    event.sigev_notify = SIGEV_SIGNAL;
    event.sigev_signo = SIGUSR1;
    if (timer_create(CLOCK_MONOTONIC, &event, &timer) != 0) {
      die("timer_create");
    }
    struct itimerspec spec = {{0, 0}, {0, 20 * 1000000L}};
    if (timer_settime(timer, 0, &spec, NULL) != 0) {
      die("timer_settime");
    }
  } else {
    struct itimerval value = {{0, 0}, {0, 20000}};
    if (setitimer(ITIMER_REAL, &value, NULL) != 0) {
      die("setitimer");
    }
  }
  if (main_blocks) {
    /* M waits for the event with the signal blocked: a sleep it cannot end. */
    sleep_ms(60);
  } else {
    /* M waits for its own pipe in poll(), with a 200 ms bound so a run
     * where Linux gave the signal to another thread cannot hang; nothing
     * writes the pipe, so only a handler ends the wait early. */
    struct pollfd fd = {wake_pipes[M][0], POLLIN, 0};
    int n = poll(&fd, 1, 200);
    if (n < 0 && errno == EINTR) {
      pthread_mutex_lock(&lock);
      if (interrupted_by < 0) {
        interrupted_by = M;
      }
      pthread_mutex_unlock(&lock);
    } else if (n != 0) {
      die("poll");
    }
  }
  int pending = shared_pending();
  int queued = !strcmp(mode, "chld-queued") || !strcmp(mode, "alrm-queued") ||
      !strcmp(mode, "alrm-ignored-blocked") || !strcmp(mode, "alrm-ignored-unblocked");
  if (ignored) {
    set_action(signo, on_signal);
  }
  if (queued) {
    /* Release the worker that unblocks the signal, and give it time. */
    if (!strcmp(mode, "alrm-queued")) {
      release(W1);
    } else {
      release(W2);
    }
    sleep_ms(20);
  }
  const char* got = receiver();
  for (int i = W1; i < THREADS; i++) {
    release(i);
  }
  for (int i = W1; i < THREADS; i++) {
    pthread_join(threads[i], NULL);
  }
  if (forked > 0 && waitpid(forked, NULL, 0) != forked) {
    die("waitpid");
  }
  if (queued) {
    printf("pending=%d receiver=%s\n", pending, got);
  } else {
    printf("receiver=%s interrupted=%s\n", got,
           interrupted_by < 0 ? "none" : NAMES[interrupted_by]);
  }
  return 0;
}
