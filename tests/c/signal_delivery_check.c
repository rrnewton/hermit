/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Caught signal deliveries that Hermit's Detcore checks against the kernel
// before its signal-state copy records them (A5 step 3.2,
// https://github.com/rrnewton/hermit/issues/4485). Each mode makes one
// delivery of a known shape, from a single thread, and checks what Linux
// guarantees about it: that the handler ran once, the mask it ran under, the
// interrupted call's result or restart, and the action afterwards. It prints
// "OK <mode>" and exits 0, or prints what differed and exits 1.
//
// Signals come from the thread itself (raise, sigqueue) or from the
// scheduler (an ITIMER_REAL expiry), never from outside, so every run is
// deterministic.

#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t handled;
static volatile uint64_t handler_mask;
static volatile int handler_value;
static int write_end = -1;

static uint64_t current_mask(void) {
  sigset_t now;
  uint64_t bits = 0;
  if (sigprocmask(SIG_BLOCK, NULL, &now) != 0) {
    _exit(2);
  }
  for (int sig = 1; sig <= 64; sig++) {
    if (sigismember(&now, sig) == 1) {
      bits |= 1ull << (sig - 1);
    }
  }
  return bits;
}

static uint64_t bit(int sig) {
  return 1ull << (sig - 1);
}

static void on_signal(int sig, siginfo_t* info, void* context) {
  (void)sig;
  (void)context;
  handled++;
  handler_mask = current_mask();
  handler_value = info->si_value.sival_int;
  if (write_end >= 0) {
    char byte = 'x';
    if (write(write_end, &byte, 1) != 1) {
      _exit(3);
    }
  }
}

// Catches `sig` with SIGUSR2 in its sa_mask.
static void catch_signal(int sig, int flags) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_sigaction = on_signal;
  action.sa_flags = SA_SIGINFO | flags;
  sigemptyset(&action.sa_mask);
  sigaddset(&action.sa_mask, SIGUSR2);
  if (sigaction(sig, &action, NULL) != 0) {
    perror("sigaction");
    exit(2);
  }
}

static void set_mask(uint64_t bits) {
  sigset_t set;
  sigemptyset(&set);
  for (int sig = 1; sig <= 64; sig++) {
    if (bits & bit(sig)) {
      sigaddset(&set, sig);
    }
  }
  if (sigprocmask(SIG_SETMASK, &set, NULL) != 0) {
    perror("sigprocmask");
    exit(2);
  }
}

static void arm_alarm(void) {
  struct itimerval timer = {{0, 0}, {0, 20000}};
  if (setitimer(ITIMER_REAL, &timer, NULL) != 0) {
    perror("setitimer");
    exit(2);
  }
}

static int failures;

static void expect(const char* what, long long actual, long long expected) {
  if (actual != expected) {
    printf("FAIL %s: %lld, expected %lld\n", what, actual, expected);
    failures++;
  }
}

int main(int argc, char** argv) {
  const char* mode = argc > 1 ? argv[1] : "";
  const uint64_t own = bit(SIGINT);
  const uint64_t sa_mask = bit(SIGUSR2);
  set_mask(own);
  long result = 0;
  int error = 0;

  if (!strcmp(mode, "raise")) {
    catch_signal(SIGUSR1, 0);
    result = raise(SIGUSR1);
    expect("raise", result, 0);
    expect("handler mask", (long long)handler_mask, (long long)(own | sa_mask | bit(SIGUSR1)));
  } else if (!strcmp(mode, "pause")) {
    catch_signal(SIGALRM, 0);
    arm_alarm();
    result = pause();
    error = errno;
    expect("pause", result, -1);
    expect("pause errno", error, EINTR);
  } else if (!strcmp(mode, "nanosleep")) {
    catch_signal(SIGALRM, 0);
    arm_alarm();
    struct timespec duration = {5, 0};
    result = nanosleep(&duration, NULL);
    error = errno;
    expect("nanosleep", result, -1);
    expect("nanosleep errno", error, EINTR);
  } else if (!strcmp(mode, "read") || !strcmp(mode, "read-restart")) {
    int restart = !strcmp(mode, "read-restart");
    catch_signal(SIGALRM, restart ? SA_RESTART : 0);
    int pipe_ends[2];
    if (pipe(pipe_ends) != 0) {
      perror("pipe");
      return 2;
    }
    // With SA_RESTART the read restarts, and the handler's byte ends it.
    if (restart) {
      write_end = pipe_ends[1];
    }
    arm_alarm();
    char byte;
    result = read(pipe_ends[0], &byte, 1);
    error = errno;
    if (restart) {
      expect("restarted read", result, 1);
    } else {
      expect("read", result, -1);
      expect("read errno", error, EINTR);
    }
  } else if (!strcmp(mode, "spin")) {
    catch_signal(SIGALRM, 0);
    arm_alarm();
    volatile unsigned long spins = 0;
    while (!handled) {
      spins++;
    }
  } else if (!strcmp(mode, "suspend")) {
    catch_signal(SIGALRM, 0);
    set_mask(own | bit(SIGALRM));
    arm_alarm();
    sigset_t temporary;
    sigemptyset(&temporary);
    result = sigsuspend(&temporary);
    error = errno;
    expect("sigsuspend", result, -1);
    expect("sigsuspend errno", error, EINTR);
    expect("handler mask", (long long)handler_mask, (long long)(sa_mask | bit(SIGALRM)));
    expect("mask after sigsuspend", (long long)current_mask(), (long long)(own | bit(SIGALRM)));
  } else if (!strcmp(mode, "realtime")) {
    catch_signal(SIGRTMIN + 3, 0);
    union sigval value = {.sival_int = 7};
    result = sigqueue(getpid(), SIGRTMIN + 3, value);
    expect("sigqueue", result, 0);
    expect("handler value", handler_value, 7);
    expect(
        "handler mask",
        (long long)handler_mask,
        (long long)(own | sa_mask | bit(SIGRTMIN + 3)));
  } else if (!strcmp(mode, "resethand")) {
    catch_signal(SIGUSR1, SA_RESETHAND);
    raise(SIGUSR1);
    struct sigaction now;
    sigaction(SIGUSR1, NULL, &now);
    expect("action after SA_RESETHAND", now.sa_handler == SIG_DFL, 1);
  } else if (!strcmp(mode, "nodefer")) {
    catch_signal(SIGUSR1, SA_NODEFER);
    raise(SIGUSR1);
    expect("handler mask", (long long)handler_mask, (long long)(own | sa_mask));
  } else if (!strcmp(mode, "ignored")) {
    signal(SIGUSR1, SIG_IGN);
    raise(SIGUSR1);
    expect("handled", handled, 0);
  } else {
    printf("unknown mode %s\n", mode);
    return 2;
  }
  if (strcmp(mode, "ignored")) {
    expect("handled", handled, 1);
  }
  if (failures) {
    return 1;
  }
  printf("OK %s\n", mode);
  return 0;
}
