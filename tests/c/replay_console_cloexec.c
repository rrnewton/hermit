/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static void fail(const char *operation) {
  fprintf(stderr, "%s: %s\n", operation, strerror(errno));
  exit(EXIT_FAILURE);
}

int main(int argc, char **argv) {
  if (argc == 1) {
    int alias = fcntl(STDOUT_FILENO, F_DUPFD_CLOEXEC, 3);
    if (alias < 0) {
      fail("fcntl(F_DUPFD_CLOEXEC)");
    }
    if (write(alias, "before-exec\n", 12) != 12) {
      fail("write(console alias)");
    }
    char *const missing_argv[] = {"/definitely/missing-hermit-test", NULL};
    errno = 0;
    if (execv(missing_argv[0], missing_argv) != -1 || errno != ENOENT) {
      fprintf(stderr, "missing exec unexpectedly returned errno %d\n", errno);
      return EXIT_FAILURE;
    }
    if (write(alias, "after-failed-exec\n", 18) != 18) {
      fail("write(console alias after failed exec)");
    }

    char alias_arg[32];
    snprintf(alias_arg, sizeof(alias_arg), "%d", alias);
    char *const exec_argv[] = {argv[0], alias_arg, NULL};
    execv(argv[0], exec_argv);
    fail("execv");
  }

  int expected_fd = atoi(argv[1]);
  int null_fd = open("/dev/null", O_WRONLY);
  if (null_fd < 0) {
    fail("open(/dev/null)");
  }
  if (null_fd != expected_fd) {
    fprintf(stderr, "expected fd %d to be reused, got %d\n", expected_fd,
            null_fd);
    return EXIT_FAILURE;
  }
  if (write(null_fd, "hidden\n", 7) != 7) {
    fail("write(/dev/null)");
  }
  if (write(STDOUT_FILENO, "visible\n", 8) != 8) {
    fail("write(stdout)");
  }
  return EXIT_SUCCESS;
}
