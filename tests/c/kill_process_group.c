/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Signals addressed to a process group
 * (https://github.com/rrnewton/hermit/issues/4046). rt_sigqueueinfo has no
 * process groups: Linux finds no process for pid 0 or
 * a negative pid and returns ESRCH, natively and under Hermit. kill(0, sig)
 * and kill(-pgrp, sig) signal every process in the group; Detcore does not
 * model that, so it refuses them by name: a fail-closed run stops with the
 * policy-refusal status after the rt_sigqueueinfo lines, and
 * --allow-unsupported-syscalls returns ENOSYS.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static volatile sig_atomic_t handled;

static void on_usr1(int sig) {
  (void)sig;
  handled++;
}

static void report(const char* what, long ret) {
  if (ret == 0) {
    printf("%s: 0, handled %d\n", what, (int)handled);
  } else {
    printf("%s: %ld %s, handled %d\n", what, ret, strerrorname_np(errno), (int)handled);
  }
}

static long queue(pid_t pid) {
  siginfo_t info;
  memset(&info, 0, sizeof(info));
  info.si_signo = SIGUSR1;
  info.si_code = SI_QUEUE;
  info.si_pid = getpid();
  info.si_uid = getuid();
  return syscall(SYS_rt_sigqueueinfo, pid, SIGUSR1, &info);
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = on_usr1;
  sigaction(SIGUSR1, &action, NULL);

  report("rt_sigqueueinfo(0)", queue(0));
  report("rt_sigqueueinfo(-pgrp)", queue(-getpgrp()));
  report("kill(0)", kill(0, SIGUSR1));
  report("kill(-pgrp)", kill(-getpgrp(), SIGUSR1));
  return 0;
}
