/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Vectored I/O (writev/readv) parity probe.
 *
 * A single process creates a pipe, assembles a fixed byte stream (every value
 * 0..255 exactly once) out of three discontiguous source buffers, and writes
 * it with one writev(2). It then closes the write end and drains the pipe with
 * readv(2) into two discontiguous destination buffers until EOF, accumulating
 * the byte count and a running checksum.
 *
 * This exercises the scatter/gather I/O path (iovec assembly on write, iovec
 * fan-out on read, partial-iov accounting) that the plain read/write contracts
 * do not cover. The whole stream is 256 bytes, well under the pipe capacity, so
 * the single writev never blocks and no reader/writer interleave exists.
 *
 * It is deliberately free of gated concerns:
 *   - Single process, no fork/thread: no scheduling interleave is observed.
 *   - The only observable is an aggregate over the byte stream (count plus an
 *     additive checksum), independent of how writev/readv chunk the segments.
 *   - No pid, timestamp, cpu-time, or address is observed.
 *
 * For the fixed 0..255 stream the byte count is 256 and the checksum is
 * 255*256/2 = 32640 on any conforming backend.
 */

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/uio.h>
#include <unistd.h>

#define STREAM_BYTES 256

static void fail(const char *message) {
  fprintf(stderr, "%s: %s\n", message, strerror(errno));
  exit(1);
}

/*
 * The e2e harness has no golden-output field: its verify oracle is exit status
 * plus cross-attempt determinism. A deterministically wrong stdout therefore
 * passes unnoticed unless the guest checks itself, so every invariant below is
 * asserted rather than merely printed.
 */
static int violations;

static void expect(const char *name, long long observed, long long wanted) {
  if (observed != wanted) {
    fprintf(stderr, "invariant %s: observed %lld, wanted %lld\n", name, observed,
            wanted);
    violations++;
  }
}

/* Write the whole iovec set, tolerating short writes by advancing past the
 * bytes already sent. */
static int writev_all(int fd, struct iovec *iov, int iovcnt) {
  while (iovcnt > 0) {
    ssize_t n = writev(fd, iov, iovcnt);
    if (n < 0) {
      if (errno == EINTR)
        continue;
      return -1;
    }
    /* Consume n bytes from the front of the iovec array. */
    while (iovcnt > 0 && (size_t)n >= iov[0].iov_len) {
      n -= (ssize_t)iov[0].iov_len;
      ++iov;
      --iovcnt;
    }
    if (iovcnt > 0 && n > 0) {
      iov[0].iov_base = (char *)iov[0].iov_base + n;
      iov[0].iov_len -= (size_t)n;
    }
  }
  return 0;
}

int main(void) {
  int fds[2];
  if (pipe(fds) != 0)
    fail("pipe");

  /* Build the 0..255 stream split across three discontiguous segments. */
  uint8_t seg_a[100];
  uint8_t seg_b[100];
  uint8_t seg_c[STREAM_BYTES - 200];
  for (int i = 0; i < 100; ++i)
    seg_a[i] = (uint8_t)i;
  for (int i = 0; i < 100; ++i)
    seg_b[i] = (uint8_t)(100 + i);
  for (int i = 0; i < (int)sizeof(seg_c); ++i)
    seg_c[i] = (uint8_t)(200 + i);

  struct iovec wiov[3];
  wiov[0].iov_base = seg_a;
  wiov[0].iov_len = sizeof(seg_a);
  wiov[1].iov_base = seg_b;
  wiov[1].iov_len = sizeof(seg_b);
  wiov[2].iov_base = seg_c;
  wiov[2].iov_len = sizeof(seg_c);

  if (writev_all(fds[1], wiov, 3) != 0)
    fail("writev");
  /* Close the write end so the reader observes EOF after the stream. */
  if (close(fds[1]) != 0)
    fail("close write end");

  long bytes = 0;
  long checksum = 0;
  uint8_t dst_a[64];
  uint8_t dst_b[64];
  for (;;) {
    struct iovec riov[2];
    riov[0].iov_base = dst_a;
    riov[0].iov_len = sizeof(dst_a);
    riov[1].iov_base = dst_b;
    riov[1].iov_len = sizeof(dst_b);
    ssize_t n = readv(fds[0], riov, 2);
    if (n < 0) {
      if (errno == EINTR)
        continue;
      fail("readv");
    }
    if (n == 0)
      break; /* EOF */
    for (ssize_t i = 0; i < n; ++i) {
      /* The two destination buffers are contiguous in logical order: the
       * first sizeof(dst_a) bytes land in dst_a, the rest in dst_b. */
      uint8_t byte = (i < (ssize_t)sizeof(dst_a))
                         ? dst_a[i]
                         : dst_b[i - (ssize_t)sizeof(dst_a)];
      ++bytes;
      checksum += byte;
    }
  }
  if (close(fds[0]) != 0)
    fail("close read end");

  /* The two aggregates the header describes. `bytes` is counted from the readv
     returns, so comparing it against the intended stream length catches a short
     drain; `checksum` is 0+1+...+255. */
  expect("bytes", (long long)bytes, STREAM_BYTES);
  expect("checksum", (long long)checksum, 32640);
  printf("vectored_io bytes=%ld checksum=%ld\n", bytes, checksum);
  return violations == 0 ? 0 : 1;
}
