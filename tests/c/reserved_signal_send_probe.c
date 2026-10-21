/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Sends and armings of the signals Hermit's in-guest runtime keeps for itself
 * (SIGSYS, SIGSEGV, SIGTRAP), A5 step 3c slice 0a
 * (https://github.com/rrnewton/hermit/issues/4485). Run by
 * hermit-cli/tests/common/liteinst_in_guest_programs.rs; never run it
 * natively in the group modes, which signal the caller's process group.
 *
 *   sends     For each send call and each of the three signals: fork a child
 *             that waits on a pipe and exits 0, send it the signal, close the
 *             pipe, and print the call's result and how the child ended.
 *             Linux (and Hermit's ptrace backend) kills the child; the
 *             in-guest backends refuse with EPERM and the child exits 0.
 *   armings   Arm each signal as a notification signal (fcntl F_SETSIG,
 *             timer_create SIGEV_SIGNAL, prctl PR_SET_PDEATHSIG), then disarm
 *             (F_SETSIG 0, PR_SET_PDEATHSIG 0), printing each result.
 *   group-kill0, group-killpgid, group-pidfd
 *             Lead a process group of its own, then send SIGSYS to it by
 *             kill(0), kill(-pgrp) or pidfd_send_signal with
 *             PIDFD_SIGNAL_PROCESS_GROUP, which Hermit refuses by name before
 *             any reserved-signal rule applies.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef PIDFD_SIGNAL_PROCESS_GROUP
#define PIDFD_SIGNAL_PROCESS_GROUP (1U << 2)
#endif

static const int SIGNALS[] = {SIGSYS, SIGSEGV, SIGTRAP};

static const char *signal_name(int signal) {
  switch (signal) {
    case SIGSYS:
      return "SIGSYS";
    case SIGSEGV:
      return "SIGSEGV";
    case SIGTRAP:
      return "SIGTRAP";
    default:
      return "other";
  }
}

static const char *errno_name(int error) {
  switch (error) {
    case 0:
      return "0";
    case EPERM:
      return "EPERM";
    case ESRCH:
      return "ESRCH";
    case EINVAL:
      return "EINVAL";
    case ENOSYS:
      return "ENOSYS";
    default:
      return "other";
  }
}

static long send_with(const char *call, pid_t child, int signal) {
  if (strcmp(call, "kill") == 0) {
    return kill(child, signal);
  }
  if (strcmp(call, "tkill") == 0) {
    return syscall(SYS_tkill, child, signal);
  }
  if (strcmp(call, "tgkill") == 0) {
    return syscall(SYS_tgkill, child, child, signal);
  }
  if (strcmp(call, "rt_sigqueueinfo") == 0 ||
      strcmp(call, "rt_tgsigqueueinfo") == 0) {
    siginfo_t info;
    memset(&info, 0, sizeof(info));
    info.si_signo = signal;
    info.si_code = SI_QUEUE;
    info.si_pid = getpid();
    info.si_uid = getuid();
    if (call[3] == 's') {
      return syscall(SYS_rt_sigqueueinfo, child, signal, &info);
    }
    return syscall(SYS_rt_tgsigqueueinfo, child, child, signal, &info);
  }
  /* pidfd_send_signal */
  long pidfd = syscall(SYS_pidfd_open, child, 0);
  if (pidfd < 0) {
    printf("pidfd_open failed: %s\n", errno_name(errno));
    return -1;
  }
  long result = syscall(SYS_pidfd_send_signal, (int)pidfd, signal, NULL, 0);
  int saved = errno;
  close((int)pidfd);
  errno = saved;
  return result;
}

static int sends(void) {
  static const char *const CALLS[] = {
      "kill",
      "tkill",
      "tgkill",
      "rt_sigqueueinfo",
      "rt_tgsigqueueinfo",
      "pidfd_send_signal",
  };
  for (size_t c = 0; c < sizeof(CALLS) / sizeof(CALLS[0]); c++) {
    for (size_t s = 0; s < sizeof(SIGNALS) / sizeof(SIGNALS[0]); s++) {
      int signal = SIGNALS[s];
      int fds[2];
      if (pipe(fds) != 0) {
        perror("pipe");
        return 1;
      }
      pid_t child = fork();
      if (child < 0) {
        perror("fork");
        return 1;
      }
      if (child == 0) {
        char byte;
        close(fds[1]);
        /* EOF when the parent closes its end, after its send. */
        while (read(fds[0], &byte, 1) > 0) {
        }
        _exit(0);
      }
      close(fds[0]);
      errno = 0;
      long result = send_with(CALLS[c], child, signal);
      int error = result < 0 ? errno : 0;
      close(fds[1]);
      int status = 0;
      if (waitpid(child, &status, 0) != child) {
        perror("waitpid");
        return 1;
      }
      char ended[32];
      if (WIFEXITED(status)) {
        snprintf(ended, sizeof(ended), "exit %d", WEXITSTATUS(status));
      } else if (WIFSIGNALED(status)) {
        snprintf(ended, sizeof(ended), "signal %d", WTERMSIG(status));
      } else {
        snprintf(ended, sizeof(ended), "status %#x", (unsigned)status);
      }
      printf("%s %s: rc=%ld errno=%s child=%s\n",
             CALLS[c],
             signal_name(signal),
             result,
             errno_name(error),
             ended);
    }
  }
  return 0;
}

static void print_result(const char *call, int signal, long result) {
  printf("%s %s: rc=%ld errno=%s\n",
         call,
         signal == 0 ? "0" : signal_name(signal),
         result,
         errno_name(result < 0 ? errno : 0));
}

static int armings(void) {
  int fds[2];
  if (pipe(fds) != 0) {
    perror("pipe");
    return 1;
  }
  for (size_t s = 0; s < sizeof(SIGNALS) / sizeof(SIGNALS[0]); s++) {
    int signal = SIGNALS[s];
    errno = 0;
    print_result("fcntl(F_SETSIG)", signal, fcntl(fds[0], F_SETSIG, signal));

    struct sigevent event;
    memset(&event, 0, sizeof(event));
    event.sigev_notify = SIGEV_SIGNAL;
    event.sigev_signo = signal;
    int timer = -1;
    errno = 0;
    long created = syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &timer);
    print_result("timer_create(SIGEV_SIGNAL)", signal, created);
    if (created == 0) {
      /* Created, never set: it never fires. */
      syscall(SYS_timer_delete, timer);
    }

    errno = 0;
    print_result("prctl(PR_SET_PDEATHSIG)",
                 signal,
                 prctl(PR_SET_PDEATHSIG, (unsigned long)signal, 0UL, 0UL, 0UL));
  }
  errno = 0;
  print_result("fcntl(F_SETSIG)", 0, fcntl(fds[0], F_SETSIG, 0));
  errno = 0;
  print_result("prctl(PR_SET_PDEATHSIG)",
               0,
               prctl(PR_SET_PDEATHSIG, 0UL, 0UL, 0UL, 0UL));
  int armed = -1;
  errno = 0;
  long got = prctl(PR_GET_PDEATHSIG, (unsigned long)&armed, 0UL, 0UL, 0UL);
  printf("prctl(PR_GET_PDEATHSIG): rc=%ld armed=%d\n", got, armed);
  close(fds[0]);
  close(fds[1]);
  return 0;
}

static int group(const char *mode) {
  /* Only this process is in the group it signals. */
  if (setpgid(0, 0) != 0) {
    perror("setpgid");
    return 1;
  }
  printf("own group: %s\n", getpgrp() == getpid() ? "yes" : "no");
  fflush(stdout);
  long result;
  errno = 0;
  if (strcmp(mode, "group-kill0") == 0) {
    result = kill(0, SIGSYS);
  } else if (strcmp(mode, "group-killpgid") == 0) {
    result = kill(-getpgrp(), SIGSYS);
  } else {
    long pidfd = syscall(SYS_pidfd_open, getpid(), 0);
    if (pidfd < 0) {
      printf("pidfd_open failed: %s\n", errno_name(errno));
      return 1;
    }
    result = syscall(SYS_pidfd_send_signal,
                     (int)pidfd,
                     SIGSYS,
                     NULL,
                     PIDFD_SIGNAL_PROCESS_GROUP);
  }
  /* A run that refuses the call stops before this line. */
  print_result(mode, SIGSYS, result);
  return 0;
}

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s sends|armings|group-kill0|group-killpgid|group-pidfd\n",
            argv[0]);
    return 2;
  }
  setvbuf(stdout, NULL, _IOLBF, 0);
  if (strcmp(argv[1], "sends") == 0) {
    return sends();
  }
  if (strcmp(argv[1], "armings") == 0) {
    return armings();
  }
  if (strncmp(argv[1], "group-", 6) == 0) {
    return group(argv[1]);
  }
  fprintf(stderr, "unknown mode %s\n", argv[1]);
  return 2;
}
