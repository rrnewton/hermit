/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A signal's action and mask bit stay as the guest set them across what the
// instrumentation does in between, as they do natively
// (https://github.com/rrnewton/hermit/issues/4409,
// https://github.com/rrnewton/hermit/issues/4485). Under ptrace, a
// single-step of Reverie's ends with a forced SIGTRAP, and a trapped rdtsc or
// cpuid with a forced SIGSEGV; a forced signal that is blocked or ignored
// resets the action to SIG_DFL and unblocks it.
//
// Usage: signal_state_kept MODE STATE
//   MODE  threads: pthread_create (whose block-all rt_sigprocmask Detcore
//                  injects) and a joined thread, for SIGTRAP;
//         rdtsc, cpuid: the instruction, for SIGSEGV.
//   STATE ign, ign-blocked, handler-blocked, dfl-blocked: the signal's
//         action and mask bit before.
//
// Prints the signal's state as the guest sees it (the action's handler kind,
// flags and sa_mask, the rt_sigprocmask bit, and procfs SigBlk, SigIgn and
// SigCgt), before and after, for each thread that reads it, then "done". The
// output is the same natively and under Hermit; the caller compares the two.

#include <cpuid.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <x86intrin.h>

static int sig;

static void handler(int signo) {
  (void)signo;
}

static unsigned long long status_field(const char* name) {
  FILE* file = fopen("/proc/thread-self/status", "r");
  if (!file) {
    perror("fopen /proc/thread-self/status");
    exit(2);
  }
  char line[256];
  size_t length = strlen(name);
  unsigned long long value = ~0ULL;
  while (fgets(line, sizeof line, file)) {
    if (strncmp(line, name, length) == 0) {
      value = strtoull(line + length, NULL, 16);
      break;
    }
  }
  fclose(file);
  if (value == ~0ULL) {
    fprintf(stderr, "no %s in /proc/thread-self/status\n", name);
    exit(2);
  }
  return value;
}

static void report(const char* where) {
  struct sigaction action;
  if (sigaction(sig, NULL, &action) != 0) {
    perror("sigaction");
    exit(2);
  }
  const char* kind = action.sa_handler == SIG_DFL ? "dfl"
      : action.sa_handler == SIG_IGN              ? "ign"
      : action.sa_handler == handler              ? "handler"
                                                  : "other";
  sigset_t mask;
  if (pthread_sigmask(SIG_BLOCK, NULL, &mask) != 0) {
    fprintf(stderr, "pthread_sigmask failed\n");
    exit(2);
  }
  unsigned long long bit = 1ULL << (sig - 1);
  printf(
      "%s: action=%s flags=%#x usr2-in-sa_mask=%d blocked=%d "
      "SigBlk=%d SigIgn=%d SigCgt=%d SigBlk-all=%#llx\n",
      where,
      kind,
      (unsigned)action.sa_flags,
      sigismember(&action.sa_mask, SIGUSR2),
      sigismember(&mask, sig),
      (status_field("SigBlk:") & bit) != 0,
      (status_field("SigIgn:") & bit) != 0,
      (status_field("SigCgt:") & bit) != 0,
      status_field("SigBlk:"));
  fflush(stdout);
}

static void set_state(const char* state) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  sigemptyset(&action.sa_mask);
  sigaddset(&action.sa_mask, SIGUSR2);
  action.sa_flags = SA_RESTART;
  int block = 1;
  if (strcmp(state, "ign") == 0) {
    action.sa_handler = SIG_IGN;
    block = 0;
  } else if (strcmp(state, "ign-blocked") == 0) {
    action.sa_handler = SIG_IGN;
  } else if (strcmp(state, "handler-blocked") == 0) {
    action.sa_handler = handler;
  } else if (strcmp(state, "dfl-blocked") == 0) {
    action.sa_handler = SIG_DFL;
  } else {
    fprintf(stderr, "unknown state %s\n", state);
    exit(2);
  }
  if (sigaction(sig, &action, NULL) != 0) {
    perror("sigaction");
    exit(2);
  }
  sigset_t mask;
  sigemptyset(&mask);
  sigaddset(&mask, sig);
  if (pthread_sigmask(block ? SIG_BLOCK : SIG_UNBLOCK, &mask, NULL) != 0) {
    fprintf(stderr, "pthread_sigmask failed\n");
    exit(2);
  }
}

static void* thread_main(void* arg) {
  (void)arg;
  report("thread");
  return NULL;
}

int main(int argc, char** argv) {
  if (argc != 3) {
    fprintf(stderr, "usage: %s threads|rdtsc|cpuid STATE\n", argv[0]);
    return 2;
  }
  const char* mode = argv[1];
  sig = strcmp(mode, "threads") == 0 ? SIGTRAP : SIGSEGV;
  set_state(argv[2]);
  report("before");
  if (strcmp(mode, "threads") == 0) {
    pthread_t thread;
    if (pthread_create(&thread, NULL, thread_main, NULL) != 0) {
      fprintf(stderr, "pthread_create failed\n");
      return 2;
    }
    if (pthread_join(thread, NULL) != 0) {
      fprintf(stderr, "pthread_join failed\n");
      return 2;
    }
  } else if (strcmp(mode, "rdtsc") == 0) {
    volatile uint64_t tsc = __rdtsc();
    (void)tsc;
  } else if (strcmp(mode, "cpuid") == 0) {
    unsigned eax, ebx, ecx, edx;
    __cpuid(0, eax, ebx, ecx, edx);
    volatile unsigned sink = eax ^ ebx ^ ecx ^ edx;
    (void)sink;
  } else {
    fprintf(stderr, "unknown mode %s\n", mode);
    return 2;
  }
  report("after");
  printf("done\n");
  return 0;
}
