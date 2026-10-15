/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Guest for hermit-cli/tests/procfs_determinism.rs: a /proc name outside the
 * run's --proc level, reached relative to a descriptor for /proc that was
 * itself opened through a descriptor link (/proc/self/fd/N, /dev/fd/N). Linux
 * follows the link to /proc, so the name is /proc/kallsyms, which
 * --proc=minimal omits: the open fails with ENOENT, as the open of
 * /proc/kallsyms does. Prints one line per spelling and exits 0 only if every
 * open failed with ENOENT. Natively /proc/kallsyms exists, so it exits 1. */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

int main(void) {
  int proc = open("/proc", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  if (proc < 0) {
    fprintf(stderr, "open(/proc): %s\n", strerror(errno));
    return 2;
  }
  int failures = 0;
  const char* formats[] = {"/proc/self/fd/%d", "/dev/fd/%d"};
  for (size_t i = 0; i < sizeof(formats) / sizeof(formats[0]); i++) {
    char spelling[64];
    snprintf(spelling, sizeof(spelling), formats[i], proc);
    int directory = open(spelling, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (directory < 0) {
      fprintf(stderr, "open(%s): %s\n", spelling, strerror(errno));
      return 2;
    }
    int fd = openat(directory, "kallsyms", O_RDONLY | O_CLOEXEC);
    int error = fd < 0 ? errno : 0;
    printf("openat(%s, \"kallsyms\"): %s\n", spelling, fd < 0 ? strerror(error) : "opened");
    failures += !(fd < 0 && error == ENOENT);
    if (fd >= 0) {
      close(fd);
    }
    close(directory);
  }
  close(proc);
  return failures == 0 ? 0 : 1;
}
