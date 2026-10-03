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

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <unistd.h>
#include <utime.h>

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

// Makes a raw syscall with the stack pointer at `sp` and returns its result.
// The kernel itself does not touch the stack.
static long syscall_at(char* sp, long nr, long a1, long a2, long a3, long a4) {
  register long r10 __asm__("r10") = a4;
  long ret;
  __asm__ volatile(
      "mov %%rsp, %%r12\n\t"
      "mov %[sp], %%rsp\n\t"
      "syscall\n\t"
      "mov %%r12, %%rsp"
      : "=a"(ret)
      : "0"(nr),
        "D"(a1),
        "S"(a2),
        "d"(a3),
        "r"(r10),
        [sp] "r"(sp)
      : "rcx", "r11", "r12", "memory");
  return ret;
}

// Maps a two-page stack whose lower page is unmapped when `guard` is set.
static char* map_stack(long page, int guard) {
  char* map =
      mmap(NULL, 2 * page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (map == MAP_FAILED || (guard && munmap(map, page) != 0)) {
    perror("stack");
    exit(2);
  }
  return map;
}

// Makes a raw syscall with the stack pointer 192 bytes above an unmapped page.
// Below the 128-byte red zone that leaves Hermit 64 bytes of scratch space:
// enough to stage the 32-byte times of raw utime and utimes(NULL), as before
// the virtual mtime update existed, but not enough for the update's stat
// buffer.
static long syscall_on_short_stack(long nr, long a1, long a2, long a3, long a4) {
  long page = sysconf(_SC_PAGESIZE);
  char* map = map_stack(page, 1);
  long ret = syscall_at(map + page + 192, nr, a1, a2, a3, a4);
  munmap(map + page, page);
  return ret;
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

  // The raw utime and utimes syscalls reach Hermit's own handlers for them;
  // glibc routes its utime() and utimes() through utimensat instead.
  struct utimbuf by_utime = {.actime = EARLY, .modtime = EARLY + 3};
  if (syscall(SYS_utime, "a", &by_utime) != 0) {
    perror("raw utime");
    return 2;
  }
  expect_mtime("raw utime", "a", EARLY + 3, 0);
  struct timeval by_utimes[2] = {{EARLY, 0}, {EARLY + 4, 9}};
  if (syscall(SYS_utimes, "b", by_utimes) != 0) {
    perror("raw utimes");
    return 2;
  }
  expect_mtime("raw utimes", "b", EARLY + 4, 9000);
  // A NULL times sets the current time, later than both values above both
  // natively and under Hermit's default 2026 epoch.
  if (syscall(SYS_utime, "a", NULL) != 0 || syscall(SYS_utimes, "b", NULL) != 0) {
    perror("raw utime/utimes NULL");
    return 2;
  }
  if (mtime_of("a").tv_sec <= EARLY + 4 || mtime_of("b").tv_sec <= EARLY + 4) {
    fprintf(
        stderr,
        "raw utime/utimes(NULL) set mtimes %ld and %ld, not the current time\n",
        (long)mtime_of("a").tv_sec,
        (long)mtime_of("b").tv_sec);
    failures++;
  }

  // Hermit's bookkeeping must not keep a call from reaching Linux when the
  // guest's stack has no room for it.
  struct timespec short_stack[2] = {{0, UTIME_OMIT}, {EARLY + 6, 13}};
  long ret = syscall_on_short_stack(
      SYS_utimensat, AT_FDCWD, (long)"a", (long)short_stack, 0);
  if (ret != 0) {
    fprintf(stderr, "utimensat on a short stack returned %ld\n", ret);
    failures++;
  }
  struct utimbuf short_utime = {.actime = EARLY, .modtime = EARLY + 6};
  ret = syscall_on_short_stack(SYS_utime, (long)"a", (long)&short_utime, 0, 0);
  if (ret != 0) {
    fprintf(stderr, "raw utime on a short stack returned %ld\n", ret);
    failures++;
  }
  ret = syscall_on_short_stack(SYS_utimes, (long)"b", 0, 0, 0);
  if (ret != 0) {
    fprintf(stderr, "raw utimes(NULL) on a short stack returned %ld\n", ret);
    failures++;
  }

  // A raw syscall may keep its arguments below its stack pointer, in the
  // bytes from 128 to 272 below it where Hermit places its stat buffer.
  // Hermit must not overwrite them before Linux reads them. The path case
  // fails with ENOENT if Hermit clears the path. The times case has an
  // invalid atime and an omitted mtime, which ends just inside the red zone;
  // it must fail with EINVAL rather than succeed with a cleared atime.
  long page = sysconf(_SC_PAGESIZE);
  char* stack = map_stack(page, 0);
  char* sp = stack + page;
  strcpy(sp - 256, "a");
  struct timespec atime_only[2] = {{EARLY, 0}, {0, UTIME_OMIT}};
  ret = syscall_at(sp, SYS_utimensat, AT_FDCWD, (long)(sp - 256), (long)atime_only, 0);
  if (ret != 0) {
    fprintf(stderr, "utimensat with its path below the stack returned %ld\n", ret);
    failures++;
  }
  struct timespec* invalid = (struct timespec*)(sp - 152);
  invalid[0] = (struct timespec){EARLY, 1000000000};
  invalid[1] = (struct timespec){0, UTIME_OMIT};
  ret = syscall_at(sp, SYS_utimensat, AT_FDCWD, (long)"b", (long)invalid, 0);
  if (ret != -EINVAL) {
    fprintf(stderr, "invalid utimensat times below the stack returned %ld\n", ret);
    failures++;
  }
  munmap(stack, 2 * page);

  // A later write still moves the mtime off the explicitly set value.
  set_mtime("a", EARLY + 5, 11);
  expect_mtime("utimensat before write", "a", EARLY + 5, 11);
  write_file("a");
  struct timespec rewritten = mtime_of("a");
  if (rewritten.tv_sec == EARLY + 5 && rewritten.tv_nsec == 11) {
    fprintf(stderr, "write after utimensat left the mtime unchanged\n");
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
