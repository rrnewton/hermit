/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Descriptor syncs, metadata changes, xattr calls and zero-length reads
// whose results must come from the recording. The first file lives in a host
// directory absent from the replay chroot, so replay hands the guest a
// placeholder descriptor: a live fsync, fchmod or fgetxattr there answers
// EINVAL or EOPNOTSUPP instead of the recorded result. The second file exists
// in the replay root, where descriptor and path xattr calls must agree. Every result is checked
// against its expected value, so record and replay cannot agree on a wrong
// failure and still pass.

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/xattr.h>
#include <unistd.h>

#define NAME "user.hermit"
#define VALUE "recorded"

static int failures;

// Print a result and require it to succeed (expected_errno == 0) or to fail
// with expected_errno.
static void expect(const char* name, long result, int expected_errno) {
  int actual_errno = result < 0 ? errno : 0;
  if (result < 0) {
    printf("%s=-1 errno=%s\n", name, strerrorname_np(actual_errno));
  } else {
    printf("%s=%ld\n", name, result);
  }
  if (actual_errno != expected_errno) {
    printf(
        "UNEXPECTED %s: wanted %s\n",
        name,
        expected_errno ? strerrorname_np(expected_errno) : "success");
    failures++;
  }
}

static void expect_value(const char* name, long result, long wanted) {
  expect(name, result, 0);
  if (result >= 0 && result != wanted) {
    printf("UNEXPECTED %s: wanted %ld\n", name, wanted);
    failures++;
  }
}

// A sized query must return the value the size query promised, and the
// buffer must hold the recorded bytes.
static void expect_xattr_value(const char* name, long result, const char* buf) {
  expect_value(name, result, (long)strlen(VALUE));
  if (result == (long)strlen(VALUE) && memcmp(buf, VALUE, result) != 0) {
    printf("UNEXPECTED %s: value %.*s\n", name, (int)result, buf);
    failures++;
  }
}

// A list must hold NAME as one of its NUL-separated entries. Other entries,
// such as a security label, depend on the host and are not checked.
static void expect_list(const char* name, long result, const char* buf) {
  expect(name, result, 0);
  int found = 0;
  for (long i = 0; i < result; i += strlen(buf + i) + 1) {
    found |= strcmp(buf + i, NAME) == 0;
  }
  if (!found) {
    printf("UNEXPECTED %s: no %s entry\n", name, NAME);
    failures++;
  }
}

int main(int argc, char** argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s HOST_DIRECTORY\n", argv[0]);
    return 2;
  }
  char path[4096];
  snprintf(path, sizeof(path), "%s/file", argv[1]);
  int fd = open(path, O_CREAT | O_RDWR | O_TRUNC, 0644);
  if (fd < 0) {
    perror("open");
    return 2;
  }
  char buf[256];

  // A read that transfers nothing never touches the buffer, so NULL is valid.
  expect_value("read-null-zero", syscall(SYS_read, fd, NULL, 0), 0);
  expect_value("read-zero", syscall(SYS_read, fd, buf, 0), 0);
  expect_value("pread-null-zero", syscall(SYS_pread64, fd, NULL, 0, 0), 0);
  expect_value("read-eof-null", syscall(SYS_read, fd, NULL, 16), 0);

  expect("fsync", fsync(fd), 0);
  expect("fdatasync", fdatasync(fd), 0);
  expect("syncfs", syncfs(fd), 0);
  expect("fchmod", fchmod(fd, 0600), 0);
  expect("fchown", fchown(fd, -1, -1), 0);

  expect("fsetxattr", fsetxattr(fd, NAME, VALUE, strlen(VALUE), 0), 0);
  // A zero size asks only for the length; the buffer is not touched.
  expect_value(
      "fgetxattr-size", fgetxattr(fd, NAME, NULL, 0), (long)strlen(VALUE));
  memset(buf, 0, sizeof(buf));
  expect_xattr_value(
      "fgetxattr", fgetxattr(fd, NAME, buf, sizeof(buf)), buf);
  memset(buf, 0, sizeof(buf));
  expect_xattr_value("getxattr", getxattr(path, NAME, buf, sizeof(buf)), buf);
  memset(buf, 0, sizeof(buf));
  expect_xattr_value(
      "lgetxattr", lgetxattr(path, NAME, buf, sizeof(buf)), buf);
  expect("fgetxattr-small", fgetxattr(fd, NAME, buf, 1), ERANGE);
  expect("fgetxattr-missing", fgetxattr(fd, "user.absent", buf, 8), ENODATA);

  long size = flistxattr(fd, NULL, 0);
  expect("flistxattr-size", size, 0);
  memset(buf, 0, sizeof(buf));
  long listed = flistxattr(fd, buf, sizeof(buf));
  expect_list("flistxattr", listed, buf);
  if (listed != size) {
    printf("UNEXPECTED flistxattr: %ld bytes, size query said %ld\n", listed, size);
    failures++;
  }
  memset(buf, 0, sizeof(buf));
  expect_list("listxattr", listxattr(path, buf, sizeof(buf)), buf);
  memset(buf, 0, sizeof(buf));
  expect_list("llistxattr", llistxattr(path, buf, sizeof(buf)), buf);

  expect("fremovexattr", fremovexattr(fd, NAME), 0);
  expect("fgetxattr-removed", fgetxattr(fd, NAME, buf, sizeof(buf)), ENODATA);

  struct stat st;
  expect("fstat", fstat(fd, &st), 0);
  if ((st.st_mode & 07777) != 0600) {
    printf("UNEXPECTED fstat mode %o\n", st.st_mode & 07777);
    failures++;
  }
  close(fd);

  // A file the guest creates in its working directory exists in the replay
  // root too (replay enters the recorded directory), so path calls there run
  // live and must see the attributes descriptor calls set or removed.
  if (chdir(argv[1]) != 0) {
    perror("chdir");
    return 2;
  }
  int local = open("local", O_CREAT | O_RDWR | O_TRUNC, 0644);
  expect("open-local", local, 0);
  expect("local-fsetxattr", fsetxattr(local, NAME, VALUE, strlen(VALUE), 0), 0);
  expect("local-removexattr", removexattr("local", NAME), 0);
  expect("local-fremovexattr-gone", fremovexattr(local, NAME), ENODATA);
  expect("local-fsetxattr-again", fsetxattr(local, NAME, "v2", 2, 0), 0);
  expect(
      "local-setxattr-create",
      setxattr("local", NAME, VALUE, strlen(VALUE), XATTR_CREATE),
      EEXIST);
  expect(
      "local-lsetxattr-replace",
      lsetxattr("local", NAME, VALUE, strlen(VALUE), XATTR_REPLACE),
      0);
  memset(buf, 0, sizeof(buf));
  expect_xattr_value(
      "local-fgetxattr", fgetxattr(local, NAME, buf, sizeof(buf)), buf);
  expect("local-lremovexattr", lremovexattr("local", NAME), 0);
  expect("local-fremovexattr-after", fremovexattr(local, NAME), ENODATA);
  expect("local-fchmod", fchmod(local, 0600), 0);
  close(local);

  printf("failures=%d\n", failures);
  return failures ? 1 : 0;
}
