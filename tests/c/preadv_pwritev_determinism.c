/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Exercises the positioned vectored I/O family (preadv, preadv2, pwritev,
 * pwritev2) on a regular file. Before batch 65 these syscalls were classified
 * Unsupported, so `hermit run --strict` fail-closed and tore down the sandbox.
 * They are now determinized as siblings of pread64/pwrite64/writev: the offset
 * is supplied explicitly and the open-file cursor is untouched, so a single
 * record_or_replay under the FileContents resource is deterministic.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/uio.h>
#include <unistd.h>

static int check_pwritev_preadv(int fd) {
  /* Positioned vectored write of "AAAA""BBBB" at offset 3. */
  char a[] = "AAAA";
  char b[] = "BBBB";
  struct iovec wiov[2] = {
      {.iov_base = a, .iov_len = 4},
      {.iov_base = b, .iov_len = 4},
  };
  ssize_t written = pwritev(fd, wiov, 2, 3);
  if (written != 8) {
    fprintf(stderr, "pwritev returned %zd, expected 8: %s\n", written,
            strerror(errno));
    return -1;
  }

  /* Positioned vectored read back into two split buffers at offset 3. */
  char r1[4] = {0};
  char r2[5] = {0};
  struct iovec riov[2] = {
      {.iov_base = r1, .iov_len = 4},
      {.iov_base = r2, .iov_len = 4},
  };
  ssize_t got = preadv(fd, riov, 2, 3);
  if (got != 8 || memcmp(r1, "AAAA", 4) != 0 || memcmp(r2, "BBBB", 4) != 0) {
    fprintf(stderr, "preadv got %zd r1=%.4s r2=%.4s\n", got, r1, r2);
    return -1;
  }
  return 0;
}

static int check_pwritev2_preadv2(int fd) {
  /* preadv2/pwritev2 add an RWF_* flags argument; flags=0 matches the plain
   * variants and must forward through the kernel deterministically. */
  char c[] = "position";
  struct iovec wiov[1] = {{.iov_base = c, .iov_len = 8}};
  ssize_t written = pwritev2(fd, wiov, 1, 0, 0);
  if (written != 8) {
    fprintf(stderr, "pwritev2 returned %zd, expected 8: %s\n", written,
            strerror(errno));
    return -1;
  }

  char r1[3] = {0};
  char r2[6] = {0};
  struct iovec riov[2] = {
      {.iov_base = r1, .iov_len = 3},
      {.iov_base = r2, .iov_len = 5},
  };
  ssize_t got = preadv2(fd, riov, 2, 0, 0);
  if (got != 8 || memcmp(r1, "pos", 3) != 0 || memcmp(r2, "ition", 5) != 0) {
    fprintf(stderr, "preadv2 got %zd r1=%.3s r2=%.5s\n", got, r1, r2);
    return -1;
  }
  return 0;
}

int main(void) {
  char path[] = "/tmp/hermit-preadv-pwritev-XXXXXX";
  int fd = mkstemp(path);
  if (fd < 0) {
    perror("mkstemp");
    return 1;
  }

  int rc = 0;
  if (check_pwritev_preadv(fd) != 0 || check_pwritev2_preadv2(fd) != 0) {
    rc = 1;
  }

  close(fd);
  unlink(path);

  if (rc == 0) {
    puts("preadv-pwritev-determinism-ok");
  }
  return rc;
}
