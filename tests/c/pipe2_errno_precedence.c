/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Linux validates pipe2's arguments in a fixed order, and a guest doing its own
 * argument checking depends on that order:
 *
 *   flags are checked FIRST, before pipefd is touched at all
 *     -> a bad pointer with BAD flags is EINVAL, not EFAULT
 *   only then is pipefd written
 *     -> a bad pointer with GOOD flags is EFAULT
 *
 * Detcore pins the capacity of scheduler-managed pipes, which requires knowing
 * the caller's pipefd bytes from before the kernel overwrote them. Reading that
 * snapshot EAGERLY, and letting its failure escape, replaced both answers with
 * the tool's own memory error (measured: EIO for both), so a guest could no
 * longer tell a bad pointer from bad flags. The snapshot is now best-effort and
 * the kernel alone decides the errno.
 *
 * This probe asserts the precedence directly rather than only asserting that
 * two runs agree: a deterministic WRONG errno is still wrong, and a pure
 * determinism check would pass it.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

/* Deliberately raw: glibc's pipe2 wrapper may pre-screen arguments, and the
 * point here is what the kernel/tool boundary returns. */
static long raw_pipe2(void* pipefd, int flags) {
  errno = 0;
  return syscall(SYS_pipe2, pipefd, (long)flags);
}

static int failures = 0;

static void expect_errno(
    const char* what,
    void* pipefd,
    int flags,
    int want_errno) {
  long rc = raw_pipe2(pipefd, flags);
  int got = errno;
  if (rc == -1 && got == want_errno) {
    printf("%s: errno=%d ok\n", what, got);
    return;
  }
  fprintf(
      stderr,
      "%s: expected rc=-1 errno=%d, got rc=%ld errno=%d\n",
      what,
      want_errno,
      rc,
      got);
  failures++;
}

int main(void) {
  /* A non-null pointer that cannot be written. This is the case that
   * regressed; NULL never did, because a null pipefd is filtered out before
   * the snapshot is attempted. Both are asserted so the fix cannot be narrowed
   * to only the null case. */
  void* bad = (void*)1;

  expect_errno("badptr_validflags", bad, 0, EFAULT);
  expect_errno("nullptr_validflags", NULL, 0, EFAULT);

  /* Flags are checked before the pointer, so these stay EINVAL even though the
   * pointer is also unusable. This is the precedence assertion. */
  expect_errno("badptr_allflags", bad, -1, EINVAL);
  expect_errno("badptr_oappend", bad, O_APPEND, EINVAL);

  /* Positive control, so a "fix" that simply stopped pinning capacity would
   * fail here rather than look clean. Under Detcore the capacity is pinned to
   * one page; run natively it is whatever the host default is, so only assert
   * the pipe works and report the capacity. */
  int fds[2] = {-1, -1};
  long rc = raw_pipe2(fds, 0);
  if (rc != 0) {
    fprintf(stderr, "valid_pipe2: expected success, got rc=%ld errno=%d\n", rc,
            errno);
    failures++;
  } else {
    int capacity = fcntl(fds[1], F_GETPIPE_SZ);
    printf("valid_pipe2: ok capacity=%d\n", capacity);
    /* The pair must be usable, not merely returned. */
    if (write(fds[1], "x", 1) != 1) {
      perror("valid_pipe2 write");
      failures++;
    }
    char byte = 0;
    if (read(fds[0], &byte, 1) != 1 || byte != 'x') {
      perror("valid_pipe2 read");
      failures++;
    }
    close(fds[0]);
    close(fds[1]);
  }

  if (failures != 0) {
    fprintf(stderr, "pipe2-errno-precedence: %d case(s) failed\n", failures);
    return 1;
  }
  puts("pipe2-errno-precedence-ok");
  return 0;
}
