/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Signals addressed to a process group
 * (https://github.com/rrnewton/hermit/issues/4046). The guest first makes
 * itself a process group leader, so -getpgrp() is a real negative pid inside
 * Hermit's PID namespace too.
 *   - rt_sigqueueinfo has no process groups: Linux finds no process for pid
 *     0, a negative pid or -1 and returns ESRCH, natively and under Hermit.
 *   - kill(INT_MIN, sig) is ESRCH, for SIGKILL too: Linux rejects INT_MIN
 *     before anything else.
 *   - kill(0, sig), kill(-pgrp, sig) and kill(-1, sig) signal every process
 *     in a set; Detcore does not model that and refuses them by name. A
 *     fail-closed run stops with the policy-refusal status at kill(0);
 *     --allow-unsupported-syscalls returns ENOSYS for each.
 * kill(-1, sig) runs only with the argument "broadcast": natively it would
 * signal every process the user may signal. With the argument "inherited" the
 * guest keeps the process group it started in, which under Hermit is
 * Hermit's own (https://github.com/rrnewton/hermit/issues/4057), first
 * checks the group with signal 0, which sends nothing, and last sends
 * SIGKILL to it; run it only inside setsid.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <limits.h>
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

int main(int argc, char** argv) {
  int broadcast = argc > 1 && strcmp(argv[1], "broadcast") == 0;
  int inherited = argc > 1 && strcmp(argv[1], "inherited") == 0;
  setvbuf(stdout, NULL, _IONBF, 0);
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = on_usr1;
  sigaction(SIGUSR1, &action, NULL);

  /* EPERM when the caller already leads a session, which is fine: it then
   * already leads its own group. */
  if (!inherited) {
    (void)setpgid(0, 0);
  }
  printf("own group: %s\n", getpgrp() == getpid() ? "yes" : "no");
  if (inherited) {
    report("kill(0, 0)", kill(0, 0));
  }
  report("rt_sigqueueinfo(0)", queue(0));
  report("rt_sigqueueinfo(-pgrp)", queue(-getpgrp()));
  report("rt_sigqueueinfo(-1)", queue(-1));
  report("kill(INT_MIN)", kill(INT_MIN, SIGUSR1));
  report("kill(INT_MIN, SIGKILL)", kill(INT_MIN, SIGKILL));
  report("kill(0)", kill(0, SIGUSR1));
  report("kill(-pgrp)", kill(-getpgrp(), SIGUSR1));
  if (inherited) {
    report("kill(0, SIGKILL)", kill(0, SIGKILL));
  }
  if (broadcast) {
    report("kill(-1)", kill(-1, SIGUSR1));
  }
  return 0;
}
