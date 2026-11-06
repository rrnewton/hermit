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
 * (kernel/signal.c) wakes the thread it names, if that thread wants it, and
 * otherwise the first thread that does, walking the thread list from the
 * process's curr_target and leaving curr_target at the thread it chose; with
 * none, the signal waits until a thread unblocks it. A thread wants it
 * (wants_signal) when it does not block it - a thread waiting in
 * sigtimedwait() for it does not, since do_sigtimedwait unblocks the waited
 * set - and it is running or has no signal pending. The thread named is the
 * child's parent thread for SIGCHLD (the thread that forked it,
 * do_notify_parent), and the leader for a timer.
 *
 * The process has the main thread M and two workers, W1 and W2, created in
 * that order. Each worker waits in read() on a pipe of its own, and M, when
 * it leaves the signal unblocked, in poll() on its own (bounded at 200 ms),
 * so no waiting thread has a signal pending and Linux would wake it; the
 * handler (no SA_RESTART) writes its thread's name to a report pipe, which M
 * reads once the event is over. Each mode prints one line, the same natively
 * and under Hermit.
 *
 * Every boundary the line depends on is reached causally, not by elapsed
 * time, so a native run has one outcome: a thread is known to wait in its
 * call when /proc shows it sleeping there (await_waiting: state S, or D in a
 * vfork wait, and the syscall number where /proc/<pid>/task/<tid>/syscall is
 * readable, which it is natively for the process's own threads; under Hermit
 * a stopped or parked thread reads as sleeping); the child exits only once
 * its creator thread waits; a timer is armed by a worker that does not decide
 * the outcome, only once M waits, and M waits for the arming, its expiry
 * and, in a queued mode, the signal's pending state (getitimer,
 * timer_gettime); a SIGCHLD has been generated once waitid(WNOWAIT) sees the
 * child's exit; a handler has run once its report is readable; a signal
 * taken by sigtimedwait() has no handler left to run; and a worker that
 * unblocks a pending signal has taken it before it acknowledges on the done
 * pipe, since the kernel delivers it on the return from pthread_sigmask:
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
 *   chld-curr-target, alrm-curr-target, posix-curr-target
 *                 the walk starts at curr_target, not at the first thread. M
 *                 blocks the signal, W1 blocks it at first and W2 does not. A
 *                 first, steering signal that only W2 leaves unblocked (an
 *                 ITIMER_REAL SIGALRM for chld-, a child's SIGCHLD for the
 *                 others) walks to W2 and leaves curr_target there; W1 then
 *                 unblocks the signal, and its walk starts at W2, which takes
 *                 it (a walk from M would reach W1 first).
 *                 "receiver=W2 interrupted=W2".
 *   chld-sigwait, alrm-sigwait, posix-sigwait
 *                 M, the thread named, blocks the signal but waits for it in
 *                 sigtimedwait() (bounded at 2 s); W1 leaves it unblocked and
 *                 W2 blocks it. M's wait unblocks it, so M wants it, is named
 *                 and takes it in its wait, and no handler runs.
 *                 "waited=1 receiver=none interrupted=none".
 *   chld-busy, alrm-busy, posix-busy
 *                 a thread with a signal pending that is not running does not
 *                 want another. W1 and W2 leave the signal unblocked and M
 *                 blocks it; W1 makes a CLONE_VFORK child (no exit signal)
 *                 that waits for M, and so sleeps in its vfork wait, which no
 *                 caught signal ends, and M sends W1 a caught SIGUSR2 there.
 *                 The walk from M passes busy W1 and reaches W2; once the
 *                 vfork child exits W1 takes its SIGUSR2.
 *                 "receiver=W2 interrupted=W2 busy=W1".
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

enum { M, W1, W2, THREADS };
static const char* const NAMES[THREADS] = {"M", "W1", "W2"};

/* What generates a mode's signal. */
enum source { CHILD_EXIT, ITIMER, POSIX_TIMER };

/* How M waits for it and what the mode prints. */
enum shape {
  /* M blocks the signal, or waits in poll() with it unblocked:
   * "receiver=R interrupted=I". */
  TAKEN,
  /* every thread blocks it until one worker unblocks it:
   * "pending=P receiver=R". */
  QUEUED,
  /* M waits in sigtimedwait(): "waited=W receiver=R interrupted=I". */
  SIGWAIT,
  /* W1 is busy in its vfork wait: "receiver=R interrupted=I busy=B". */
  BUSY,
};

struct mode {
  const char* name;
  enum source source;
  enum shape shape;
  /* Whether each thread blocks the signal; W1's block in a curr-target mode
   * is lifted after the steering signal. */
  int blocks[THREADS];
  /* The forking thread for a child exit; the worker that unblocks a queued
   * signal; unused otherwise. */
  int actor;
  /* Ignore the signal until it has been generated. */
  int ignored;
  /* Steer curr_target to W2 first with this source's signal (curr-target
   * modes). */
  int steer;
  enum source steer_source;
};

static const struct mode MODES[] = {
    {"chld-creator", CHILD_EXIT, TAKEN, {0, 0, 0}, M, 0, 0, 0},
    {"chld-walk", CHILD_EXIT, TAKEN, {1, 1, 0}, M, 0, 0, 0},
    {"chld-worker", CHILD_EXIT, TAKEN, {0, 0, 0}, W1, 0, 0, 0},
    {"chld-queued", CHILD_EXIT, QUEUED, {1, 1, 1}, W2, 0, 0, 0},
    {"alrm-leader", ITIMER, TAKEN, {0, 0, 0}, M, 0, 0, 0},
    {"alrm-walk", ITIMER, TAKEN, {1, 1, 0}, M, 0, 0, 0},
    {"alrm-queued", ITIMER, QUEUED, {1, 1, 1}, W1, 0, 0, 0},
    {"posix-walk", POSIX_TIMER, TAKEN, {1, 1, 0}, M, 0, 0, 0},
    {"alrm-ignored-blocked", ITIMER, QUEUED, {1, 1, 1}, W2, 1, 0, 0},
    {"alrm-ignored-unblocked", ITIMER, QUEUED, {1, 1, 0}, W2, 1, 0, 0},
    {"chld-curr-target", CHILD_EXIT, TAKEN, {1, 1, 0}, M, 0, 1, ITIMER},
    {"alrm-curr-target", ITIMER, TAKEN, {1, 1, 0}, M, 0, 1, CHILD_EXIT},
    {"posix-curr-target", POSIX_TIMER, TAKEN, {1, 1, 0}, M, 0, 1, CHILD_EXIT},
    {"chld-sigwait", CHILD_EXIT, SIGWAIT, {1, 0, 1}, M, 0, 0, 0},
    {"alrm-sigwait", ITIMER, SIGWAIT, {1, 0, 1}, M, 0, 0, 0},
    {"posix-sigwait", POSIX_TIMER, SIGWAIT, {1, 0, 1}, M, 0, 0, 0},
    {"chld-busy", CHILD_EXIT, BUSY, {1, 0, 0}, M, 0, 0, 0},
    {"alrm-busy", ITIMER, BUSY, {1, 0, 0}, M, 0, 0, 0},
    {"posix-busy", POSIX_TIMER, BUSY, {1, 0, 0}, M, 0, 0, 0},
};

static const struct mode* mode;
static __thread int self = M;
static pid_t tids[THREADS];
static int wake_pipes[THREADS][2];
static int report_pipe[2];
static int done_pipe[2];
static int vfork_gate[2];
static int interrupted_by = -1;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cond = PTHREAD_COND_INITIALIZER;
static int ready;
static int fork_now;
static pid_t forked;
static pid_t creator_tid;
static timer_t timer;
static char vfork_stack[64 * 1024] __attribute__((aligned(16)));

static void die(const char* what) {
  fprintf(stderr, "%s: %s\n", what, strerror(errno));
  _exit(99);
}

static int signal_of(enum source source) {
  return source == CHILD_EXIT ? SIGCHLD : source == ITIMER ? SIGALRM : SIGUSR1;
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

/* Whether task tid of process pid sleeps in syscall sysno (any when < 0),
 * as /proc shows it: state S (D in a vfork wait; t or T for a thread a
 * tracer holds), and the syscall number where that file is readable. */
static int waits_in(pid_t pid, pid_t tid, long sysno) {
  char path[64];
  char buf[512];
  snprintf(path, sizeof(path), "/proc/%d/task/%d/stat", (int)pid, (int)tid);
  FILE* file = fopen(path, "r");
  if (!file) {
    return 0;
  }
  char* line = fgets(buf, sizeof(buf), file);
  fclose(file);
  char* paren = line ? strrchr(buf, ')') : NULL;
  if (!paren || !strchr("SDtT", paren[2])) {
    return 0;
  }
  if (sysno < 0) {
    return 1;
  }
  snprintf(path, sizeof(path), "/proc/%d/task/%d/syscall", (int)pid, (int)tid);
  file = fopen(path, "r");
  if (!file) {
    return 1;
  }
  line = fgets(buf, sizeof(buf), file);
  fclose(file);
  return !line || strtol(buf, NULL, 10) == sysno;
}

/* Wait until task tid of process pid sleeps in syscall sysno (waits_in);
 * bounded at 10 s, after which the run goes on, so a misreading /proc can
 * slow a run but never hang it. */
static void await_waiting(pid_t pid, pid_t tid, long sysno) {
  for (int i = 0; i < 10000 && !waits_in(pid, tid, sysno); i++) {
    sleep_ms(1);
  }
}

/* Wait until fd is readable, bounded at 2 s. */
static void await_readable(int fd) {
  struct pollfd pfd = {fd, POLLIN, 0};
  while (poll(&pfd, 1, 2000) < 0 && errno == EINTR) {
  }
}

static pid_t fork_child(void) {
  creator_tid = gettid();
  pid_t child = fork();
  if (child < 0) {
    die("fork");
  }
  if (child == 0) {
    /* Exit only once the thread that forked it waits. */
    await_waiting(getppid(), creator_tid, -1);
    _exit(0);
  }
  return child;
}

/* Arm a one-shot timer (20 ms), from whichever thread calls it: ITIMER_REAL
 * and a SIGEV_SIGNAL POSIX timer belong to the process. */
static void arm_timer(enum source source) {
  if (source == POSIX_TIMER) {
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
}

/* Wait until a one-shot timer has expired, bounded at 10 s. */
static void await_expiry(enum source source) {
  for (int i = 0; i < 10000; i++) {
    if (source == POSIX_TIMER) {
      struct itimerspec spec;
      if (timer_gettime(timer, &spec) != 0) {
        die("timer_gettime");
      }
      if (spec.it_value.tv_sec == 0 && spec.it_value.tv_nsec == 0) {
        return;
      }
    } else {
      struct itimerval value;
      if (getitimer(ITIMER_REAL, &value) != 0) {
        die("getitimer");
      }
      if (value.it_value.tv_sec == 0 && value.it_value.tv_usec == 0) {
        return;
      }
    }
    sleep_ms(1);
  }
}

static int shared_pending(int sig) {
  sigset_t set;
  if (sigpending(&set) != 0) {
    die("sigpending");
  }
  return sigismember(&set, sig);
}

/* Wait until sig's pending state is `want`, bounded at `ms`. */
static void await_pending(int sig, int want, int ms) {
  for (int i = 0; i < ms && shared_pending(sig) != want; i++) {
    sleep_ms(1);
  }
}

/* Wait in read() on this thread's own pipe for a command byte; a handler
 * that interrupts the read is noted. */
static char wait_for_command(void) {
  char byte;
  for (;;) {
    ssize_t n = read(wake_pipes[self][0], &byte, 1);
    if (n == 1) {
      return byte;
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

static void command(int thread, char byte) {
  if (write(wake_pipes[thread][1], &byte, 1) != 1) {
    die("write");
  }
}

static void acknowledge(void) {
  char byte = 'd';
  if (write(done_pipe[1], &byte, 1) != 1) {
    die("write");
  }
}

static void await_acknowledgement(void) {
  char byte;
  if (read(done_pipe[0], &byte, 1) != 1) {
    die("read");
  }
}

/* A system call made without the C library, for the CLONE_VFORK child. */
static long raw_syscall3(long number, long a, long b, long c) {
  long result;
  __asm__ volatile("syscall"
                   : "=a"(result)
                   : "a"(number), "D"(a), "S"(b), "d"(c)
                   : "rcx", "r11", "memory");
  return result;
}

/* A CLONE_VFORK child shares its parent's memory, so it touches nothing of
 * the C library's (not even a lazily bound symbol): it acknowledges, waits
 * for M's byte, and exits. */
static int vfork_child(void* arg) {
  (void)arg;
  char byte = 'd';
  raw_syscall3(SYS_write, done_pipe[1], (long)&byte, 1);
  raw_syscall3(SYS_read, vfork_gate[0], (long)&byte, 1);
  raw_syscall3(SYS_exit, 0, 0, 0);
  return 0;
}

/* The syscall M waits in while a worker arms the timer, or -1 when M blocks
 * the signal and need not be waiting. */
static long main_wait_syscall(void) {
  if (mode->shape == SIGWAIT) {
    return SYS_rt_sigtimedwait;
  }
  return mode->blocks[M] ? -1 : SYS_poll;
}

/* Each worker sets its own mask (a thread's mask is its own), reports ready,
 * and waits for commands:
 *   'a' arm the mode's timer once M waits (main_wait_syscall), acknowledge;
 *   's' arm the steering ITIMER_REAL, acknowledge;
 *   'n' acknowledge;
 *   'u' unblock the signal and acknowledge (a pending copy is taken before
 *       pthread_sigmask returns);
 *   'v' make the CLONE_VFORK child, which acknowledges, and wait for it;
 *       acknowledge once it exited and any signal pending was taken;
 *   'q' end.
 * W1 forks the child first in chld-worker. */
static void* worker(void* arg) {
  self = (int)(long)arg;
  tids[self] = gettid();
  int signo = signal_of(mode->source);
  mask(mode->blocks[self] ? SIG_BLOCK : SIG_UNBLOCK, signo);
  if (mode->steer) {
    mask(self == W2 ? SIG_UNBLOCK : SIG_BLOCK, signal_of(mode->steer_source));
  }
  pthread_mutex_lock(&lock);
  ready++;
  pthread_cond_broadcast(&cond);
  int forks = mode->source == CHILD_EXIT && mode->shape == TAKEN && mode->actor == self;
  while (forks && !fork_now) {
    pthread_cond_wait(&cond, &lock);
  }
  pthread_mutex_unlock(&lock);
  if (forks) {
    forked = fork_child();
  }
  for (;;) {
    char byte = wait_for_command();
    switch (byte) {
      case 'q':
        return NULL;
      case 'a': {
        long sysno = main_wait_syscall();
        if (sysno >= 0) {
          await_waiting(getpid(), tids[M], sysno);
        }
        arm_timer(mode->source);
        acknowledge();
        break;
      }
      case 's':
        arm_timer(ITIMER);
        acknowledge();
        break;
      case 'n':
        acknowledge();
        break;
      case 'u':
        mask(SIG_UNBLOCK, signo);
        acknowledge();
        break;
      case 'v': {
        pid_t child = clone(vfork_child, vfork_stack + sizeof(vfork_stack), CLONE_VM | CLONE_VFORK,
                            NULL);
        if (child < 0) {
          die("clone");
        }
        /* It has no exit signal, so it is left to go with the process:
         * waiting for it takes __WCLONE, which Hermit's waitid emulation
         * does not accept. */
        acknowledge();
        break;
      }
      default:
        fprintf(stderr, "unknown command %c\n", byte);
        _exit(97);
    }
  }
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

/* Steer curr_target to W2 with the steering signal, which only W2 leaves
 * unblocked, then let W1 unblock the mode's signal. */
static void steer(void) {
  if (mode->steer_source == CHILD_EXIT) {
    forked = fork_child();
  } else {
    command(W1, 's');
    await_acknowledgement();
  }
  await_readable(report_pipe[0]);
  const char* steered = receiver();
  if (strcmp(steered, "W2")) {
    printf("steered=%s\n", steered);
    exit(1);
  }
  if (forked > 0) {
    if (waitpid(forked, NULL, 0) != forked) {
      die("waitpid");
    }
    forked = 0;
  }
  /* W2 has noted its interrupted read once it acknowledges a command. */
  command(W2, 'n');
  await_acknowledgement();
  pthread_mutex_lock(&lock);
  interrupted_by = -1;
  pthread_mutex_unlock(&lock);
  command(W1, 'u');
  await_acknowledgement();
  await_waiting(getpid(), tids[W1], SYS_read);
  await_waiting(getpid(), tids[W2], SYS_read);
}

int main(int argc, char** argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s MODE\n", argv[0]);
    return 2;
  }
  for (size_t i = 0; i < sizeof(MODES) / sizeof(MODES[0]); i++) {
    if (!strcmp(argv[1], MODES[i].name)) {
      mode = &MODES[i];
    }
  }
  if (!mode) {
    fprintf(stderr, "unknown mode %s\n", argv[1]);
    return 2;
  }
  tids[M] = gettid();
  int signo = signal_of(mode->source);
  for (int i = 0; i < THREADS; i++) {
    if (pipe(wake_pipes[i]) != 0) {
      die("pipe");
    }
  }
  if (pipe2(report_pipe, O_NONBLOCK) != 0 || pipe(done_pipe) != 0 || pipe(vfork_gate) != 0) {
    die("pipe2");
  }
  set_action(signo, mode->ignored ? SIG_IGN : on_signal);
  if (mode->steer) {
    set_action(signal_of(mode->steer_source), on_signal);
    mask(SIG_BLOCK, signal_of(mode->steer_source));
  }
  if (mode->shape == BUSY) {
    set_action(SIGUSR2, on_signal);
  }
  if (mode->blocks[M]) {
    mask(SIG_BLOCK, signo);
  }
  if (mode->source == POSIX_TIMER) {
    struct sigevent event;
    memset(&event, 0, sizeof(event));
    event.sigev_notify = SIGEV_SIGNAL;
    event.sigev_signo = SIGUSR1;
    if (timer_create(CLOCK_MONOTONIC, &event, &timer) != 0) {
      die("timer_create");
    }
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
  /* Every worker waits in its read() (W1 in chld-worker once it forked). */
  int forker = mode->source == CHILD_EXIT && mode->shape == TAKEN ? mode->actor : M;
  if (forker == W1) {
    await_waiting(getpid(), tids[W2], SYS_read);
    pthread_mutex_lock(&lock);
    fork_now = 1;
    pthread_cond_broadcast(&cond);
    pthread_mutex_unlock(&lock);
  } else {
    await_waiting(getpid(), tids[W1], SYS_read);
    await_waiting(getpid(), tids[W2], SYS_read);
  }
  if (mode->steer) {
    steer();
  }
  if (mode->shape == BUSY) {
    /* W1 sleeps in its vfork wait, and SIGUSR2 is pending for it there. */
    command(W1, 'v');
    await_acknowledgement();
    await_waiting(getpid(), tids[W1], SYS_clone);
    if (syscall(SYS_tgkill, getpid(), tids[W1], SIGUSR2) != 0) {
      die("tgkill");
    }
  }
  /* The event: M forks the child (unless W1 did), or a worker arms the
   * timer; W2 does in a busy mode, where W1 is in its vfork wait, and W1
   * otherwise. */
  if (mode->source == CHILD_EXIT) {
    if (forker == M) {
      forked = fork_child();
    }
  } else {
    command(mode->shape == BUSY ? W2 : W1, 'a');
  }
  int waited = 0;
  if (mode->shape == SIGWAIT) {
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, signo);
    struct timespec bound = {2, 0};
    siginfo_t info;
    int got;
    while ((got = sigtimedwait(&set, &info, &bound)) < 0 && errno == EINTR) {
    }
    waited = got == signo;
  } else if (mode->blocks[M]) {
    /* M waits, with the signal blocked, until the event has happened. */
    if (mode->source == CHILD_EXIT) {
      siginfo_t info;
      while (waitid(P_PID, forked, &info, WEXITED | WNOWAIT) != 0) {
        if (errno != EINTR) {
          die("waitid");
        }
      }
    } else {
      await_acknowledgement();
      await_expiry(mode->source);
      /* A timer reads as expired when it is due, which can be before its
       * signal has been generated (seen on Linux 7.1), so a queued mode also
       * waits for the signal itself: with every thread blocking it stays
       * pending; in alrm-ignored-unblocked W2 takes it and discards it, so M
       * waits for it to show (missing it when W2 was quicker costs the 200 ms
       * bound) and then to go. */
      if (mode->shape == QUEUED && !mode->blocks[W2]) {
        await_pending(signo, 1, 200);
        await_pending(signo, 0, 200);
      } else if (mode->shape == QUEUED) {
        await_pending(signo, 1, 10000);
      }
    }
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
  if (mode->source != CHILD_EXIT && (mode->shape == SIGWAIT || !mode->blocks[M])) {
    await_acknowledgement();
  }
  if (mode->shape == TAKEN || mode->shape == BUSY || (mode->shape == SIGWAIT && !waited)) {
    /* The thread that took it has run its handler. */
    await_readable(report_pipe[0]);
  }
  int pending = shared_pending(signo);
  if (mode->ignored) {
    set_action(signo, on_signal);
  }
  if (mode->shape == QUEUED) {
    /* The worker named by the mode unblocks the signal and acknowledges once
     * it took any pending copy. */
    command(mode->actor, 'u');
    await_acknowledgement();
  }
  const char* got = receiver();
  const char* busy = "none";
  if (mode->shape == BUSY) {
    /* Release the vfork child; W1 acknowledges once it took its SIGUSR2. */
    char byte = 'x';
    if (write(vfork_gate[1], &byte, 1) != 1) {
      die("write");
    }
    await_acknowledgement();
    busy = receiver();
  }
  for (int i = W1; i < THREADS; i++) {
    command(i, 'q');
  }
  for (int i = W1; i < THREADS; i++) {
    pthread_join(threads[i], NULL);
  }
  if (forked > 0 && waitpid(forked, NULL, 0) != forked) {
    die("waitpid");
  }
  const char* interrupted = interrupted_by < 0 ? "none" : NAMES[interrupted_by];
  switch (mode->shape) {
    case QUEUED:
      printf("pending=%d receiver=%s\n", pending, got);
      break;
    case SIGWAIT:
      printf("waited=%d receiver=%s interrupted=%s\n", waited, got, interrupted);
      break;
    case BUSY:
      printf("receiver=%s interrupted=%s busy=%s\n", got, interrupted, busy);
      break;
    case TAKEN:
      printf("receiver=%s interrupted=%s\n", got, interrupted);
      break;
  }
  return 0;
}
