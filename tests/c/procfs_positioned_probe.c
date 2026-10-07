/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * AUTONOMOUS-BOT-IMPLEMENTED
 * TODO-HUMAN-REVIEW(PR-973): Review positioned/copy procfs bypass coverage.
 *
 * Exercises the offset-based procfs read paths that used to bypass the
 * deterministic ProcfsFile snapshot:
 *
 *   1. pread64(/proc/self/stat) must return the *sanitized* snapshot, so the
 *      volatile "starttime" field (proc stat field 22) reads back as 0 rather
 *      than a live, run-varying kernel value.
 *   2. sendfile() with a procfs input must be refused with ENOSYS so callers
 *      fall back to the mediated read()/write() path.
 *   3. With the --mounts argument only: readv(), preadv() and preadv2() of
 *      /proc/self/mounts must serve the same bytes as read(), with Linux's
 *      shared and explicit position semantics, flag checks and fault
 *      behaviour.
 *      TODO-HUMAN-REVIEW(PR-3712): Review vectored /proc/self/mounts coverage.
 *   4. With the --mounts argument only: lseek(), read() and pread64() of
 *      /proc/self/mounts follow Linux's seq_file rules. SEEK_SET and SEEK_CUR
 *      move the position, including before the first read and past the end;
 *      SEEK_END, SEEK_DATA, SEEK_HOLE and a negative result are EINVAL and
 *      leave the position alone; a positioned read at or past the end
 *      returns 0 whatever the buffer; and a read into an unwritable buffer is
 *      EFAULT without moving the position.
 *      TODO-HUMAN-REVIEW(PR-3712): Review scalar /proc/self/mounts coverage.
 *
 * Checks 3 and 4 compare several opens of /proc/self/mounts, and Hermit
 * passes the host's mount rows through to that file, so they need a mount
 * table that does not change while the probe runs. Hermit does not make a
 * changing filesystem deterministic. The cargo test
 * hermit-cli/tests/procfs_positioned_determinism.rs passes --mounts and runs
 * Hermit with the ptrace backend in a private copy of the mount table; a run
 * without the argument, such as the c-programs/procfs-positioned-probe E2E
 * cell, performs checks 1 and 2 only. Checks 3 and 4 are therefore evidence
 * for the ptrace backend only.
 *
 * The probe prints only fixed strings, so a --strict --verify run is bitwise
 * identical across the two executions.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/sendfile.h>
#include <sys/uio.h>
#include <unistd.h>

/* Returns the 1-based proc stat field `index` (>= 3), or NULL on parse error.
 * The comm field (2) may contain spaces and parentheses, so scan past the
 * final ") " terminator and split the remainder on whitespace. */
static char* stat_field(char* buf, int index) {
  char* comm_end = strstr(buf, ") ");
  if (comm_end == NULL || index < 3) {
    return NULL;
  }
  char* rest = comm_end + 2;
  char* save = NULL;
  char* tok = strtok_r(rest, " \t\n", &save);
  for (int field = 3; tok != NULL; field++) {
    if (field == index) {
      return tok;
    }
    tok = strtok_r(NULL, " \t\n", &save);
  }
  return NULL;
}

static int check_pread_sanitized(void) {
  int fd = open("/proc/self/stat", O_RDONLY);
  if (fd < 0) {
    perror("open /proc/self/stat");
    return 1;
  }

  char buf[8192];
  ssize_t n = pread(fd, buf, sizeof(buf) - 1, 0);
  if (n <= 0) {
    fprintf(stderr, "pread /proc/self/stat: n=%zd errno=%d\n", n, errno);
    close(fd);
    return 1;
  }
  buf[n] = '\0';
  close(fd);

  /* Field 22 (starttime) is normalized to 0 by the snapshot sanitizer. Live
   * kernel bytes would contain a nonzero, run-varying tick count. */
  char* starttime = stat_field(buf, 22);
  if (starttime == NULL) {
    fprintf(stderr, "could not locate stat field 22 in: %s\n", buf);
    return 1;
  }
  if (strcmp(starttime, "0") != 0) {
    fprintf(
        stderr, "pread bypassed procfs snapshot: starttime=%s\n", starttime);
    return 1;
  }

  puts("procfs-pread-sanitized-ok");
  return 0;
}

static int check_sendfile_refused(void) {
  int in_fd = open("/proc/self/stat", O_RDONLY);
  int out_fd = open(
      "/tmp/hermit-procfs-sendfile-destination",
      O_WRONLY | O_CREAT | O_TRUNC,
      0600);
  if (in_fd < 0 || out_fd < 0) {
    perror("open sendfile endpoints");
    return 1;
  }

  errno = 0;
  ssize_t copied = sendfile(out_fd, in_fd, NULL, 4096);
  int saved = errno;
  close(in_fd);
  close(out_fd);

  if (copied == -1 && saved == ENOSYS) {
    puts("procfs-sendfile-refused-ok");
    return 0;
  }

  fprintf(
      stderr,
      "sendfile: expected ENOSYS, got copied=%zd errno=%d\n",
      copied,
      saved);
  return 1;
}

#define MOUNTS_CAPACITY (1 << 20)
static char mounts_by_read[MOUNTS_CAPACITY];
static char mounts_by_vector[MOUNTS_CAPACITY];

/* Reads `fd` to end of file with read(); returns the length, or -1. */
static ssize_t read_to_end(int fd, char* buf, size_t capacity) {
  size_t total = 0;
  for (;;) {
    if (total == capacity) {
      fprintf(stderr, "/proc/self/mounts exceeds %zu bytes\n", capacity);
      return -1;
    }
    ssize_t n = read(fd, buf + total, capacity - total);
    if (n < 0) {
      perror("read /proc/self/mounts");
      return -1;
    }
    if (n == 0) {
      return (ssize_t)total;
    }
    total += (size_t)n;
  }
}

static int expect_bytes(
    const char* what,
    ssize_t got,
    ssize_t want,
    const char* bytes,
    const char* expected) {
  if (got != want) {
    fprintf(
        stderr,
        "%s: returned %zd (errno %d), expected %zd\n",
        what,
        got,
        errno,
        want);
    return 1;
  }
  if (want > 0 && memcmp(bytes, expected, (size_t)want) != 0) {
    fprintf(stderr, "%s: bytes differ from read()\n", what);
    return 1;
  }
  return 0;
}

static int expect_errno(const char* what, ssize_t got, int want) {
  int saved = errno;
  if (got != -1 || saved != want) {
    fprintf(
        stderr,
        "%s: returned %zd (errno %d), expected -1 (errno %d)\n",
        what,
        got,
        saved,
        want);
    return 1;
  }
  return 0;
}

static int check_mounts_vectored(void) {
  int fd = open("/proc/self/mounts", O_RDONLY);
  if (fd < 0) {
    perror("open /proc/self/mounts");
    return 1;
  }
  ssize_t length = read_to_end(fd, mounts_by_read, MOUNTS_CAPACITY);
  close(fd);
  if (length < 32) {
    fprintf(stderr, "/proc/self/mounts read %zd bytes\n", length);
    return 1;
  }
  const char* m = mounts_by_read;
  int failed = 0;

  /* readv takes its first bytes, through an empty middle vector, from the
   * shared position, and read() then continues from where readv stopped. */
  fd = open("/proc/self/mounts", O_RDONLY);
  if (fd < 0) {
    perror("open /proc/self/mounts");
    return 1;
  }
  char* v = mounts_by_vector;
  struct iovec first[3] = {{v, 5}, {v + 5, 0}, {v + 5, 6}};
  failed |= expect_bytes("readv", readv(fd, first, 3), 11, v, m);
  ssize_t rest = read_to_end(fd, v + 11, MOUNTS_CAPACITY - 11);
  failed |= expect_bytes("read after readv", rest, length - 11, v + 11, m + 11);
  failed |= expect_bytes("readv at end of file", readv(fd, first, 3), 0, v, m);
  close(fd);

  fd = open("/proc/self/mounts", O_RDONLY);
  if (fd < 0) {
    perror("open /proc/self/mounts");
    return 1;
  }
  char a[3];
  char b[8];
  char head[4];
  char c[6];
  struct iovec positioned[2] = {{a, sizeof(a)}, {b, sizeof(b)}};
  struct iovec current[1] = {{c, sizeof(c)}};

  /* preadv reads at its own offset ... */
  ssize_t n = preadv(fd, positioned, 2, 4);
  char joined[sizeof(a) + sizeof(b)];
  memcpy(joined, a, sizeof(a));
  memcpy(joined + sizeof(a), b, sizeof(b));
  failed |= expect_bytes("preadv", n, 11, joined, m + 4);
  /* ... and leaves the shared position at 0. */
  failed |= expect_bytes("read after preadv", read(fd, head, 4), 4, head, m);
  /* preadv2 at offset -1 reads from, and advances, the shared position. */
  failed |= expect_bytes(
      "preadv2 at -1", preadv2(fd, current, 1, -1, 0), 6, c, m + 4);
  failed |=
      expect_bytes("read after preadv2", read(fd, head, 4), 4, head, m + 10);
  /* RWF_HIPRI is accepted at an explicit offset, which reads to the end. */
  n = preadv2(fd, positioned, 2, length - 2, RWF_HIPRI);
  failed |= expect_bytes("preadv2 HIPRI near the end", n, 2, a, m + length - 2);
  failed |= expect_bytes(
      "preadv at end of file", preadv(fd, positioned, 2, length), 0, a, m);
  /* This seq_file description refuses RWF_NOWAIT; a vector total of zero
   * returns zero before any flag check. */
  errno = 0;
  failed |= expect_errno(
      "preadv2 NOWAIT", preadv2(fd, current, 1, 0, RWF_NOWAIT), EOPNOTSUPP);
  struct iovec empty[1] = {{c, 0}};
  failed |= expect_bytes(
      "preadv2 NOWAIT of nothing",
      preadv2(fd, empty, 1, 0, RWF_NOWAIT),
      0,
      c,
      m);
  /* A fault at the first byte is EFAULT; a fault later returns the bytes
   * copied before it. Neither moves the shared position. */
  long page_size = sysconf(_SC_PAGESIZE);
  char* pages = mmap(
      NULL,
      (size_t)page_size * 2,
      PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS,
      -1,
      0);
  if (pages == MAP_FAILED ||
      mprotect(pages + page_size, (size_t)page_size, PROT_NONE) != 0) {
    perror("mmap guard page");
    return 1;
  }
  struct iovec unwritable[1] = {{pages + page_size, 8}};
  errno = 0;
  failed |=
      expect_errno("readv into a guard page", readv(fd, unwritable, 1), EFAULT);
  struct iovec straddle[1] = {{pages + page_size - 5, 10}};
  failed |= expect_bytes(
      "preadv across a guard page",
      preadv(fd, straddle, 1, 2),
      5,
      pages + page_size - 5,
      m + 2);
  failed |=
      expect_bytes("read after the faults", read(fd, head, 4), 4, head, m + 14);
  munmap(pages, (size_t)page_size * 2);
  close(fd);

  if (failed) {
    return 1;
  }
  puts("procfs-mounts-vectored-ok");
  return 0;
}

static int expect_offset(const char* what, off_t got, off_t want) {
  if (got != want) {
    fprintf(
        stderr,
        "%s: returned %lld (errno %d), expected %lld\n",
        what,
        (long long)got,
        errno,
        (long long)want);
    return 1;
  }
  return 0;
}

static int expect_seek_refused(const char* what, int fd, off_t offset, int whence) {
  errno = 0;
  return expect_errno(what, (ssize_t)lseek(fd, offset, whence), EINVAL);
}

/* SEEK_END, SEEK_DATA and SEEK_HOLE are EINVAL on a seq_file and leave the
 * position at `position`. */
static int expect_regular_file_seeks_refused(int fd, off_t position) {
  int failed = 0;
  failed |= expect_seek_refused("lseek SEEK_END", fd, 0, SEEK_END);
  failed |= expect_seek_refused("lseek SEEK_DATA", fd, 0, SEEK_DATA);
  failed |= expect_seek_refused("lseek SEEK_HOLE", fd, 0, SEEK_HOLE);
  failed |= expect_offset(
      "position after refused seeks", lseek(fd, 0, SEEK_CUR), position);
  return failed;
}

static int check_mounts_scalar(void) {
  int fd = open("/proc/self/mounts", O_RDONLY);
  if (fd < 0) {
    perror("open /proc/self/mounts");
    return 1;
  }
  ssize_t length = read_to_end(fd, mounts_by_read, MOUNTS_CAPACITY);
  close(fd);
  if (length < 32) {
    fprintf(stderr, "/proc/self/mounts read %zd bytes\n", length);
    return 1;
  }
  const char* m = mounts_by_read;
  char head[6];
  int failed = 0;

  /* Before the first read: refused seeks leave the position at 0, and
   * SEEK_SET and SEEK_CUR then decide where the first read starts. */
  fd = open("/proc/self/mounts", O_RDONLY);
  if (fd < 0) {
    perror("open /proc/self/mounts");
    return 1;
  }
  failed |= expect_regular_file_seeks_refused(fd, 0);
  failed |= expect_offset("lseek SEEK_SET 3", lseek(fd, 3, SEEK_SET), 3);
  failed |= expect_seek_refused("lseek SEEK_SET -1", fd, -1, SEEK_SET);
  failed |= expect_seek_refused("lseek SEEK_CUR -4", fd, -4, SEEK_CUR);
  failed |= expect_offset("lseek SEEK_CUR 2", lseek(fd, 2, SEEK_CUR), 5);
  failed |= expect_bytes(
      "read after seeking before the first read",
      read(fd, head, sizeof(head)),
      (ssize_t)sizeof(head),
      head,
      m + 5);

  /* After the first read the same rules hold. */
  off_t position = 5 + (off_t)sizeof(head);
  failed |= expect_regular_file_seeks_refused(fd, position);
  failed |= expect_seek_refused(
      "lseek SEEK_CUR before the start", fd, -position - 1, SEEK_CUR);
  failed |= expect_offset(
      "position after a negative SEEK_CUR", lseek(fd, 0, SEEK_CUR), position);

  /* A read into an unwritable buffer is EFAULT and does not move the
   * position; the next read continues from the same place. */
  /* volatile keeps the compiler from rejecting a NULL read buffer. */
  char* volatile nowhere = NULL;
  errno = 0;
  failed |= expect_errno("read into NULL", read(fd, nowhere, 1), EFAULT);
  failed |= expect_offset(
      "position after a faulting read", lseek(fd, 0, SEEK_CUR), position);
  failed |= expect_bytes(
      "read after a faulting read",
      read(fd, head, sizeof(head)),
      (ssize_t)sizeof(head),
      head,
      m + position);

  /* Seeking past the end is accepted, and a read there returns 0. */
  failed |= expect_offset(
      "lseek past the end", lseek(fd, length + 100, SEEK_SET), length + 100);
  failed |= expect_bytes(
      "read past the end", read(fd, head, sizeof(head)), 0, head, m);
  failed |= expect_offset(
      "position after reading past the end",
      lseek(fd, 0, SEEK_CUR),
      length + 100);
  close(fd);

  /* On a description that has not been read yet, a positioned read at or
   * past the end returns 0 even into an unwritable buffer, and a positioned
   * read inside the file is EFAULT. None of them moves the position. */
  long page_size = sysconf(_SC_PAGESIZE);
  char* page = mmap(
      NULL,
      (size_t)page_size,
      PROT_NONE,
      MAP_PRIVATE | MAP_ANONYMOUS,
      -1,
      0);
  if (page == MAP_FAILED) {
    perror("mmap PROT_NONE page");
    return 1;
  }
  fd = open("/proc/self/mounts", O_RDONLY);
  if (fd < 0) {
    perror("open /proc/self/mounts");
    return 1;
  }
  struct iovec unwritable[1] = {{page, 8}};
  failed |= expect_bytes(
      "pread64 at the end into PROT_NONE", pread(fd, page, 8, length), 0, page, m);
  failed |= expect_bytes(
      "preadv at the end into PROT_NONE",
      preadv(fd, unwritable, 1, length),
      0,
      page,
      m);
  failed |= expect_bytes(
      "pread64 past the end into PROT_NONE",
      pread(fd, page, 8, length + 100),
      0,
      page,
      m);
  errno = 0;
  failed |= expect_errno(
      "pread64 inside the file into PROT_NONE", pread(fd, page, 8, 0), EFAULT);
  errno = 0;
  failed |= expect_errno("pread64 at -1", pread(fd, head, 1, -1), EINVAL);
  failed |= expect_offset(
      "position after positioned reads", lseek(fd, 0, SEEK_CUR), 0);
  failed |= expect_bytes(
      "read after positioned reads",
      read(fd, head, sizeof(head)),
      (ssize_t)sizeof(head),
      head,
      m);
  close(fd);
  munmap(page, (size_t)page_size);

  if (failed) {
    return 1;
  }
  puts("procfs-mounts-scalar-ok");
  return 0;
}

int main(int argc, char** argv) {
  int mounts = 0;
  if (argc == 2 && strcmp(argv[1], "--mounts") == 0) {
    mounts = 1;
  } else if (argc != 1) {
    fprintf(stderr, "usage: %s [--mounts]\n", argv[0]);
    return 2;
  }
  if (check_pread_sanitized() != 0) {
    return 1;
  }
  if (check_sendfile_refused() != 0) {
    return 1;
  }
  if (mounts && check_mounts_vectored() != 0) {
    return 1;
  }
  if (mounts && check_mounts_scalar() != 0) {
    return 1;
  }
  return 0;
}
