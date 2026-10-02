/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// An explicit mtime set through utimensat, futimens or utimes must be the
// mtime that stat reports afterwards, as on Linux. `tar` extraction, `cp -p`
// and `touch -r` depend on it, and so does `make`, which otherwise sees source
// files ordered by when they were unpacked.

#define _GNU_SOURCE

#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <unistd.h>

// 2030-01-01T00:00:00Z and 2020-01-01T00:00:00Z.
#define LATE 1893456000L
#define EARLY 1577836800L

static int failures = 0;

static void write_file(const char* path) {
  int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
  if (fd < 0 || write(fd, "x", 1) != 1 || close(fd) != 0) {
    perror(path);
    exit(2);
  }
}

static struct timespec mtime_of(const char* path) {
  struct stat st;
  if (stat(path, &st) != 0) {
    perror(path);
    exit(2);
  }
  return st.st_mtim;
}

static void expect_mtime(const char* what, const char* path, long sec, long nsec) {
  struct timespec got = mtime_of(path);
  if (got.tv_sec != sec || got.tv_nsec != nsec) {
    fprintf(
        stderr,
        "%s: %s has mtime %ld.%09ld, expected %ld.%09ld\n",
        what,
        path,
        (long)got.tv_sec,
        (long)got.tv_nsec,
        sec,
        nsec);
    failures++;
  }
}

static void set_mtime(const char* path, long sec, long nsec) {
  struct timespec times[2] = {{0, UTIME_OMIT}, {sec, nsec}};
  if (utimensat(AT_FDCWD, path, times, 0) != 0) {
    perror("utimensat");
    exit(2);
  }
}

int main(void) {
  char dir[] = "/tmp/utimensat-mtime-XXXXXX";
  if (mkdtemp(dir) == NULL || chdir(dir) != 0) {
    perror("mkdtemp");
    return 2;
  }

  // Written in this order, so before any utimensat "a" is not newer than "b".
  write_file("a");
  write_file("b");

  // Reverse the write order, as tar does when it restores archived mtimes.
  set_mtime("a", LATE, 123456789);
  set_mtime("b", EARLY, 0);
  expect_mtime("utimensat", "a", LATE, 123456789);
  expect_mtime("utimensat", "b", EARLY, 0);

  // UTIME_OMIT for the mtime leaves it alone.
  struct timespec omit[2] = {{EARLY, 0}, {0, UTIME_OMIT}};
  if (utimensat(AT_FDCWD, "a", omit, 0) != 0) {
    perror("utimensat omit");
    return 2;
  }
  expect_mtime("UTIME_OMIT", "a", LATE, 123456789);

  // `touch -r a b`: copy one file's mtime onto another.
  struct timespec reference[2] = {{0, UTIME_OMIT}, mtime_of("a")};
  if (utimensat(AT_FDCWD, "b", reference, 0) != 0) {
    perror("utimensat reference");
    return 2;
  }
  expect_mtime("touch -r", "b", LATE, 123456789);

  // futimens names the file by descriptor (a NULL path to the syscall).
  int fd = open("b", O_RDONLY);
  struct timespec by_fd[2] = {{0, UTIME_OMIT}, {EARLY + 1, 5}};
  if (fd < 0 || futimens(fd, by_fd) != 0 || close(fd) != 0) {
    perror("futimens");
    return 2;
  }
  expect_mtime("futimens", "b", EARLY + 1, 5);

  // utimes, the microsecond interface, is routed through utimensat.
  struct timeval tv[2] = {{EARLY, 0}, {EARLY + 2, 7}};
  if (utimes("a", tv) != 0) {
    perror("utimes");
    return 2;
  }
  expect_mtime("utimes", "a", EARLY + 2, 7000);

  // A later write still moves the mtime off the explicitly set value.
  write_file("a");
  struct timespec rewritten = mtime_of("a");
  if (rewritten.tv_sec == EARLY + 2 && rewritten.tv_nsec == 7000) {
    fprintf(stderr, "write after utimes left the mtime unchanged\n");
    failures++;
  }

  unlink("a");
  unlink("b");
  if (chdir("/") != 0 || rmdir(dir) != 0) {
    perror("cleanup");
    return 2;
  }
  if (failures != 0) {
    return 1;
  }
  puts("explicit mtimes honored");
  return 0;
}
