/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

static void read_clock(clockid_t id) {
  struct timespec ts = {-1, -1};
  assert(syscall(SYS_clock_gettime, id, &ts) == 0);
  assert(ts.tv_sec >= 0 && ts.tv_nsec >= 0 && ts.tv_nsec < 1000000000);
  printf("clock %d: %ld %ld\n", id, ts.tv_sec, ts.tv_nsec);
}

static void boundary_output(long number, int protection, size_t prefix) {
  long page = sysconf(_SC_PAGESIZE);
  assert(page > 16);
  unsigned char* pages = mmap(NULL, page * 2, PROT_READ | PROT_WRITE,
                              MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  assert(pages != MAP_FAILED);
  size_t size = number == SYS_time ? sizeof(time_t) : sizeof(struct timespec);
  unsigned char* output = pages + page - prefix;
  memset(output, 0x5a, size);
  assert(mprotect(pages + page, page, protection) == 0);
  errno = 0;
  long result;
  if (number == SYS_clock_gettime) {
    result = syscall(number, CLOCK_REALTIME, output);
  } else if (number == SYS_gettimeofday) {
    result = syscall(number, output, NULL);
  } else {
    result = syscall(number, output);
  }
  int error = errno;
  assert(mprotect(pages + page, page, PROT_READ | PROT_WRITE) == 0);
  if (protection != PROT_WRITE) {
    assert(result == -1 && error == EFAULT);
    for (size_t i = prefix; i < size; i++) {
      assert(output[i] == 0x5a);
    }
    // An eight-byte prefix is a full tv_sec, which Linux copied before EFAULT.
    if (prefix == 8) {
      assert(memcmp(output, "ZZZZZZZZ", 8) != 0);
    }
  } else {
    assert(result >= 0);
  }
  printf("boundary %ld prot=%d prefix=%zu result=%ld errno=%d bytes=", number,
         protection, prefix, result, error);
  for (size_t i = 0; i < size; i++) {
    printf("%02x", output[i]);
  }
  puts("");
  assert(munmap(pages, page * 2) == 0);
  read_clock(CLOCK_MONOTONIC);
}

static void timezone_outputs(void) {
  struct timeval tv = {-1, -1};
  errno = 0;
  assert(syscall(SYS_gettimeofday, &tv, (void*)1) == -1 && errno == EFAULT);
  assert(tv.tv_sec > 0 && tv.tv_usec >= 0);
  printf("timezone EFAULT timeval=%ld %ld\n", tv.tv_sec, tv.tv_usec);
  read_clock(CLOCK_REALTIME);

  struct timezone tz = {-1, -1};
  assert(syscall(SYS_gettimeofday, NULL, &tz) == 0);
  printf("timezone NULL timeval=%d %d\n", tz.tz_minuteswest, tz.tz_dsttime);
  // The timezone overlaps the second half of timeval; snapshots must retain
  // the final bytes, including this later kernel overwrite.
  assert(syscall(SYS_gettimeofday, &tv, &tv.tv_usec) == 0);
  printf("overlapping timezone=%ld %ld\n", tv.tv_sec, tv.tv_usec);
  assert(syscall(SYS_gettimeofday, &tv, &tv) == 0);
  printf("same-address timezone=%ld %ld\n", tv.tv_sec, tv.tv_usec);
  for (int protection = PROT_NONE; protection <= PROT_READ; protection++) {
    long page = sysconf(_SC_PAGESIZE);
    unsigned char* pages = mmap(NULL, page * 2, PROT_READ | PROT_WRITE,
                                MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    assert(pages != MAP_FAILED);
    unsigned char* output = pages + page - 4;
    memset(output, 0x5a, sizeof(struct timezone));
    assert(mprotect(pages + page, page, protection) == 0);
    tv = (struct timeval){-1, -1};
    errno = 0;
    assert(syscall(SYS_gettimeofday, &tv, output) == -1 && errno == EFAULT);
    assert(tv.tv_sec > 0 && tv.tv_usec >= 0);
    assert(mprotect(pages + page, page, PROT_READ | PROT_WRITE) == 0);
    printf("timezone boundary prot=%d tv=%ld %ld bytes=", protection, tv.tv_sec, tv.tv_usec);
    for (size_t i = 0; i < sizeof(struct timezone); i++) {
      printf("%02x", output[i]);
      if (i >= 4) {
        assert(output[i] == 0x5a);
      }
    }
    puts("");
    assert(munmap(pages, page * 2) == 0);
    read_clock(CLOCK_MONOTONIC);
  }
}

int main(int argc, char** argv) {
  if (argc == 2) {
    if (strcmp(argv[1], "clock") == 0) {
      read_clock(CLOCK_MONOTONIC);
    } else if (strcmp(argv[1], "timeofday") == 0) {
      struct timeval tv;
      assert(syscall(SYS_gettimeofday, &tv, NULL) == 0);
    } else if (strcmp(argv[1], "time") == 0) {
      assert(syscall(SYS_time, NULL) > 0);
    } else {
      assert(strcmp(argv[1], "resolution") == 0);
      struct timespec ts;
      assert(syscall(SYS_clock_getres, CLOCK_MONOTONIC, &ts) == 0);
    }
    puts("uncaptured-clock-completed");
    return 0;
  }
  assert(argc == 1);

  read_clock(CLOCK_MONOTONIC);
  read_clock(CLOCK_REALTIME);

  struct timespec ts = {-1, -1};
  errno = 0;
  assert(syscall(SYS_clock_gettime, -1, &ts) == -1 && errno == EINVAL);
  assert(ts.tv_sec == -1 && ts.tv_nsec == -1);
  read_clock(CLOCK_REALTIME);

  // Linux validates this clock ID before copyout. Replay must consume EINVAL,
  // rather than substituting EFAULT for the NULL pointer or leaving the event.
  errno = 0;
  assert(syscall(SYS_clock_gettime, -1, NULL) == -1 && errno == EINVAL);
  read_clock(CLOCK_MONOTONIC);
  errno = 0;
  assert(syscall(SYS_clock_gettime, CLOCK_MONOTONIC, NULL) == -1 && errno == EFAULT);
  read_clock(CLOCK_REALTIME);
  errno = 0;
  assert(syscall(SYS_clock_gettime, CLOCK_MONOTONIC, (void*)1) == -1 && errno == EFAULT);
  read_clock(CLOCK_MONOTONIC);

  struct timeval tv;
  struct timezone tz;
  assert(syscall(SYS_gettimeofday, &tv, &tz) == 0);
  printf("timeofday: %ld %ld %d %d\n", tv.tv_sec, tv.tv_usec, tz.tz_minuteswest, tz.tz_dsttime);
  assert(syscall(SYS_gettimeofday, NULL, NULL) == 0);
  errno = 0;
  assert(syscall(SYS_gettimeofday, (void*)1, NULL) == -1 && errno == EFAULT);
  assert(syscall(SYS_gettimeofday, &tv, NULL) == 0);
  printf("timeofday after errno: %ld %ld\n", tv.tv_sec, tv.tv_usec);

  time_t seconds = -1;
  long result = syscall(SYS_time, &seconds);
  assert(result > 0 && result == seconds);
  printf("time: %ld %ld\n", result, seconds);
  result = syscall(SYS_time, NULL);
  assert(result > 0);
  printf("time NULL: %ld\n", result);
  errno = 0;
  assert(syscall(SYS_time, (void*)1) == -1 && errno == EFAULT);
  result = syscall(SYS_time, &seconds);
  assert(result > 0 && result == seconds);
  printf("time after errno: %ld %ld\n", result, seconds);
  timezone_outputs();
  for (int protection = PROT_NONE; protection <= PROT_WRITE; protection++) {
    boundary_output(SYS_clock_gettime, protection, 4);
    boundary_output(SYS_clock_gettime, protection, 8);
    boundary_output(SYS_gettimeofday, protection, 4);
    boundary_output(SYS_gettimeofday, protection, 8);
    boundary_output(SYS_time, protection, 4);
  }
  puts("captured-clock-errors-and-values-ok");
  return 0;
}
