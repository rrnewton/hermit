/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Reports what five operations on /proc/stat return, one line each:
//
//   read            read(2) of 64 bytes from a fresh descriptor
//   lseek           lseek(2) by 0 bytes from the current position
//   read            a second read(2) of 64 bytes on the same descriptor
//   pread64         pread64(2) of 64 bytes at offset 0 on that descriptor
//   fresh-pread64   pread64(2) of 64 bytes at offset 0 on a second descriptor
//
// Each line is "<operation> result=<r> errno=<e>", where <e> is EOVERFLOW, 0
// when the call succeeded, or the decimal errno otherwise. Each read also
// reports whether its buffer, filled with 0xa5 beforehand, is "unchanged" or
// was "written". Finally the program reads the whole file from a third
// descriptor and prints its btime line, or "whole-file result=-1 errno=<e>"
// when that read fails.
//
// hermit-cli/tests/procfs_determinism.rs runs this at the first uptime offset
// whose boot instant no time64_t can hold, and at the last one that it can.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/types.h>
#include <unistd.h>

#define SENTINEL 0xa5

static unsigned char buffer[64];
static char contents[1 << 20];

static int open_stat(void) {
  int fd = open("/proc/stat", O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    perror("open /proc/stat");
    _exit(2);
  }
  return fd;
}

static void name_errno(char *name, size_t size, long result, int error) {
  if (result >= 0) {
    snprintf(name, size, "0");
  } else if (error == EOVERFLOW) {
    snprintf(name, size, "EOVERFLOW");
  } else {
    snprintf(name, size, "%d", error);
  }
}

static void fill_buffer(void) {
  memset(buffer, SENTINEL, sizeof(buffer));
  errno = 0;
}

static void report_read(const char *operation, ssize_t result, int error) {
  int unchanged = 1;
  for (size_t index = 0; index < sizeof(buffer); index++) {
    if (buffer[index] != SENTINEL) {
      unchanged = 0;
    }
  }
  char name[32];
  name_errno(name, sizeof(name), (long)result, error);
  printf(
      "%s result=%zd errno=%s buffer=%s\n",
      operation,
      result,
      name,
      unchanged ? "unchanged" : "written");
}

static void report_whole_file(int fd) {
  size_t length = 0;
  for (;;) {
    if (length == sizeof(contents) - 1) {
      printf("whole-file result=-1 errno=%d\n", EFBIG);
      return;
    }
    errno = 0;
    ssize_t count = read(fd, contents + length, sizeof(contents) - 1 - length);
    if (count < 0) {
      char name[32];
      name_errno(name, sizeof(name), -1, errno);
      printf("whole-file result=-1 errno=%s\n", name);
      return;
    }
    if (count == 0) {
      break;
    }
    length += (size_t)count;
  }
  contents[length] = '\0';
  for (char *line = contents; *line != '\0';) {
    char *newline = strchr(line, '\n');
    if (strncmp(line, "btime ", strlen("btime ")) == 0) {
      if (newline != NULL) {
        newline[1] = '\0';
      }
      fputs(line, stdout);
      return;
    }
    if (newline == NULL) {
      break;
    }
    line = newline + 1;
  }
  printf("whole-file has no btime line\n");
}

int main(void) {
  int fd = open_stat();

  fill_buffer();
  ssize_t result = read(fd, buffer, sizeof(buffer));
  report_read("read", result, errno);

  errno = 0;
  off_t position = lseek(fd, 0, SEEK_CUR);
  int error = errno;
  char name[32];
  name_errno(name, sizeof(name), (long)position, error);
  printf("lseek result=%lld errno=%s\n", (long long)position, name);

  fill_buffer();
  result = read(fd, buffer, sizeof(buffer));
  report_read("read", result, errno);

  fill_buffer();
  result = pread(fd, buffer, sizeof(buffer), 0);
  report_read("pread64", result, errno);

  int fresh = open_stat();
  fill_buffer();
  result = pread(fresh, buffer, sizeof(buffer), 0);
  report_read("fresh-pread64", result, errno);

  int whole = open_stat();
  report_whole_file(whole);

  if (close(whole) != 0 || close(fresh) != 0 || close(fd) != 0) {
    perror("close /proc/stat");
    return 2;
  }
  return fflush(stdout) == 0 ? 0 : 2;
}
