/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

extern char** environ;

static void fail(const char* operation) {
  fprintf(stderr, "%s failed: %s\n", operation, strerror(errno));
  exit(1);
}

static void exec_fd_alias(const char* executable, const char* prefix, const char* phase) {
  int fd = open(executable, O_PATH);
  if (fd < 0) {
    fail("open executable alias");
  }
  char path[PATH_MAX];
  if (snprintf(path, sizeof(path), "%s/%d", prefix, fd) >= (int)sizeof(path)) {
    fprintf(stderr, "descriptor alias path overflow\n");
    exit(1);
  }
  char* next[] = {(char*)executable, (char*)phase, NULL};
  execve(path, next, environ);
  fail("execve descriptor alias");
}

int main(int argc, char** argv) {
  char executable[PATH_MAX];
  if (realpath(argv[0], executable) == NULL) {
    fail("realpath executable");
  }
  const char* phase = argc > 1 ? argv[1] : "empty";

  if (strcmp(phase, "empty") == 0) {
    const char* link = "/tmp/hermit-record-replay-execveat-link";
    unlink(link);
    if (symlink(executable, link) != 0) {
      fail("symlink nofollow fixture");
    }
    char* unused[] = {executable, "unexpected", NULL};
    errno = 0;
    long result = syscall(
        SYS_execveat, AT_FDCWD, link, unused, environ, AT_SYMLINK_NOFOLLOW);
    if (result != -1 || errno != ELOOP) {
      fprintf(stderr, "AT_SYMLINK_NOFOLLOW returned %ld errno=%d\n", result, errno);
      return 1;
    }
    unlink(link);

    int fd = open(executable, O_PATH);
    if (fd < 0) {
      fail("open AT_EMPTY_PATH executable");
    }
    char* next[] = {executable, "dirfd", NULL};
    syscall(SYS_execveat, fd, "", next, environ, AT_EMPTY_PATH);
    fail("AT_EMPTY_PATH execveat");
  }

  if (strcmp(phase, "dirfd") == 0) {
    char directory[PATH_MAX];
    char* slash = strrchr(executable, '/');
    if (slash == NULL) {
      fprintf(stderr, "absolute executable has no slash\n");
      return 1;
    }
    size_t directory_length = (size_t)(slash - executable);
    memcpy(directory, executable, directory_length);
    directory[directory_length] = '\0';
    int dirfd = open(directory, O_PATH | O_DIRECTORY);
    if (dirfd < 0) {
      fail("open executable directory");
    }
    char* next[] = {executable, "procfd", NULL};
    syscall(SYS_execveat, dirfd, slash + 1, next, environ, 0);
    fail("dirfd-relative execveat");
  }

  if (strcmp(phase, "procfd") == 0) {
    exec_fd_alias(executable, "/proc/self/fd", "devfd");
  }

  if (strcmp(phase, "devfd") == 0) {
    exec_fd_alias(executable, "/dev/fd", "done");
  }

  if (strcmp(phase, "done") == 0) {
    puts("execveat and descriptor aliases preserved");
    return 0;
  }

  fprintf(stderr, "unknown phase: %s\n", phase);
  return 1;
}
