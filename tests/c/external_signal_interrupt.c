/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Guest for hermit-cli/tests/external_signal_interrupt.rs
 * (https://github.com/rrnewton/hermit/issues/3146).
 *
 * usage: external_signal_interrupt <futex|sem|select|rawselect|poll|epoll|wait4|waitid>
 *            <external|process|thread|timer|exit> [restart] [timed] [warm]
 *            [ignored|blocked|winch|tstp|ign2caught|caught2ign|chldlate|chldign|
 *             chldkill|chldthrexit|chldpend|stealgrp|stealkill|stealthrexit|
 *             forkgrp|forkkill|forkthrexit|spin|spinkill|spinthrexit|usr2|
 *             extchld|extchldreaped|extchldlive]
 *        external_signal_interrupt sigsuspend creator
 *        external_signal_interrupt <poll|ppoll|epoll|epollpwait|epollinval|epollbadf|sigtimedwaitfault> racing
 *
 * `select` is glibc's, which issues pselect6 with no mask; `rawselect` is the
 * select system call itself. `sem` is glibc's sem_timedwait, a FUTEX_WAIT_BITSET
 * with an absolute CLOCK_REALTIME deadline 10 s away (300 ms for a
 * must-not-wake option).
 *
 * The main thread installs a SIGUSR1 and SIGALRM handler (SA_RESTART only
 * with `restart`), prints READY, and blocks in the chosen call until the
 * signal arrives:
 *   external: from outside Hermit; the harness signals this process.
 *   process:  from a forked guest process that stays alive after kill().
 *             For wait4/waitid it is the waited child, blocked on a pipe;
 *             with `restart` it instead exits 100 ms after its kill(), so a
 *             wait that Linux restarts under SA_RESTART returns the child
 *             (printed as ret=child for wait4) near 200 ms, and a HANDLED_AT
 *             line after ELAPSED reports when the handler first ran (-1 if
 *             it never did): near 100 ms when the signal interrupted the wait.
 *   thread:   from a sibling thread with pthread_kill(). The thread waits,
 *             boundedly, for the handler to run (exiting 93 with
 *             WAKER_TIMEOUT if it never does), then sets the futex word and
 *             wakes the futex, so an SA_RESTART futex wait that Linux
 *             restarts still returns.
 *   timer:    SIGALRM from a 100 ms ITIMER_REAL, which Hermit's scheduler
 *             delivers itself.
 *   exit:     SIGCHLD, caught from the start, from a forked child that exits
 *             after 100 ms. Hermit's scheduler delivers the child-exit SIGCHLD;
 *             the kernel also posts its own at a host-timed moment.
 * `timed` gives the futex wait a relative timeout far beyond the signal.
 *
 * The last three options send a signal that must NOT end the wait: SIGUSR1
 * with SIG_IGN (`ignored`), SIGUSR1 blocked by the waiter (`blocked`), or
 * SIGWINCH, which is ignored by default (`winch`). The call then has a 300 ms
 * timeout, the sender does not wake the futex, and an ELAPSED line reports the
 * CLOCK_MONOTONIC time the call took, so the wait must run to its deadline.
 *
 * `tstp` is a fourth such option, with one exception. The guest first forks,
 * and the child runs the whole test after setsid(), which makes it the only
 * member of a new session and process group. That group is orphaned: no
 * member has a parent in another group of the same session. The sender then
 * sends SIGTSTP, left at SIG_DFL. Linux discards a SIG_DFL SIGTSTP, SIGTTIN or
 * SIGTTOU in an orphaned process group instead of stopping the process, and
 * restarts a call the signal interrupted. poll, select, a timed futex and
 * sem_timedwait restart with their original deadline, so they too return at
 * 300 ms; epoll_wait is the exception and returns EINTR when the signal
 * arrives. The parent prints nothing and exits with the child's status.
 *
 * The next seven options change a disposition, or queue a signal, while the
 * waiter is already parked; they need the `thread` sender. The sibling thread blocks SIGUSR1 and
 * SIGCHLD, so only the waiter can take the signal, and after 100 ms it:
 *   ign2caught: installs the SIGUSR1 handler over SIG_IGN, then pthread_kill.
 *   caught2ign: sets SIGUSR1 to SIG_IGN over the handler, then pthread_kill.
 *   chldlate:   installs a SIGCHLD handler over SIG_DFL, then forks a child
 *               that exits at once.
 *   chldign:    resets a caught SIGCHLD to SIG_DFL (ignored by default), then
 *               forks a child that exits at once.
 *   chldkill:   as chldlate, but the child dies by sending itself SIGKILL.
 *   chldthrexit: as chldlate, but the child's only thread calls the exit
 *               system call rather than exit_group.
 *   chldpend:   leaves SIGCHLD at its default disposition, forks a child that
 *               exits at once and reaps it, so that child's SIGCHLD is pending
 *               on the process's shared queue (a traced process queues even a
 *               signal it ignores), where only the waiter can take it; then
 *               pthread_kill(SIGUSR1). The waiter has a caught signal and a
 *               default-ignored SIGCHLD pending together.
 * chldkill and chldthrexit end the child without exit_group, so Hermit's
 * scheduler sends no child-exit SIGCHLD of its own: the only SIGCHLD is the
 * kernel's, which the scheduler makes eligible at the child's logical death.
 * ign2caught, chldlate, chldkill, chldthrexit and chldpend must end the wait
 * with EINTR near 100 ms; the sibling wakes the futex (and writes the pipe)
 * only after the handler ran.
 * caught2ign and chldign must not end it: a `timed` wait then returns at its
 * original 300 ms deadline, and an untimed one is ended by the sibling's
 * FUTEX_WAKE (or pipe write) 200 ms after the signal.
 *
 * `exit` and these seven options also print the ELAPSED line.
 *
 * The remaining options add a sibling thread that does NOT block SIGCHLD, so
 * the kernel may deliver a child's SIGCHLD to it instead of to the waiter.
 * Linux's complete_signal() offers a process-directed signal first to the
 * thread it names, which for a child's SIGCHLD is the thread that forked the
 * child, and takes it if that thread does not block the signal and is running
 * or has no signal pending; otherwise it tries the other threads. The SIGCHLD
 * handler is installed from the start, and a HANDLER line reports how many
 * times it ran on the main thread and on the sibling. The child dies by
 * exit_group (`...grp`, or `spin`), by sending itself SIGKILL (`...kill`), or
 * by its only thread calling the exit system call (`...threxit`).
 *   steal*:  `thread` sender. After 100 ms the sibling forks a child that dies
 *            at once and stays runnable, calling sched_yield (which gives up
 *            the CPU but does not block), until the handler has run. The
 *            forking sibling takes the SIGCHLD, so
 *            the waiter keeps waiting: a `timed` wait (300 ms) returns
 *            ETIMEDOUT at its original deadline, and an untimed one is ended
 *            by the sibling's FUTEX_WAKE 100 ms after the handler ran.
 *   fork*:   `thread` sender and `timed`. The sibling forks a child that dies
 *            after 100 ms, then parks in its own 300 ms FUTEX_WAIT. The forking
 *            sibling takes the SIGCHLD: its wait ends with EINTR near 100 ms,
 *            reported on a SIBLING line, and the main thread's 300 ms wait
 *            runs to ETIMEDOUT.
 *   spin*:   `exit` sender. The main thread forks the child, which dies after
 *            100 ms, and starts a sibling that sleeps 50 ms, so the main thread
 *            is already waiting, and then spins in user code, with no system
 *            calls, until the handler has run. The forking main thread is
 *            waiting, so it takes the SIGCHLD and its wait ends with EINTR.
 *            For an untimed wait the sibling then wakes the futex, so a wait
 *            that lost the signal to the sibling returns 0 instead of hanging.
 *
 * `warm` issues the waiting call's own system-call instruction once before the
 * wait, so a backend that patches a call site on its first execution (LiteInst)
 * runs the wait through the patched site: syscall(SYS_gettid) for `futex`,
 * `rawselect`, `wait4` and `waitid`, which then share glibc's syscall()
 * instruction (with `warm`, wait4 and waitid are issued through syscall()
 * rather than their glibc wrappers), and a 10 ms sem_timedwait that must time
 * out for `sem`.
 *
 * `usr2` needs the `external` sender, which then sends SIGUSR1 and SIGUSR2
 * back to back: SIGUSR2 is caught too, and after the RESULT line the guest
 * waits, boundedly, for both handlers and prints how often each ran on a
 * HANDLED line (`HANDLED usr1=1 usr2=1` when neither signal was lost).
 *
 * `extchld`, `extchldreaped` and `extchldlive` need the `external` sender and
 * the `select` or `rawselect` call, whose wait is then unbounded. The guest
 * catches SIGCHLD (flags 0), and the harness sends SIGCHLD rather than SIGUSR1
 * with kill(2), so its siginfo code is SI_USER, not a child event.
 *   extchld:       the process never has a child.
 *   extchldreaped: before READY the guest forks a child that exits at once and
 *                  reaps it, sets SIGCHLD to SIG_DFL, which discards that
 *                  child's SIGCHLD if it is still pending, and only then
 *                  installs the handler, so the handler cannot run for it.
 *   extchldlive:   before READY the guest forks a child that stays blocked
 *                  reading a pipe until the guest closes the pipe after the
 *                  RESULT line. The child first changes its copy of argv[0],
 *                  so the harness, which looks the guest up by argv[0],
 *                  signals the parent alone.
 * Linux ends the wait with EINTR as soon as the signal arrives, with or
 * without a child, and the handler has run once by the RESULT line.
 *
 * `sigsuspend creator` is a mode of its own. The main thread installs a
 * SIGCHLD handler (flags 0), blocks SIGCHLD, prints READY and waits in
 * pthread_join for a second thread, the creator, which inherits that mask. Six times, the creator
 * forks a child that exits 50 ms later with the trial number as its status,
 * and sleeps in sigsuspend with SIGCHLD unblocked. Linux sends the child's
 * SIGCHLD to the thread that forked it when that thread does not block it, so
 * the handler runs on the creator and sigsuspend returns -1 with EINTR once
 * the child has exited. The creator then reaps the child and sets SIGCHLD to
 * SIG_IGN and back to the handler, which discards any second SIGCHLD left
 * pending for the same exit. Each trial prints a TRIAL line (whether the wait
 * ended after the child's exit, the call's result, how often the handler ran,
 * whether it ran on the creator, and the reaped status), then one RESULT line
 * counts the trials that matched Linux.
 *
 * The `racing` sender is a mode of its own, for a call that does not wait: a
 * readable pipe for poll, ppoll (no mask), epoll and epollpwait (epoll_wait and
 * epoll_pwait with no mask, on an epoll set holding that pipe), epoll_wait with
 * maxevents 0 for epollinval, epoll_wait on a descriptor that is not open
 * for epollbadf, and rt_sigtimedwait with a set pointer that cannot be read for
 * sigtimedwaitfault; every call has a 5 s timeout. Six times, the main thread
 * starts a thread that waits for a flag, yields the trial number of times, and
 * sends SIGUSR1 (caught, flags 0) to the main thread with tgkill; the main
 * thread sets the flag and makes the call. Linux reports a ready descriptor or
 * an argument error before it looks at pending signals (do_poll, ep_poll,
 * do_epoll_wait, and rt_sigtimedwait copies its set first), so whenever the
 * signal arrives the call returns 1 with the pipe's event, or -1 with EINVAL,
 * EBADF or EFAULT, and the handler runs once by the time the thread is joined.
 * Each trial prints a TRIAL line, then one RESULT line counts the trials that
 * matched Linux.
 *
 * Output is one deterministic RESULT line after the call returns, followed
 * by DONE once every helper has been reaped. A kernel-internal errno, which has
 * no name, prints as UNNAMED(<number>). */
#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <semaphore.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/resource.h>
#include <sys/select.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t handled = 0;
/* Handler runs on the main thread and on any other thread. */
static volatile sig_atomic_t handled_main = 0;
static volatile sig_atomic_t handled_sibling = 0;
/* The `usr2` option's second caught signal. */
static volatile sig_atomic_t handled_usr2 = 0;
/* With `restart`, wait4 and waitid stamp the handler's first run, so a wait
 * that Linux restarts (handler near 100 ms, wait ending near 200 ms) can be
 * told apart from one the signal never interrupted (handler after the wait). */
static volatile sig_atomic_t stamp_handler = 0;
static struct timespec handled_at;
static uint32_t futex_word = 0;
static pthread_t main_thread;
static int sent_signal = SIGUSR1;
static int quiet = 0;
/* A disposition change while the waiter is parked (see the usage comment). */
enum flip { FLIP_NONE, IGN2CAUGHT, CAUGHT2IGN, CHLDLATE, CHLDIGN, CHLDKILL, CHLDTHREXIT, CHLDPEND };
static enum flip flip = FLIP_NONE;
static int flip_timed = 0;
/* A sibling that does not block SIGCHLD (see the usage comment). */
enum role { ROLE_NONE, ROLE_STEAL, ROLE_FORK, ROLE_SPIN };
static enum role role = ROLE_NONE;
/* How the child of the `exit` sender or of a role dies. */
enum death { DEATH_GROUP, DEATH_KILL, DEATH_THREXIT };
static enum death death = DEATH_GROUP;
/* A caught SIGCHLD sent from outside Hermit (see the usage comment). */
enum extchld { EXTCHLD_NONE, EXTCHLD_CHILDLESS, EXTCHLD_REAPED, EXTCHLD_LIVE };
/* The fork role's own wait on sibling_word, as the main thread reports it. */
static uint32_t sibling_word = 0;
static long sibling_ret = 0;
static int sibling_err = 0;
static long sibling_ms = -1;
static volatile unsigned long spin_sink = 0;
/* Write end of the pipe a readiness call waits on. */
static int ready_write_fd = -1;

#define QUIET_TIMEOUT_MS 300
#define WAKER_POLLS 5000
/* Bound on the steal role's sched_yield calls while it waits for the handler. */
#define STEAL_SYSCALLS 200000L
/* Bound on the spin role's user-code iterations while it waits for the handler. */
#define SPIN_ITERATIONS 20000000000UL

static void on_usr1(int sig) {
  (void)sig;
  handled += 1;
  /* clock_gettime is async-signal-safe. */
  if (stamp_handler && handled == 1) clock_gettime(CLOCK_MONOTONIC, &handled_at);
  /* pthread_self() reads the thread pointer; it makes no system call. */
  if (pthread_equal(pthread_self(), main_thread)) handled_main += 1;
  else handled_sibling += 1;
}

static void on_usr2(int sig) {
  (void)sig;
  handled_usr2 += 1;
}

static void say(const char *s) {
  size_t left = strlen(s);
  while (left > 0) {
    ssize_t n = write(1, s, left);
    if (n < 0 && errno == EINTR) continue;
    if (n <= 0) _exit(90);
    s += n;
    left -= (size_t)n;
  }
}

static void sleep_ms(long ms) {
  struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
  while (nanosleep(&ts, &ts) != 0 && errno == EINTR) {
  }
}

static void set_handler(int sig, void (*handler)(int)) {
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_handler = handler;
  sigemptyset(&sa.sa_mask);
  if (sigaction(sig, &sa, NULL) != 0) _exit(94);
}

/* End the waiter's call the way a waker would: set and wake the futex word and
 * make the readiness pipe readable. */
static void wake_waiter(void) {
  __atomic_store_n(&futex_word, 1, __ATOMIC_SEQ_CST);
  syscall(SYS_futex, &futex_word, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0);
  if (write(ready_write_fd, "x", 1) != 1) _exit(95);
}

static void wait_for_handler(void) {
  int polls = 0;
  while (handled == 0) {
    if (++polls > WAKER_POLLS) {
      say("WAKER_TIMEOUT\n");
      _exit(93);
    }
    sleep_ms(1);
  }
}

/* Whether the flip makes the signal caught, so it must end the wait. */
static int flip_catches(void) {
  return flip == IGN2CAUGHT || flip == CHLDLATE || flip == CHLDKILL || flip == CHLDTHREXIT ||
         flip == CHLDPEND;
}

/* The child of the SIGCHLD options, which never returns. */
static void flip_child(void) {
  if (flip == CHLDKILL) syscall(SYS_kill, syscall(SYS_getpid), SIGKILL);
  if (flip == CHLDTHREXIT) syscall(SYS_exit, 0);
  _exit(0);
}

/* The sibling for the disposition-change options. */
static void flip_sender(void) {
  pid_t child = -1;
  switch (flip) {
    case IGN2CAUGHT:
      set_handler(SIGUSR1, on_usr1);
      if (pthread_kill(main_thread, SIGUSR1) != 0) _exit(91);
      break;
    case CAUGHT2IGN:
      set_handler(SIGUSR1, SIG_IGN);
      if (pthread_kill(main_thread, SIGUSR1) != 0) _exit(91);
      break;
    case CHLDLATE:
    case CHLDIGN:
    case CHLDKILL:
    case CHLDTHREXIT:
      set_handler(SIGCHLD, flip == CHLDIGN ? SIG_DFL : on_usr1);
      child = fork();
      if (child < 0) _exit(96);
      if (child == 0) flip_child();
      break;
    case CHLDPEND: {
      /* Reaping the child orders its SIGCHLD before the pthread_kill: the
       * kernel queues the parent's SIGCHLD for a traced child before the
       * tracer can report the exit that lets waitpid() return. */
      pid_t pending_child = fork();
      if (pending_child < 0) _exit(96);
      if (pending_child == 0) _exit(0);
      int st;
      while (waitpid(pending_child, &st, 0) < 0 && errno == EINTR) {
      }
      if (pthread_kill(main_thread, SIGUSR1) != 0) _exit(91);
      break;
    }
    case FLIP_NONE:
      _exit(97);
  }
  if (flip_catches()) {
    /* Wake only after the handler ran, so a wait the signal did not end is
     * not rescued by this wake. */
    wait_for_handler();
    sleep_ms(100);
    wake_waiter();
  } else {
    sleep_ms(200);
    if (!flip_timed) wake_waiter();
  }
  if (child > 0) {
    int st;
    while (waitpid(child, &st, 0) < 0 && errno == EINTR) {
    }
  }
}

static void reap(pid_t child) {
  int st;
  while (waitpid(child, &st, 0) < 0 && errno == EINTR) {
  }
}

/* A child of the `exit` sender or of a role, which dies as `death` says and
 * never returns. */
static void die(void) {
  if (death == DEATH_KILL) syscall(SYS_kill, syscall(SYS_getpid), SIGKILL);
  if (death == DEATH_THREXIT) syscall(SYS_exit, 0);
  _exit(0);
}

static long ms_between(const struct timespec *start, const struct timespec *end) {
  return (end->tv_sec - start->tv_sec) * 1000L + (end->tv_nsec - start->tv_nsec) / 1000000L;
}

static void add_ms(struct timespec *ts, long ms) {
  ts->tv_sec += ms / 1000;
  ts->tv_nsec += (ms % 1000) * 1000000L;
  if (ts->tv_nsec >= 1000000000L) {
    ts->tv_sec += 1;
    ts->tv_nsec -= 1000000000L;
  }
}

/* The errno's name, or UNNAMED(<number>) for a kernel-internal errno. */
static const char *errno_name(int err) {
  static char unnamed[4][32];
  static int next = 0;
  const char *name = strerrorname_np(err);
  if (name != NULL) return name;
  char *slot = unnamed[next++ % 4];
  snprintf(slot, sizeof unnamed[0], "UNNAMED(%d)", err);
  return slot;
}

/* The steal role, 100 ms after the sibling started. */
static void steal_sibling(void) {
  pid_t child = fork();
  if (child < 0) _exit(96);
  if (child == 0) die();
  /* Stay runnable, giving up the CPU often, until some thread's handler ran. */
  for (long calls = 0; handled == 0; calls++) {
    if (calls > STEAL_SYSCALLS) {
      say("STEAL_TIMEOUT\n");
      _exit(93);
    }
    sched_yield();
  }
  if (!flip_timed) {
    sleep_ms(100);
    wake_waiter();
  }
  reap(child);
}

/* The fork role: fork a child that dies after 100 ms, then wait on its own word. */
static void fork_sibling(void) {
  struct timespec start, end;
  clock_gettime(CLOCK_MONOTONIC, &start);
  pid_t child = fork();
  if (child < 0) _exit(96);
  if (child == 0) {
    sleep_ms(100);
    die();
  }
  struct timespec timeout = {0, QUIET_TIMEOUT_MS * 1000000L};
  errno = 0;
  sibling_ret = syscall(SYS_futex, &sibling_word, FUTEX_WAIT_PRIVATE, 0, &timeout, NULL, 0);
  sibling_err = errno;
  clock_gettime(CLOCK_MONOTONIC, &end);
  sibling_ms = ms_between(&start, &end);
  reap(child);
}

/* The spin role: after a 50 ms sleep, user code only, with no system calls,
 * until some thread's handler ran.
 *
 * The PAUSE makes each iteration slower without adding a conditional branch.
 * Hermit preempts this thread by counting retired conditional branches, and
 * the counter's interrupt arrives some branches late (skid): the denser the
 * branches, the later it lands. Without the PAUSE, Hermit refused about 1 run
 * in 100 for a skid overshoot (https://github.com/rrnewton/hermit/issues/1845).
 * The branches per iteration, and so the schedule and the output, are
 * unchanged. */
static void *spin_sibling(void *arg) {
  (void)arg;
  /* Let the main thread park in its wait before spinning. */
  sleep_ms(50);
  unsigned long x = 1;
  for (unsigned long i = 0; handled == 0 && i < SPIN_ITERATIONS; i++) {
    __builtin_ia32_pause();
    x = x * 6364136223846793005UL + 1442695040888963407UL;
  }
  spin_sink = x;
  if (!flip_timed) wake_waiter();
  return NULL;
}

static void *thread_sender(void *arg) {
  (void)arg;
  sigset_t block;
  sigemptyset(&block);
  sigaddset(&block, SIGUSR1);
  if (flip != FLIP_NONE) sigaddset(&block, SIGCHLD);
  pthread_sigmask(SIG_BLOCK, &block, NULL);
  if (role == ROLE_FORK) {
    fork_sibling();
    return NULL;
  }
  sleep_ms(100);
  if (role == ROLE_STEAL) {
    steal_sibling();
    return NULL;
  }
  if (flip != FLIP_NONE) {
    flip_sender();
    return NULL;
  }
  if (pthread_kill(main_thread, sent_signal) != 0) _exit(91);
  if (quiet) return NULL;
  /* Wake only after the handler ran, so a wait that the signal did not
   * interrupt cannot be rescued by this wake. */
  int polls = 0;
  while (handled == 0) {
    if (++polls > WAKER_POLLS) {
      say("WAKER_TIMEOUT\n");
      _exit(93);
    }
    sleep_ms(1);
  }
  sleep_ms(100);
  __atomic_store_n(&futex_word, 1, __ATOMIC_SEQ_CST);
  syscall(SYS_futex, &futex_word, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0);
  return NULL;
}

/* The `sigsuspend creator` mode (see the usage comment). */
#define CREATOR_TRIALS 6
#define CREATOR_CHILD_MS 50
static volatile sig_atomic_t creator_handled = 0;
static volatile sig_atomic_t creator_handled_tid = 0;

static void on_creator_chld(int sig) {
  (void)sig;
  creator_handled += 1;
  /* gettid is async-signal-safe. */
  creator_handled_tid = (sig_atomic_t)syscall(SYS_gettid);
}

static void *creator_thread(void *arg) {
  int *matched = arg;
  pid_t self = (pid_t)syscall(SYS_gettid);
  sigset_t wait_mask;
  if (pthread_sigmask(SIG_SETMASK, NULL, &wait_mask) != 0) _exit(95);
  sigdelset(&wait_mask, SIGCHLD);
  for (int i = 0; i < CREATOR_TRIALS; i++) {
    creator_handled = 0;
    creator_handled_tid = 0;
    struct timespec start, end;
    clock_gettime(CLOCK_MONOTONIC, &start);
    pid_t child = fork();
    if (child < 0) _exit(96);
    if (child == 0) {
      sleep_ms(CREATOR_CHILD_MS);
      _exit(i);
    }
    errno = 0;
    int ret = sigsuspend(&wait_mask);
    int err = errno;
    clock_gettime(CLOCK_MONOTONIC, &end);
    int status = 0;
    pid_t reaped;
    while ((reaped = waitpid(child, &status, 0)) < 0 && errno == EINTR) {
    }
    int after_exit = ms_between(&start, &end) >= CREATOR_CHILD_MS;
    int handled_count = creator_handled;
    int on_creator = creator_handled_tid == self;
    int child_status = reaped == child && WIFEXITED(status) ? WEXITSTATUS(status) : -1;
    printf("TRIAL %d after_exit=%d ret=%d errno=%s handled=%d on_creator=%d status=%d\n", i,
           after_exit, ret, errno_name(err), handled_count, on_creator, child_status);
    fflush(stdout);
    if (after_exit && ret == -1 && err == EINTR && handled_count == 1 && on_creator &&
        child_status == i)
      *matched += 1;
    /* Discard a second SIGCHLD still pending for this exit. */
    set_handler(SIGCHLD, SIG_IGN);
    set_handler(SIGCHLD, on_creator_chld);
  }
  return NULL;
}

static int creator_main(const char *role_name) {
  if (strcmp(role_name, "creator")) return 2;
  set_handler(SIGCHLD, on_creator_chld);
  sigset_t chld;
  sigemptyset(&chld);
  sigaddset(&chld, SIGCHLD);
  if (pthread_sigmask(SIG_BLOCK, &chld, NULL) != 0) return 97;
  say("READY\n");
  int matched = 0;
  pthread_t creator;
  if (pthread_create(&creator, NULL, creator_thread, &matched) != 0) return 98;
  pthread_join(creator, NULL);
  printf("RESULT call=sigsuspend role=creator trials=%d matched=%d\n", CREATOR_TRIALS, matched);
  printf("DONE\n");
  fflush(stdout);
  return 0;
}

/* The `racing` sender (see the usage comment). */
#define RACING_TRIALS 6
#define RACING_TIMEOUT_MS 5000
enum racing_call {
  RACE_POLL,
  RACE_PPOLL,
  RACE_EPOLL,
  RACE_EPOLLP,
  RACE_EPOLLINVAL,
  RACE_EPOLLBADF,
  RACE_SIGTIMEDWAITFAULT
};
static int race_go = 0;

struct racer {
  pid_t target;
  int yields;
};

static void *racer_thread(void *arg) {
  const struct racer *racer = arg;
  while (!__atomic_load_n(&race_go, __ATOMIC_SEQ_CST)) sched_yield();
  for (int i = 0; i < racer->yields; i++) sched_yield();
  if (syscall(SYS_tgkill, getpid(), racer->target, SIGUSR1) != 0) _exit(95);
  return NULL;
}

static int racing_main(const char *call) {
  static const struct {
    const char *name;
    enum racing_call call;
  } calls[] = {
      {"poll", RACE_POLL},           {"ppoll", RACE_PPOLL},
      {"epoll", RACE_EPOLL},         {"epollpwait", RACE_EPOLLP},
      {"epollinval", RACE_EPOLLINVAL}, {"epollbadf", RACE_EPOLLBADF},
      {"sigtimedwaitfault", RACE_SIGTIMEDWAITFAULT},
  };
  int which = -1;
  for (size_t i = 0; i < sizeof calls / sizeof calls[0]; i++)
    if (!strcmp(call, calls[i].name)) which = (int)calls[i].call;
  if (which < 0) return 2;
  set_handler(SIGUSR1, on_usr1);
  int fds[2];
  if (pipe(fds) != 0) return 97;
  /* The read end stays readable for every trial: nothing reads the byte. */
  if (write(fds[1], "x", 1) != 1) return 97;
  int ep = epoll_create1(0);
  if (ep < 0) return 97;
  struct epoll_event event;
  memset(&event, 0, sizeof event);
  event.events = EPOLLIN;
  event.data.u32 = 7;
  if (epoll_ctl(ep, EPOLL_CTL_ADD, fds[0], &event) != 0) return 97;
  /* A descriptor number that is not open. */
  int closed = dup(fds[0]);
  if (closed < 0 || close(closed) != 0) return 97;
  pid_t self = (pid_t)syscall(SYS_gettid);
  say("READY\n");
  int matched = 0;
  for (int i = 0; i < RACING_TRIALS; i++) {
    handled = 0;
    __atomic_store_n(&race_go, 0, __ATOMIC_SEQ_CST);
    struct racer racer = {self, i};
    pthread_t thread;
    if (pthread_create(&thread, NULL, racer_thread, &racer) != 0) return 98;
    __atomic_store_n(&race_go, 1, __ATOMIC_SEQ_CST);
    struct pollfd pfd = {fds[0], POLLIN, 0};
    struct timespec timeout = {RACING_TIMEOUT_MS / 1000, 0};
    struct epoll_event events[4];
    memset(events, 0, sizeof events);
    long ret = -1;
    errno = 0;
    switch (which) {
    case RACE_POLL:
      ret = poll(&pfd, 1, RACING_TIMEOUT_MS);
      break;
    case RACE_PPOLL:
      ret = ppoll(&pfd, 1, &timeout, NULL);
      break;
    case RACE_EPOLL:
      ret = epoll_wait(ep, events, 4, RACING_TIMEOUT_MS);
      break;
    case RACE_EPOLLP:
      ret = syscall(SYS_epoll_pwait, ep, events, 4, RACING_TIMEOUT_MS, NULL, (size_t)8);
      break;
    case RACE_EPOLLINVAL:
      ret = epoll_wait(ep, events, 0, RACING_TIMEOUT_MS);
      break;
    case RACE_EPOLLBADF:
      ret = epoll_wait(closed, events, 4, RACING_TIMEOUT_MS);
      break;
    case RACE_SIGTIMEDWAITFAULT:
      /* Address 1 is never mapped, so the kernel cannot copy the set. */
      ret = syscall(SYS_rt_sigtimedwait, (const void *)1, NULL, &timeout, (size_t)8);
      break;
    }
    int err = errno;
    int got = which == RACE_POLL || which == RACE_PPOLL ? pfd.revents
              : ret > 0                                ? (int)events[0].data.u32
                                                       : 0;
    if (pthread_join(thread, NULL) != 0) return 98;
    /* The racer sent the signal before it exited, so it is pending at the latest
     * now; the first system call below delivers it. */
    wait_for_handler();
    int handled_count = handled;
    printf("TRIAL %d ret=%ld errno=%s got=%d handled=%d\n", i, ret,
           ret < 0 ? errno_name(err) : "none", got, handled_count);
    fflush(stdout);
    int ok;
    switch (which) {
    case RACE_POLL:
    case RACE_PPOLL:
      ok = ret == 1 && got == POLLIN;
      break;
    case RACE_EPOLL:
    case RACE_EPOLLP:
      ok = ret == 1 && got == 7;
      break;
    case RACE_EPOLLINVAL:
      ok = ret == -1 && err == EINVAL;
      break;
    case RACE_EPOLLBADF:
      ok = ret == -1 && err == EBADF;
      break;
    default:
      ok = ret == -1 && err == EFAULT;
      break;
    }
    if (ok && handled_count == 1) matched += 1;
  }
  printf("RESULT call=%s role=racing trials=%d matched=%d\n", call, RACING_TRIALS, matched);
  printf("DONE\n");
  fflush(stdout);
  return 0;
}

int main(int argc, char **argv) {
  if (argc < 3) {
    say("usage: external_signal_interrupt <futex|sem|select|rawselect|poll|epoll|wait4|waitid> "
        "<external|process|thread|timer|exit> [restart] [timed] [warm] "
        "[ignored|blocked|winch|tstp|ign2caught|caught2ign|chldlate|chldign|chldkill|chldthrexit|"
        "chldpend|stealgrp|stealkill|stealthrexit|forkgrp|forkkill|forkthrexit|spin|spinkill|"
        "spinthrexit|usr2|extchld|extchldreaped|extchldlive]\n"
        "       external_signal_interrupt sigsuspend creator\n"
        "       external_signal_interrupt <poll|ppoll|epoll|epollpwait|epollinval|epollbadf|sigtimedwaitfault> racing\n");
    return 2;
  }
  /* Before any handler is installed: the handler compares against it. */
  main_thread = pthread_self();
  const char *call = argv[1];
  const char *sender = argv[2];
  if (!strcmp(call, "sigsuspend")) return creator_main(sender);
  if (!strcmp(sender, "racing")) return racing_main(call);
  int restart = 0, timed = 0, ignored = 0, blocked = 0, warm = 0, usr2 = 0, tstp = 0, options = 0;
  enum extchld extchld = EXTCHLD_NONE;
  static const struct {
    const char *name;
    enum role role;
    enum death death;
  } roles[] = {
      {"stealgrp", ROLE_STEAL, DEATH_GROUP}, {"stealkill", ROLE_STEAL, DEATH_KILL},
      {"stealthrexit", ROLE_STEAL, DEATH_THREXIT}, {"forkgrp", ROLE_FORK, DEATH_GROUP},
      {"forkkill", ROLE_FORK, DEATH_KILL}, {"forkthrexit", ROLE_FORK, DEATH_THREXIT},
      {"spin", ROLE_SPIN, DEATH_GROUP}, {"spinkill", ROLE_SPIN, DEATH_KILL},
      {"spinthrexit", ROLE_SPIN, DEATH_THREXIT},
  };
  for (int i = 3; i < argc; i++) {
    int matched_role = 0;
    for (size_t r = 0; r < sizeof roles / sizeof roles[0]; r++) {
      if (!strcmp(argv[i], roles[r].name)) {
        role = roles[r].role;
        death = roles[r].death;
        options += 1;
        matched_role = 1;
      }
    }
    if (matched_role) continue;
    if (!strcmp(argv[i], "restart")) restart = 1;
    else if (!strcmp(argv[i], "timed")) timed = 1;
    else if (!strcmp(argv[i], "warm")) warm = 1;
    else if (!strcmp(argv[i], "ignored")) ignored = quiet = 1;
    else if (!strcmp(argv[i], "blocked")) blocked = quiet = 1;
    else if (!strcmp(argv[i], "winch")) {
      sent_signal = SIGWINCH;
      quiet = 1;
    } else if (!strcmp(argv[i], "tstp")) {
      sent_signal = SIGTSTP;
      tstp = quiet = 1;
    } else if (!strcmp(argv[i], "ign2caught")) flip = IGN2CAUGHT;
    else if (!strcmp(argv[i], "caught2ign")) flip = CAUGHT2IGN;
    else if (!strcmp(argv[i], "chldlate")) flip = CHLDLATE;
    else if (!strcmp(argv[i], "chldign")) flip = CHLDIGN;
    else if (!strcmp(argv[i], "chldkill")) flip = CHLDKILL;
    else if (!strcmp(argv[i], "chldthrexit")) flip = CHLDTHREXIT;
    else if (!strcmp(argv[i], "chldpend")) flip = CHLDPEND;
    else if (!strcmp(argv[i], "usr2")) usr2 = 1;
    else if (!strcmp(argv[i], "extchld")) extchld = EXTCHLD_CHILDLESS;
    else if (!strcmp(argv[i], "extchldreaped")) extchld = EXTCHLD_REAPED;
    else if (!strcmp(argv[i], "extchldlive")) extchld = EXTCHLD_LIVE;
    else return 2;
  }
  if (ignored + blocked + (sent_signal == SIGWINCH) + tstp + (flip != FLIP_NONE) + options + usr2 +
          (extchld != EXTCHLD_NONE) >
      1)
    return 2;
  flip_timed = timed;
  int is_futex = !strcmp(call, "futex");
  int is_sem = !strcmp(call, "sem");
  int is_wait4 = !strcmp(call, "wait4");
  int is_wait = is_wait4 || !strcmp(call, "waitid");
  int from_process = !strcmp(sender, "process");
  int from_thread = !strcmp(sender, "thread");
  int from_timer = !strcmp(sender, "timer");
  int from_exit = !strcmp(sender, "exit");
  int is_poll = !strcmp(call, "poll");
  int is_epoll = !strcmp(call, "epoll");
  int is_rawselect = !strcmp(call, "rawselect");
  if (!is_futex && !is_sem && !is_wait && !is_poll && !is_epoll && !is_rawselect &&
      strcmp(call, "select"))
    return 2;
  if (!from_process && !from_thread && !from_timer && !from_exit && strcmp(sender, "external"))
    return 2;
  if (is_wait && !from_process) return 2;
  if (quiet && (is_wait || !(from_process || from_thread))) return 2;
  if (flip != FLIP_NONE && (is_wait || !from_thread || restart)) return 2;
  if (from_exit && restart) return 2;
  if (role != ROLE_NONE && (!is_futex || restart)) return 2;
  if ((role == ROLE_STEAL || role == ROLE_FORK) && !from_thread) return 2;
  if (role == ROLE_FORK && !timed) return 2;
  if (role == ROLE_SPIN && !from_exit) return 2;
  if (warm && !is_futex && !is_rawselect && !is_sem && !is_wait) return 2;
  if (is_sem && timed) return 2;
  if (usr2 && strcmp(sender, "external")) return 2;
  if (extchld != EXTCHLD_NONE && (strcmp(sender, "external") || restart || timed || warm || quiet ||
                                  (!is_rawselect && strcmp(call, "select"))))
    return 2;
  int report_elapsed =
      quiet || flip != FLIP_NONE || from_exit || role != ROLE_NONE || warm || (is_wait && restart);
  stamp_handler = is_wait && restart;
  if (tstp) {
    /* Run the test in a child that is alone in an orphaned process group (see
     * the usage comment). The forking thread's pthread_t is also the child's
     * main thread, so main_thread stays valid. */
    pid_t runner = fork();
    if (runner < 0) return 3;
    if (runner > 0) {
      int st;
      while (waitpid(runner, &st, 0) < 0)
        if (errno != EINTR) return 3;
      return WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st);
    }
    if (setsid() < 0) return 3;
  }

  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_handler = on_usr1;
  sigemptyset(&sa.sa_mask);
  sa.sa_flags = restart ? SA_RESTART : 0;
  if ((from_exit || flip == CHLDIGN || role != ROLE_NONE) && sigaction(SIGCHLD, &sa, NULL) != 0)
    return 3;
  if (ignored) sa.sa_handler = SIG_IGN;
  struct sigaction usr1 = sa;
  if (flip == IGN2CAUGHT) usr1.sa_handler = SIG_IGN;
  if (sigaction(SIGUSR1, &usr1, NULL) != 0) return 3;
  if (sigaction(SIGALRM, &sa, NULL) != 0) return 3;
  if (usr2) {
    struct sigaction second = sa;
    second.sa_handler = on_usr2;
    if (sigaction(SIGUSR2, &second, NULL) != 0) return 3;
  }
  if (blocked) {
    sigset_t mask;
    sigemptyset(&mask);
    sigaddset(&mask, SIGUSR1);
    if (sigprocmask(SIG_BLOCK, &mask, NULL) != 0) return 3;
  }
  sem_t sem;
  if (is_sem && sem_init(&sem, 0, 0) != 0) return 3;
  /* Warm the waiting call's site before any helper starts, so the helpers'
   * timing is unchanged. */
  if (warm && is_sem) {
    struct timespec deadline;
    clock_gettime(CLOCK_REALTIME, &deadline);
    add_ms(&deadline, 10);
    if (sem_timedwait(&sem, &deadline) != -1 || errno != ETIMEDOUT) return 3;
  } else if (warm) {
    syscall(SYS_gettid);
  }

  int pfd[2];
  if (pipe(pfd) != 0) return 3;
  int sp[2];
  if (pipe(sp) != 0) return 3;
  ready_write_fd = sp[1];
  pid_t parent = getpid();
  pid_t child = -1;
  pthread_t thread;
  if (extchld == EXTCHLD_REAPED) {
    pid_t reaped = fork();
    if (reaped < 0) return 3;
    if (reaped == 0) _exit(0);
    int st;
    while (waitpid(reaped, &st, 0) < 0)
      if (errno != EINTR) return 3;
    /* Setting SIG_DFL discards a pending SIGCHLD (POSIX sigaction, Linux
     * do_sigaction), so the handler installed next cannot run for this child. */
    struct sigaction dfl;
    memset(&dfl, 0, sizeof dfl);
    dfl.sa_handler = SIG_DFL;
    sigemptyset(&dfl.sa_mask);
    if (sigaction(SIGCHLD, &dfl, NULL) != 0) return 3;
  }
  if (extchld != EXTCHLD_NONE && sigaction(SIGCHLD, &sa, NULL) != 0) return 3;
  if (extchld == EXTCHLD_LIVE) {
    int renamed[2];
    if (pipe(renamed) != 0) return 3;
    child = fork();
    if (child < 0) return 3;
    if (child == 0) {
      /* The harness signals the one process whose argv[0] is the guest's path.
       * Change this copy of argv[0] in place, so that process is the parent,
       * and tell the parent before it prints READY. */
      argv[0][0] = '-';
      close(renamed[0]);
      if (write(renamed[1], "x", 1) != 1) _exit(9);
      close(renamed[1]);
      /* Stay alive until the parent closes the pipe after its RESULT line. */
      close(pfd[1]);
      char c;
      ssize_t r = read(pfd[0], &c, 1);
      _exit(r == 0 ? 7 : 8);
    }
    close(renamed[1]);
    char c;
    if (read(renamed[0], &c, 1) != 1) return 3;
    close(renamed[0]);
  }
  if (from_exit) {
    child = fork();
    if (child < 0) return 3;
    if (child == 0) {
      sleep_ms(100);
      die();
    }
    if (role == ROLE_SPIN && pthread_create(&thread, NULL, spin_sibling, NULL) != 0) return 3;
  }
  if (from_process) {
    child = fork();
    if (child < 0) return 3;
    if (child == 0) {
      signal(SIGUSR1, SIG_IGN);
      close(pfd[1]);
      sleep_ms(100);
      kill(parent, sent_signal);
      /* A wait that Linux restarts under SA_RESTART ends when this child
       * exits, 100 ms after its signal. */
      if (restart && is_wait) {
        sleep_ms(100);
        _exit(7);
      }
      /* Stay alive: the waiter must not depend on this process exiting. */
      char c;
      ssize_t r = read(pfd[0], &c, 1);
      _exit(r == 0 ? 7 : 8);
    }
  }
  close(pfd[0]);
  if (from_thread && pthread_create(&thread, NULL, thread_sender, NULL) != 0) return 3;

  say("READY\n");
  if (from_timer) {
    struct itimerval it;
    memset(&it, 0, sizeof it);
    it.it_value.tv_usec = 100000;
    if (setitimer(ITIMER_REAL, &it, NULL) != 0) return 3;
  }

  long ret;
  int err;
  int epfd = -1;
  if (is_epoll) {
    struct epoll_event ev = {.events = EPOLLIN, .data = {.fd = sp[0]}};
    epfd = epoll_create1(0);
    if (epfd < 0 || epoll_ctl(epfd, EPOLL_CTL_ADD, sp[0], &ev) != 0) return 3;
  }
  struct timespec start;
  clock_gettime(CLOCK_MONOTONIC, &start);
  errno = 0;
  /* A must-not-wake wait, or a timed disposition-change wait, has the short
   * timeout; a readiness wait is otherwise unbounded. */
  int bounded = quiet || (flip != FLIP_NONE && timed);
  if (is_futex) {
    struct timespec timeout = {10, 0};
    if (quiet || flip != FLIP_NONE || role == ROLE_STEAL || role == ROLE_FORK)
      timeout = (struct timespec){0, QUIET_TIMEOUT_MS * 1000000L};
    ret = syscall(SYS_futex, &futex_word, FUTEX_WAIT_PRIVATE, 0,
                  timed || quiet ? &timeout : NULL, NULL, 0);
  } else if (is_sem) {
    struct timespec deadline;
    clock_gettime(CLOCK_REALTIME, &deadline);
    add_ms(&deadline, bounded ? QUIET_TIMEOUT_MS : 10000);
    ret = sem_timedwait(&sem, &deadline);
  } else if (!strcmp(call, "select") || is_rawselect) {
    fd_set rf;
    FD_ZERO(&rf);
    FD_SET(sp[0], &rf);
    struct timeval tv = {0, QUIET_TIMEOUT_MS * 1000L};
    if (is_rawselect)
      ret = syscall(SYS_select, sp[0] + 1, &rf, NULL, NULL, bounded ? &tv : NULL);
    else
      ret = select(sp[0] + 1, &rf, NULL, NULL, bounded ? &tv : NULL);
  } else if (is_poll) {
    struct pollfd pfd1 = {.fd = sp[0], .events = POLLIN};
    ret = poll(&pfd1, 1, bounded ? QUIET_TIMEOUT_MS : -1);
  } else if (is_epoll) {
    struct epoll_event ev;
    ret = epoll_wait(epfd, &ev, 1, bounded ? QUIET_TIMEOUT_MS : -1);
  } else if (is_wait4) {
    int st = 0;
    struct rusage ru;
    /* `warm` runs the wait through glibc's syscall() instruction, the site
     * syscall(SYS_gettid) warmed. */
    ret = warm ? syscall(SYS_wait4, child, &st, 0, &ru) : wait4(child, &st, 0, &ru);
  } else {
    siginfo_t si;
    memset(&si, 0, sizeof si);
    ret = warm ? syscall(SYS_waitid, P_PID, child, &si, WEXITED, NULL)
               : waitid(P_PID, child, &si, WEXITED);
  }
  err = errno;
  struct timespec end;
  clock_gettime(CLOCK_MONOTONIC, &end);

  char buf[160];
  /* A wait4 that reaps the child prints `child` rather than its pid. */
  char ret_text[32];
  if (is_wait4 && child > 0 && ret == child)
    snprintf(ret_text, sizeof ret_text, "child");
  else
    snprintf(ret_text, sizeof ret_text, "%ld", ret < 0 ? -1L : ret);
  snprintf(buf, sizeof buf, "RESULT call=%s ret=%s errno=%s handler=%d\n", call, ret_text,
           ret < 0 ? errno_name(err) : "none", (int)handled);
  say(buf);
  if (report_elapsed) {
    snprintf(buf, sizeof buf, "ELAPSED ms=%ld\n", ms_between(&start, &end));
    say(buf);
  }
  if (stamp_handler) {
    /* The handler ran before the call returned or restarted, if at all. */
    snprintf(buf, sizeof buf, "HANDLED_AT ms=%ld\n",
             handled > 0 ? ms_between(&start, &handled_at) : -1L);
    say(buf);
  }
  if (from_thread || role == ROLE_SPIN) pthread_join(thread, NULL);
  if (role == ROLE_FORK) {
    snprintf(buf, sizeof buf, "SIBLING ret=%ld errno=%s ms=%ld\n", sibling_ret < 0 ? -1L : sibling_ret,
             sibling_ret < 0 ? errno_name(sibling_err) : "none", sibling_ms);
    say(buf);
  }
  if (role != ROLE_NONE) {
    snprintf(buf, sizeof buf, "HANDLER main=%d sibling=%d\n", (int)handled_main,
             (int)handled_sibling);
    say(buf);
  }
  if (usr2) {
    /* Both signals were sent before the wait could return; give a handler
     * that has not run yet a bounded time before counting. */
    for (int i = 0; i < 200 && !(handled && handled_usr2); i++) sleep_ms(10);
    snprintf(buf, sizeof buf, "HANDLED usr1=%d usr2=%d\n", (int)handled, (int)handled_usr2);
    say(buf);
  }
  if (child > 0) {
    close(pfd[1]);
    int st;
    while (waitpid(child, &st, 0) < 0 && errno == EINTR) {
    }
  }
  say("DONE\n");
  return 0;
}
