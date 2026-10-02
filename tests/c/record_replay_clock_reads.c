/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Record/replay guest for the clock reads that reach Detcore as syscalls.
 *
 * Record mode leaves time real, so each value printed here is host time when
 * recorded. Replay must return the recorded values, which makes stdout
 * identical only if every read below was captured by the recorder and served
 * by the replayer. The raw syscall() form bypasses the vDSO so the calls reach
 * the gettimeofday, time and clock_gettime dispatch in Detcore.
 */

#define _GNU_SOURCE

#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

int main(void) {
  for (int round = 0; round < 3; ++round) {
    struct timeval tv;
    if (syscall(SYS_gettimeofday, &tv, NULL) != 0) {
      perror("gettimeofday");
      return EXIT_FAILURE;
    }
    time_t via_arg = 0;
    long via_ret = syscall(SYS_time, &via_arg);
    if (via_ret == -1 || via_ret != via_arg) {
      perror("time");
      return EXIT_FAILURE;
    }
    struct timespec realtime;
    struct timespec monotonic;
    if (syscall(SYS_clock_gettime, CLOCK_REALTIME, &realtime) != 0 ||
        syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &monotonic) != 0) {
      perror("clock_gettime");
      return EXIT_FAILURE;
    }
    printf(
        "round %d gettimeofday=%lld.%06lld time=%ld realtime=%lld.%09ld "
        "monotonic=%lld.%09ld\n",
        round,
        (long long)tv.tv_sec,
        (long long)tv.tv_usec,
        via_ret,
        (long long)realtime.tv_sec,
        realtime.tv_nsec,
        (long long)monotonic.tv_sec,
        monotonic.tv_nsec);
  }
  return EXIT_SUCCESS;
}
