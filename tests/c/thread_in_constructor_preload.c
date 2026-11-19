/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A preload whose constructor creates and joins a thread before the
 * program's main.
 *
 * glibc installs its handler for signal 33 (SIGSETXID) when a process creates
 * its first thread, so a launcher's ignored 33 is replaced by that handler
 * before main runs. Only code that runs before every constructor, such as an
 * executable's preinit array, still sees the launcher's dispositions.
 *
 * The constructor then reports signal 33's disposition on stderr, as the raw
 * system call returns it (glibc's sigaction refuses signals 32 and 33), and
 * removes LD_PRELOAD from the environment, so that the programs the process
 * starts do not load the preload.
 */

#define _GNU_SOURCE
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <unistd.h>

/* The kernel's struct sigaction on x86_64. */
struct kernel_sigaction {
  void (*handler)(int);
  unsigned long flags;
  void (*restorer)(void);
  unsigned long mask;
};

static void* returns_at_once(void* argument) {
  return argument;
}

__attribute__((constructor)) static void create_a_thread_before_main(void) {
  pthread_t thread;
  if (pthread_create(&thread, NULL, returns_at_once, NULL) != 0 ||
      pthread_join(thread, NULL) != 0) {
    fputs("preload: the thread failed\n", stderr);
    _exit(97);
  }
  struct kernel_sigaction old;
  if (syscall(SYS_rt_sigaction, 33, NULL, &old, sizeof(old.mask)) != 0) {
    perror("preload: rt_sigaction");
    _exit(97);
  }
  const char* disposition = old.handler == SIG_IGN ? "ignored"
      : old.handler == SIG_DFL                     ? "default"
                                                   : "caught";
  fprintf(stderr, "preload: signal 33 %s after the thread\n", disposition);
  if (unsetenv("LD_PRELOAD") != 0) {
    perror("preload: unsetenv");
    _exit(97);
  }
}
