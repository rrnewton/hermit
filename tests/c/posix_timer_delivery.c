/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/signalfd.h>
#include <time.h>
#include <unistd.h>

static int block_signal(int signo) {
  sigset_t set;
  if (sigemptyset(&set) != 0 || sigaddset(&set, signo) != 0 ||
      sigprocmask(SIG_BLOCK, &set, NULL) != 0) {
    perror("sigprocmask");
    return -1;
  }
  return 0;
}

static int create_and_arm(clockid_t clockid, int signo, uintptr_t value,
                          timer_t *timerid) {
  struct sigevent event;
  memset(&event, 0, sizeof(event));
  event.sigev_notify = SIGEV_SIGNAL;
  event.sigev_signo = signo;
  event.sigev_value.sival_ptr = (void *)value;
  if (timer_create(clockid, &event, timerid) != 0) {
    perror("timer_create");
    return -1;
  }

  struct itimerspec spec;
  memset(&spec, 0, sizeof(spec));
  spec.it_value.tv_nsec = 1000000;
  if (timer_settime(*timerid, 0, &spec, NULL) != 0) {
    perror("timer_settime");
    timer_delete(*timerid);
    return -1;
  }
  return 0;
}

static int run_sigtimedwait(void) {
  const int signo = SIGUSR1;
  const uintptr_t value = 0x1234;
  if (block_signal(signo) != 0) {
    return 1;
  }

  timer_t timerid;
  if (create_and_arm(CLOCK_MONOTONIC, signo, value, &timerid) != 0) {
    return 2;
  }

  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, signo);
  struct timespec timeout = {.tv_sec = 0, .tv_nsec = 100000000};
  siginfo_t info;
  memset(&info, 0, sizeof(info));
  int received = sigtimedwait(&set, &info, &timeout);
  if (received != signo) {
    fprintf(stderr, "sigtimedwait=%d errno=%d\n", received, errno);
    timer_delete(timerid);
    return 3;
  }
  if (info.si_code != SI_TIMER ||
      (uintptr_t)info.si_value.sival_ptr != value || info.si_timerid != 0) {
    fprintf(stderr,
            "bad siginfo code=%d timerid=%d value=%#lx expected=%#lx\n",
            info.si_code, info.si_timerid,
            (unsigned long)(uintptr_t)info.si_value.sival_ptr,
            (unsigned long)value);
    timer_delete(timerid);
    return 4;
  }
  if (timer_delete(timerid) != 0) {
    perror("timer_delete");
    return 5;
  }

  puts("posix timer reached sigtimedwait with SI_TIMER metadata");
  return 0;
}

static int run_signalfd(void) {
  const int signo = SIGUSR2;
  const uintptr_t value = 0x5678;
  if (block_signal(signo) != 0) {
    return 1;
  }

  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, signo);
  int fd = signalfd(-1, &set, SFD_CLOEXEC | SFD_NONBLOCK);
  if (fd < 0) {
    perror("signalfd");
    return 2;
  }

  timer_t timerid;
  if (create_and_arm(CLOCK_REALTIME, signo, value, &timerid) != 0) {
    close(fd);
    return 3;
  }

  struct timespec delay = {.tv_sec = 0, .tv_nsec = 10000000};
  if (nanosleep(&delay, NULL) != 0) {
    perror("nanosleep");
    timer_delete(timerid);
    close(fd);
    return 4;
  }

  struct signalfd_siginfo info;
  memset(&info, 0, sizeof(info));
  ssize_t bytes = read(fd, &info, sizeof(info));
  if (bytes != (ssize_t)sizeof(info)) {
    fprintf(stderr, "signalfd read=%zd errno=%d\n", bytes, errno);
    timer_delete(timerid);
    close(fd);
    return 5;
  }
  if (info.ssi_signo != (uint32_t)signo || info.ssi_code != SI_TIMER ||
      info.ssi_tid != 0 || info.ssi_ptr != value) {
    fprintf(stderr,
            "bad signalfd info signo=%u code=%d timerid=%u value=%#llx\n",
            info.ssi_signo, info.ssi_code, info.ssi_tid,
            (unsigned long long)info.ssi_ptr);
    timer_delete(timerid);
    close(fd);
    return 6;
  }
  if (timer_delete(timerid) != 0) {
    perror("timer_delete");
    close(fd);
    return 7;
  }
  close(fd);

  puts("posix timer reached signalfd with SI_TIMER metadata");
  return 0;
}

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s sigtimedwait|signalfd\n", argv[0]);
    return 64;
  }
  if (strcmp(argv[1], "sigtimedwait") == 0) {
    return run_sigtimedwait();
  }
  if (strcmp(argv[1], "signalfd") == 0) {
    return run_signalfd();
  }
  fprintf(stderr, "unknown scenario: %s\n", argv[1]);
  return 64;
}
