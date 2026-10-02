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
 *
 * The error cases at the end check that a failed read is replayed with the
 * recorded errno and consumes its recorded event: a replayer that answered
 * one of them without consuming the event would hand that event to the next
 * call and print a different errno or value.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

/* Prints the errno of a clock_gettime that the kernel must refuse. */
static int expect_clock_gettime_error(const char* label, clockid_t clock,
                                      struct timespec* tp) {
  long ret = syscall(SYS_clock_gettime, clock, tp);
  if (ret != -1) {
    fprintf(stderr, "clock_gettime %s unexpectedly returned %ld\n", label, ret);
    return -1;
  }
  printf("clock_gettime %s errno=%d\n", label, errno);
  return 0;
}

int main(void) {
  for (int round = 0; round < 3; ++round) {
    struct timeval tv;
    struct timezone tz;
    struct timeval tv_only;
    if (syscall(SYS_gettimeofday, &tv, &tz) != 0 ||
        syscall(SYS_gettimeofday, &tv_only, NULL) != 0) {
      perror("gettimeofday");
      return EXIT_FAILURE;
    }
#ifdef SYS_time
    time_t via_arg = 0;
    long via_ret = syscall(SYS_time, &via_arg);
    if (via_ret == -1 || via_ret != via_arg) {
      perror("time");
      return EXIT_FAILURE;
    }
#else
    long via_ret = (long)tv.tv_sec;
#endif
    struct timespec realtime;
    struct timespec monotonic;
    struct timespec cputime;
    if (syscall(SYS_clock_gettime, CLOCK_REALTIME, &realtime) != 0 ||
        syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &monotonic) != 0 ||
        syscall(SYS_clock_gettime, CLOCK_PROCESS_CPUTIME_ID, &cputime) != 0) {
      perror("clock_gettime");
      return EXIT_FAILURE;
    }
    printf(
        "round %d gettimeofday=%lld.%06lld tz=%d/%d tv_only=%lld.%06lld time=%ld "
        "realtime=%lld.%09ld monotonic=%lld.%09ld cputime=%lld.%09ld\n",
        round,
        (long long)tv.tv_sec,
        (long long)tv.tv_usec,
        tz.tz_minuteswest,
        tz.tz_dsttime,
        (long long)tv_only.tv_sec,
        (long long)tv_only.tv_usec,
        via_ret,
        (long long)realtime.tv_sec,
        realtime.tv_nsec,
        (long long)monotonic.tv_sec,
        monotonic.tv_nsec,
        (long long)cputime.tv_sec,
        cputime.tv_nsec);
  }

  struct timespec valid;
  if (expect_clock_gettime_error("monotonic-null", CLOCK_MONOTONIC, NULL) ||
      expect_clock_gettime_error("invalid-null", (clockid_t)99, NULL) ||
      expect_clock_gettime_error("invalid-valid", (clockid_t)99, &valid)) {
    return EXIT_FAILURE;
  }
  if (syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &valid) != 0) {
    perror("clock_gettime after errors");
    return EXIT_FAILURE;
  }
  printf("after errors monotonic=%lld.%09ld\n", (long long)valid.tv_sec,
         valid.tv_nsec);

  struct timespec resolution;
  if (syscall(SYS_clock_getres, CLOCK_MONOTONIC, &resolution) != 0) {
    perror("clock_getres");
    return EXIT_FAILURE;
  }
  printf("clock_getres monotonic=%lld.%09ld\n", (long long)resolution.tv_sec,
         resolution.tv_nsec);
  return EXIT_SUCCESS;
}
