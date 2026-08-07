/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * AUTONOMOUS-BOT-IMPLEMENTED
 * TODO-HUMAN-REVIEW(PR-951): Review /proc/key-users access-form mediation.
 *
 * Regression coverage for review items 1 and 2 on PR #951: recognition keyed on
 * the literal guest spelling `/proc/key-users`, so equivalent spellings and
 * dirfd-relative opens were expected to bypass normalization, and only
 * `handle_read` consumed the snapshot, so positioned reads were expected to
 * expose live host quota data.
 *
 * Both were fixed by the systemic procfs work (issue #973) before this probe
 * existed. It exists so a regression is caught by a test rather than
 * rediscovered by review: every access form below must observe the SAME
 * sanitized snapshot, and none may return live kernel content.
 *
 * The discriminator is content, not merely determinism. Real
 * `/proc/key-users` on the validation host is several hundred bytes listing
 * every uid's live key usage; the sanitized snapshot is a single normalized
 * row. Comparing every form against the plain `read` result therefore
 * distinguishes "mediated" from "bypassed" even when the host file happens to
 * be momentarily stable.
 *
 * `readv` and `preadv` are expected to be REFUSED with ENOSYS rather than
 * mediated. That is deliberate fail-closed behaviour in
 * `detcore/src/syscalls/files.rs`, not an accident, and the probe asserts the
 * refusal instead of collapsing it into "mediated" -- they are different
 * outcomes with different consequences.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/uio.h>
#include <unistd.h>

#define PATH_KEY_USERS "/proc/key-users"

static char reference[8192];
static ssize_t reference_len;

/* Read a whole file with plain read(2). Returns bytes read, or -1. */
static ssize_t slurp(int fd, char *buf, size_t cap) {
  ssize_t total = 0;
  for (;;) {
    ssize_t n = read(fd, buf + total, cap - (size_t)total);
    if (n < 0) {
      return -1;
    }
    if (n == 0) {
      return total;
    }
    total += n;
  }
}

/* Assert that `buf` matches the reference snapshot taken via plain read(2). */
static int same_as_reference(const char *form, const char *buf, ssize_t n) {
  if (n != reference_len || memcmp(buf, reference, (size_t)n) != 0) {
    fprintf(stderr,
            "%s bypassed the procfs snapshot: got %zd bytes, reference is %zd\n",
            form, n, reference_len);
    return 0;
  }
  return 1;
}

/* Open with the given spelling, plain-read it, and compare to the reference. */
static int check_spelling(const char *form, const char *path) {
  char buf[sizeof(reference)];
  int fd = open(path, O_RDONLY);
  if (fd < 0) {
    fprintf(stderr, "%s: open(%s) failed errno=%d\n", form, path, errno);
    return 0;
  }
  ssize_t n = slurp(fd, buf, sizeof(buf));
  close(fd);
  if (n < 0) {
    fprintf(stderr, "%s: read failed errno=%d\n", form, errno);
    return 0;
  }
  return same_as_reference(form, buf, n);
}

int main(void) {
  char buf[sizeof(reference)];

  /* Reference: the plain sequential read that `handle_read` has always
   * mediated. Every other form is compared against this. */
  int fd = open(PATH_KEY_USERS, O_RDONLY);
  if (fd < 0) {
    fprintf(stderr, "open(%s) failed errno=%d\n", PATH_KEY_USERS, errno);
    return 1;
  }
  reference_len = slurp(fd, reference, sizeof(reference));
  close(fd);
  if (reference_len <= 0) {
    fprintf(stderr, "reference read returned %zd\n", reference_len);
    return 1;
  }

  /* Item 2: positioned read must observe the snapshot, not live bytes. */
  fd = open(PATH_KEY_USERS, O_RDONLY);
  if (fd < 0) {
    return 1;
  }
  ssize_t n = pread(fd, buf, sizeof(buf), 0);
  close(fd);
  if (n < 0 || !same_as_reference("pread64", buf, n)) {
    return 1;
  }

  /* Item 2: lseek must rewind the synthetic cursor, not just the kernel's. */
  fd = open(PATH_KEY_USERS, O_RDONLY);
  if (fd < 0) {
    return 1;
  }
  if (slurp(fd, buf, sizeof(buf)) < 0) {
    close(fd);
    return 1;
  }
  off_t rewound = lseek(fd, 0, SEEK_SET);
  n = slurp(fd, buf, sizeof(buf));
  close(fd);
  if (rewound != 0) {
    fprintf(stderr, "lseek did not rewind: offset=%lld\n", (long long)rewound);
    return 1;
  }
  if (n < 0 || !same_as_reference("lseek+read", buf, n)) {
    return 1;
  }
  printf("key-users-positioned-mediated-ok\n");

  /* Vectored reads are refused, not mediated. Assert the refusal explicitly. */
  struct iovec vec = {buf, sizeof(buf)};
  fd = open(PATH_KEY_USERS, O_RDONLY);
  if (fd < 0) {
    return 1;
  }
  n = readv(fd, &vec, 1);
  int readv_errno = errno;
  close(fd);
  if (n >= 0 || readv_errno != ENOSYS) {
    fprintf(stderr, "readv: expected ENOSYS, got n=%zd errno=%d\n", n,
            n < 0 ? readv_errno : 0);
    return 1;
  }

  fd = open(PATH_KEY_USERS, O_RDONLY);
  if (fd < 0) {
    return 1;
  }
  n = preadv(fd, &vec, 1, 0);
  int preadv_errno = errno;
  close(fd);
  if (n >= 0 || preadv_errno != ENOSYS) {
    fprintf(stderr, "preadv: expected ENOSYS, got n=%zd errno=%d\n", n,
            n < 0 ? preadv_errno : 0);
    return 1;
  }
  printf("key-users-vectored-refused-ok\n");

  /* Item 1: equivalent spellings and dirfd-relative opens. */
  if (!check_spelling("alias-dot", "/proc/./" "key-users")) {
    return 1;
  }
  if (!check_spelling("alias-self-parent", "/proc/self/../key-users")) {
    return 1;
  }
  if (chdir("/proc") != 0) {
    fprintf(stderr, "chdir(/proc) failed errno=%d\n", errno);
    return 1;
  }
  if (!check_spelling("relative", "key-users")) {
    return 1;
  }

  int dir = open("/proc", O_RDONLY | O_DIRECTORY);
  if (dir < 0) {
    fprintf(stderr, "open(/proc) failed errno=%d\n", errno);
    return 1;
  }
  fd = openat(dir, "key-users", O_RDONLY);
  close(dir);
  if (fd < 0) {
    fprintf(stderr, "openat(dirfd, key-users) failed errno=%d\n", errno);
    return 1;
  }
  n = slurp(fd, buf, sizeof(buf));
  close(fd);
  if (n < 0 || !same_as_reference("openat-dirfd", buf, n)) {
    return 1;
  }
  printf("key-users-aliases-mediated-ok\n");

  /*
   * Cross-form equality alone is a weak discriminator: if every form bypassed
   * together, they would still agree with each other. Measured natively, the
   * checks above all pass on a completely unmediated system because the reads
   * complete faster than the harness's churn moves the file.
   *
   * So re-read after a real delay. The harness has already proved the churn
   * changes `/proc/key-users` within ~200ms, so a live read taken a second
   * later must differ from the reference; only a frozen snapshot stays equal.
   * Detcore virtualizes the sleep, which is exactly what makes this work: under
   * Hermit no real time passes and the snapshot is stable by construction,
   * while an unmediated run really does wait and really does see the file move.
   */
  sleep(1);
  fd = open(PATH_KEY_USERS, O_RDONLY);
  if (fd < 0) {
    return 1;
  }
  n = slurp(fd, buf, sizeof(buf));
  close(fd);
  if (n < 0 || !same_as_reference("delayed-reread", buf, n)) {
    return 1;
  }
  printf("key-users-snapshot-stable-ok\n");

  return 0;
}
