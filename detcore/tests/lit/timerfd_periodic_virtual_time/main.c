/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// RUN: %me | FileCheck %s
// CHECK: empty_nonblock: r=-1 errno=11
// CHECK: gettime_interval_ns=30000000
// CHECK: periodic_read: r=8 count=3
// CHECK: disarmed_nonblock: r=-1 errno=11
// CHECK: short_read: r=-1 errno=22
// CHECK: done

#define _GNU_SOURCE
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/timerfd.h>
#include <time.h>
#include <unistd.h>

/* Periodic virtual timerfd across a nanosleep: the guest sleeps 100ms of
 * virtual time with a 30ms periodic timerfd armed, so exactly three
 * expirations (30/60/90ms) must be pending at the read. This exercises the
 * scheduler empty-queue fast-forward over periodic TimerFdExpiry re-arms; a
 * regression there hung this guest (scheduler panic on re-arm overflow). */
int main(void) {
  int tfd = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK);
  if (tfd < 0) {
    perror("timerfd_create");
    return 1;
  }
  uint64_t v = 0;
  ssize_t r = read(tfd, &v, sizeof v);
  printf("empty_nonblock: r=%zd errno=%d\n", r, errno);
  struct itimerspec its = {0};
  its.it_value.tv_nsec = 30 * 1000 * 1000;
  its.it_interval.tv_nsec = 30 * 1000 * 1000;
  if (timerfd_settime(tfd, 0, &its, NULL) != 0) {
    perror("timerfd_settime");
    return 1;
  }
  struct timespec ts = {0, 100 * 1000 * 1000};
  nanosleep(&ts, NULL);
  struct itimerspec cur;
  timerfd_gettime(tfd, &cur);
  printf("gettime_interval_ns=%ld\n", (long)cur.it_interval.tv_nsec);
  r = read(tfd, &v, sizeof v);
  printf("periodic_read: r=%zd count=%llu\n", r, (unsigned long long)v);
  struct itimerspec dis = {0};
  timerfd_settime(tfd, 0, &dis, NULL);
  r = read(tfd, &v, sizeof v);
  printf("disarmed_nonblock: r=%zd errno=%d\n", r, errno);
  char small[4];
  r = read(tfd, small, 4);
  printf("short_read: r=%zd errno=%d\n", r, errno);
  close(tfd);
  printf("done\n");
  return 0;
}
