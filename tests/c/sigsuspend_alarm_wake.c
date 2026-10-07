/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * `sigsuspend(2)` with a pending timer signal that only the suspend mask
 * unblocks. The thread blocks SIGALRM, arms `alarm(1)`, and then waits in
 * `sigsuspend` with an empty mask, so the alarm cannot fire before the wait
 * begins and the wait can end only through that delivery.
 *
 * Under Detcore the alarm is the scheduler's own timed event: `fire_alarm`
 * signals a thread that is parked in the scheduler's `rt_sigsuspend` pool. The
 * scheduler must take the thread out of that pool and wait for the thread's
 * own report of the interrupted syscall. It must not fabricate that report.
 * Before that was fixed, debug builds failed the assertion
 * `signal_guest: thread should be parked in the scheduler` here, and the run
 * exited 125 with no verify result.
 *
 * The second phase gives the alarm a target that cannot take it. The main
 * thread, which armed the alarm, waits in `sigsuspend` with SIGALRM still
 * blocked, while a worker waits with a mask that admits it. The alarm is
 * process-directed, so Linux delivers it to the worker, and the worker wakes
 * the main thread with SIGUSR1. The scheduler must choose the worker itself.
 * Before it did, the scheduler signalled the main thread, the host kernel
 * rerouted the signal, and a scheduler with nothing else to run could report
 * a terminal deadlock before the worker's report arrived. Under `--verify`
 * that happened on every run.
 *
 * The third phase, timer-sibling-exit, is a periodic POSIX timer whose arming
 * thread cannot take its signal and then exits. A sibling unblocks SIGALRM,
 * arms the timer (SIGEV_SIGNAL, an absolute first expiry, period
 * TIMER_PERIOD_NS), sleeps until one nanosecond before the second expiry,
 * and leaves with a raw exit(2), so its exit is the next thing it does after
 * the sleep. The main thread keeps SIGALRM blocked outside `sigsuspend` and
 * must be woken by every one of TIMER_WAKES expiries, each within
 * TIMER_OVERSHOOT_NS of it. On Linux the signal is queued on the process and
 * wakes the main thread, the only thread that can take it at once: the
 * sibling is asleep at the first expiry and gone by the third, and at the
 * second it is woken by the same timer interrupt only after the expiry is
 * queued. Under Detcore the timer names the sibling, which the scheduler
 * prefers while it lives, and the scheduler may send a copy to one thread
 * alone only if that thread takes it before it runs any guest instruction
 * or exits. A copy sent to the sleeping sibling, or to the sibling held at
 * its exit, where the kernel discards it with the thread, makes the main
 * thread miss an expiry and wake late.
 *
 * Natively the phase keeps both threads, and so both timers, on one CPU.
 * The timer has no slack and the sibling's sleep has the default 50 us, so
 * at the second expiry both expire in one interrupt, which handles the
 * timer first: the expiry is queued for the main thread before the sibling
 * can run. Linux would prefer the sibling if the expiry interrupted it.
 * Detcore keeps CPU affinity virtual, so the pinning changes nothing under
 * Hermit.
 *
 * `pause_alarm_interrupt.c` is the neighbouring case: `pause(2)` is modelled
 * as an indefinite sleep, not as an `rt_sigsuspend` blocker, so it does not
 * reach this path. In `signal_determinism.c itimer-delivery` the timer fires
 * before the suspend begins, so the signal is already pending and it does not
 * reach this path either.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

/* timer-sibling-exit (see the comment at the top). */
#define TIMER_PERIOD_NS 100000000L
#define TIMER_WAKES 4
#define TIMER_OVERSHOOT_NS 50000000LL

static volatile sig_atomic_t handler_runs = 0;

static void on_alarm(int signo) {
  (void)signo;
  handler_runs++;
}

static pthread_t main_thread;
static pthread_t worker_thread;
static volatile sig_atomic_t rerouted_alarm_runs = 0;
static volatile sig_atomic_t rerouted_alarm_on_worker = 0;
static volatile sig_atomic_t wake_runs = 0;

static void on_rerouted_alarm(int signo) {
  (void)signo;
  rerouted_alarm_runs++;
  rerouted_alarm_on_worker = pthread_equal(pthread_self(), worker_thread);
}

static void on_wake(int signo) {
  (void)signo;
  wake_runs++;
}

/* Waits with SIGALRM admitted and SIGUSR1 blocked, then wakes the main thread
 * with SIGUSR1. Both signals stay blocked outside the suspend. */
static void* rerouted_alarm_worker(void* arg) {
  (void)arg;
  sigset_t admit_alarm;
  sigemptyset(&admit_alarm);
  sigaddset(&admit_alarm, SIGUSR1);
  int returns = 0;
  while (rerouted_alarm_runs == 0 && returns < 4) {
    sigsuspend(&admit_alarm);
    returns++;
  }
  pthread_kill(main_thread, SIGUSR1);
  return NULL;
}

/* Returns 0 on success. */
static int rerouted_alarm_phase(void) {
  struct sigaction sa;
  memset(&sa, 0, sizeof(sa));
  sigemptyset(&sa.sa_mask);
  sa.sa_handler = on_rerouted_alarm;
  if (sigaction(SIGALRM, &sa, NULL) != 0) {
    puts("SIGSUSPEND_ALARM_SIGACTION_FAILED");
    return 1;
  }
  sa.sa_handler = on_wake;
  if (sigaction(SIGUSR1, &sa, NULL) != 0) {
    puts("SIGSUSPEND_ALARM_SIGACTION_FAILED");
    return 1;
  }

  sigset_t both;
  sigemptyset(&both);
  sigaddset(&both, SIGALRM);
  sigaddset(&both, SIGUSR1);
  if (sigprocmask(SIG_BLOCK, &both, NULL) != 0) {
    puts("SIGSUSPEND_ALARM_SIGPROCMASK_FAILED");
    return 1;
  }

  main_thread = pthread_self();
  if (pthread_create(&worker_thread, NULL, rerouted_alarm_worker, NULL) != 0) {
    puts("SIGSUSPEND_ALARM_PTHREAD_CREATE_FAILED");
    return 1;
  }
  alarm(1);

  sigset_t admit_wake;
  sigemptyset(&admit_wake);
  sigaddset(&admit_wake, SIGALRM);
  int returns = 0;
  while (wake_runs == 0 && returns < 4) {
    sigsuspend(&admit_wake);
    returns++;
  }
  if (pthread_join(worker_thread, NULL) != 0) {
    puts("SIGSUSPEND_ALARM_PTHREAD_JOIN_FAILED");
    return 1;
  }

  printf(
      "rerouted alarm_runs=%d alarm_on_worker=%d wake_runs=%d main_returns=%d\n",
      (int)rerouted_alarm_runs,
      (int)rerouted_alarm_on_worker,
      (int)wake_runs,
      returns);
  return !(rerouted_alarm_runs == 1 && rerouted_alarm_on_worker &&
           wake_runs == 1 && returns == 1);
}

static volatile sig_atomic_t timer_runs = 0;
static volatile sig_atomic_t timer_runs_on_waiter = 0;
static volatile sig_atomic_t sibling_sleep_interrupted = 0;
static atomic_int timer_armed;
static timer_t periodic_timer;
static struct timespec first_expiry;

static void on_timer(int signo) {
  (void)signo;
  timer_runs++;
  if (pthread_equal(pthread_self(), main_thread)) {
    timer_runs_on_waiter++;
  }
}

static struct timespec timespec_add_ns(struct timespec t, long ns) {
  t.tv_sec += ns / 1000000000L;
  t.tv_nsec += ns % 1000000000L;
  if (t.tv_nsec >= 1000000000L) {
    t.tv_sec++;
    t.tv_nsec -= 1000000000L;
  }
  return t;
}

static long long timespec_ns(const struct timespec* t) {
  return (long long)t->tv_sec * 1000000000LL + t->tv_nsec;
}

/* Arms the timer with SIGALRM unblocked, sleeps until one nanosecond before
 * the second expiry, and exits without returning through glibc. */
static void* timer_sibling(void* arg) {
  (void)arg;
  sigset_t alarm_only;
  sigemptyset(&alarm_only);
  sigaddset(&alarm_only, SIGALRM);
  struct sigevent sev;
  memset(&sev, 0, sizeof(sev));
  sev.sigev_notify = SIGEV_SIGNAL;
  sev.sigev_signo = SIGALRM;
  if (pthread_sigmask(SIG_UNBLOCK, &alarm_only, NULL) != 0 ||
      timer_create(CLOCK_MONOTONIC, &sev, &periodic_timer) != 0) {
    atomic_store(&timer_armed, -1);
    return NULL;
  }
  struct timespec now;
  struct itimerspec spec;
  memset(&spec, 0, sizeof(spec));
  int ok = clock_gettime(CLOCK_MONOTONIC, &now) == 0;
  if (ok) {
    first_expiry = timespec_add_ns(now, TIMER_PERIOD_NS);
    spec.it_value = first_expiry;
    spec.it_interval.tv_nsec = TIMER_PERIOD_NS;
    ok = timer_settime(periodic_timer, TIMER_ABSTIME, &spec, NULL) == 0;
  }
  if (!ok) {
    timer_delete(periodic_timer);
    atomic_store(&timer_armed, -1);
    return NULL;
  }
  atomic_store(&timer_armed, 1);
  struct timespec until = timespec_add_ns(first_expiry, TIMER_PERIOD_NS - 1);
  while (clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &until, NULL) ==
         EINTR) {
    sibling_sleep_interrupted++;
  }
  syscall(SYS_exit, 0);
  return NULL;
}

/* Keeps the calling thread, and the threads it creates, on its lowest
 * allowed CPU. Natively that removes a race (see the comment at the top);
 * under Hermit the call has no effect, and its result is not checked. */
static void stay_on_one_cpu(void) {
  cpu_set_t allowed;
  CPU_ZERO(&allowed);
  if (sched_getaffinity(0, sizeof(allowed), &allowed) != 0) {
    return;
  }
  for (int cpu = 0; cpu < CPU_SETSIZE; cpu++) {
    if (CPU_ISSET(cpu, &allowed)) {
      cpu_set_t one;
      CPU_ZERO(&one);
      CPU_SET(cpu, &one);
      (void)sched_setaffinity(0, sizeof(one), &one);
      return;
    }
  }
}

/* timer-sibling-exit (see the comment at the top). Returns 0 on success. */
static int timer_sibling_exit_phase(void) {
  struct sigaction sa;
  memset(&sa, 0, sizeof(sa));
  sigemptyset(&sa.sa_mask);
  sa.sa_handler = on_timer;
  if (sigaction(SIGALRM, &sa, NULL) != 0) {
    puts("SIGSUSPEND_ALARM_SIGACTION_FAILED");
    return 1;
  }
  sigset_t alarm_only;
  sigemptyset(&alarm_only);
  sigaddset(&alarm_only, SIGALRM);
  if (sigprocmask(SIG_BLOCK, &alarm_only, NULL) != 0) {
    puts("SIGSUSPEND_ALARM_SIGPROCMASK_FAILED");
    return 1;
  }
  stay_on_one_cpu();

  main_thread = pthread_self();
  atomic_store(&timer_armed, 0);
  pthread_t sibling;
  if (pthread_create(&sibling, NULL, timer_sibling, NULL) != 0) {
    puts("SIGSUSPEND_ALARM_PTHREAD_CREATE_FAILED");
    return 1;
  }
  while (atomic_load(&timer_armed) == 0) {
    sched_yield();
  }
  int armed = atomic_load(&timer_armed) == 1;

  /* SIGALRM stays blocked outside the suspend, so each return must come from
   * a delivery inside it; expiry k is due TIMER_PERIOD_NS * k after the
   * first. */
  sigset_t admit_alarm;
  sigfillset(&admit_alarm);
  sigdelset(&admit_alarm, SIGALRM);
  int eintr_returns = 0;
  int in_window = 0;
  for (int k = 0; armed && k < TIMER_WAKES; k++) {
    errno = 0;
    int rc = sigsuspend(&admit_alarm);
    int eintr = rc == -1 && errno == EINTR;
    struct timespec at;
    clock_gettime(CLOCK_MONOTONIC, &at);
    eintr_returns += eintr;
    long long late = timespec_ns(&at) - timespec_ns(&first_expiry) -
        (long long)k * TIMER_PERIOD_NS;
    if (late >= 0 && late < TIMER_OVERSHOOT_NS) {
      in_window++;
    } else {
      /* The time goes to stderr only, so stdout stays the same natively and
       * under Hermit. */
      fprintf(
          stderr,
          "timer-sibling-exit wake %d came %lld ns after expiry %d\n",
          k,
          late,
          k);
    }
  }
  if (armed) {
    struct itimerspec off;
    memset(&off, 0, sizeof(off));
    timer_settime(periodic_timer, 0, &off, NULL);
    timer_delete(periodic_timer);
  }
  int joined = pthread_join(sibling, NULL) == 0;

  printf(
      "timer-sibling-exit armed=%d eintr=%d timer_runs=%d on_waiter=%d "
      "in_window=%d sibling_sleep_interrupted=%d joined=%d\n",
      armed,
      eintr_returns,
      (int)timer_runs,
      (int)timer_runs_on_waiter,
      in_window,
      (int)sibling_sleep_interrupted,
      joined);
  return !(armed && eintr_returns == TIMER_WAKES &&
           timer_runs == TIMER_WAKES && timer_runs_on_waiter == TIMER_WAKES &&
           in_window == TIMER_WAKES && sibling_sleep_interrupted == 0 &&
           joined);
}

int main(void) {
  struct sigaction sa;
  memset(&sa, 0, sizeof(sa));
  sa.sa_handler = on_alarm;
  sigemptyset(&sa.sa_mask);
  if (sigaction(SIGALRM, &sa, NULL) != 0) {
    puts("SIGSUSPEND_ALARM_SIGACTION_FAILED");
    return 1;
  }

  sigset_t alarm_only;
  sigset_t empty;
  sigemptyset(&alarm_only);
  sigaddset(&alarm_only, SIGALRM);
  sigemptyset(&empty);
  if (sigprocmask(SIG_BLOCK, &alarm_only, NULL) != 0) {
    puts("SIGSUSPEND_ALARM_SIGPROCMASK_FAILED");
    return 1;
  }

  alarm(1);

  /* SIGALRM stays blocked outside the suspend, so every return must come from
   * a delivery inside it; one delivery is expected. */
  int returns = 0;
  int eintr_returns = 0;
  while (handler_runs == 0 && returns < 4) {
    errno = 0;
    int rc = sigsuspend(&empty);
    returns++;
    if (rc == -1 && errno == EINTR) {
      eintr_returns++;
    }
  }

  sigset_t after;
  sigemptyset(&after);
  int mask_restored = sigprocmask(SIG_SETMASK, NULL, &after) == 0 &&
      sigismember(&after, SIGALRM) == 1;

  /* Report booleans and counts rather than strerror(3) so the output cannot
   * depend on the ambient locale. */
  printf(
      "sigsuspend returns=%d eintr=%d handler_runs=%d mask_restored=%d\n",
      returns,
      eintr_returns,
      (int)handler_runs,
      mask_restored);

  if (returns != 1 || eintr_returns != 1 || handler_runs != 1 ||
      !mask_restored) {
    puts("SIGSUSPEND_ALARM_WAKE_FAILED");
    return 1;
  }

  if (rerouted_alarm_phase() != 0) {
    puts("SIGSUSPEND_ALARM_WAKE_FAILED");
    return 1;
  }

  if (timer_sibling_exit_phase() != 0) {
    puts("SIGSUSPEND_ALARM_WAKE_FAILED");
    return 1;
  }

  puts("SIGSUSPEND_ALARM_WAKE_OK");
  return 0;
}
