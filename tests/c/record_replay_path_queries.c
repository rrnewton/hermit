/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Path queries and legacy path mutations whose results must come from the
// recording. Replay runs in a chroot where host files such as /etc/passwd and
// directories such as /usr/lib are absent, so a live access, faccessat, chdir
// or getcwd answers differently there. Raw syscall numbers keep libc from
// substituting the *at forms. Every result is checked against its expected
// value, so record and replay cannot agree on a wrong failure and still pass.

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_faccessat2
#define SYS_faccessat2 439
#endif

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

static long create(const char* path) {
  int fd = open(path, O_CREAT | O_WRONLY | O_EXCL, 0644);
  if (fd >= 0) {
    close(fd);
  }
  return fd < 0 ? -1 : 0;
}

// Each argument names a directory that exists on the host but not in the
// replay chroot. Replay must still move its working directory there, or both
// rounds of relative mutations land in one stale directory and the second link
// collides with the first.
static void mutate_in(const char* dir) {
  expect("chdir_host_dir", syscall(SYS_chdir, dir), 0);
  expect("create_f", create("f"), 0);
  expect("link_f_g", syscall(SYS_link, "f", "g"), 0);
}

int main(int argc, char** argv) {
  expect("access_passwd_r", syscall(SYS_access, "/etc/passwd", R_OK), 0);
  expect(
      "faccessat_passwd_r",
      syscall(SYS_faccessat, AT_FDCWD, "/etc/passwd", R_OK),
      0);
  expect(
      "faccessat2_usr_bin_x",
      syscall(SYS_faccessat2, AT_FDCWD, "/usr/bin", X_OK, AT_EACCESS),
      0);
  expect(
      "faccessat_missing",
      syscall(SYS_faccessat, AT_FDCWD, "/nonexistent/hermit", F_OK),
      ENOENT);

  char cwd[4096];
  expect("chdir_usr_lib", syscall(SYS_chdir, "/usr/lib"), 0);
  long length = syscall(SYS_getcwd, cwd, sizeof(cwd));
  expect("getcwd", length, 0);
  if (length > 0) {
    printf("cwd=%s\n", cwd);
    if (cwd[0] != '/' || (size_t)length != strlen(cwd) + 1) {
      printf("UNEXPECTED getcwd: length %ld does not match the path\n", length);
      failures++;
    }
  }
  expect("getcwd_small", syscall(SYS_getcwd, cwd, 2), ERANGE);
  expect(
      "chdir_missing", syscall(SYS_chdir, "/nonexistent/hermit"), ENOENT);

  for (int i = 1; i < argc; i++) {
    mutate_in(argv[i]);
  }
  if (argc > 1) {
    // fchdir back into the first host directory: its "f" must be reachable
    // relative to the restored working directory.
    int dirfd = open(argv[1], O_RDONLY | O_DIRECTORY);
    expect("open_host_dir", dirfd < 0 ? -1 : 0, 0);
    expect("chdir_root_before_fchdir", syscall(SYS_chdir, "/"), 0);
    expect("fchdir_host_dir", syscall(SYS_fchdir, dirfd), 0);
    expect("link_f_h", syscall(SYS_link, "f", "h"), 0);
    if (dirfd >= 0) {
      close(dirfd);
    }
  }

  char dir[] = "/tmp/hermit-rr-paths.XXXXXX";
  if (mkdtemp(dir) == NULL) {
    perror("mkdtemp");
    return 1;
  }
  expect("chdir_tmp", syscall(SYS_chdir, dir), 0);
  expect("create_a", create("a"), 0);
  expect("chmod", syscall(SYS_chmod, "a", 0600), 0);
  expect("rename", syscall(SYS_rename, "a", "b"), 0);
  expect("link", syscall(SYS_link, "b", "c"), 0);
  expect("symlink", syscall(SYS_symlink, "b", "d"), 0);
  expect("chown", syscall(SYS_chown, "b", -1, -1), 0);
  expect("lchown", syscall(SYS_lchown, "d", -1, -1), 0);
  expect("mknod", syscall(SYS_mknod, "p", S_IFIFO | 0600, 0), 0);
  expect("mkdir", mkdir("e", 0755), 0);
  expect("rmdir", syscall(SYS_rmdir, "e"), 0);
  expect("rename_missing", syscall(SYS_rename, "missing", "x"), ENOENT);

  struct stat st;
  expect("stat_b", stat("b", &st), 0);
  printf("b_mode=%o b_nlink=%lu\n", st.st_mode & 07777, st.st_nlink);
  if ((st.st_mode & 07777) != 0600 || st.st_nlink != 2) {
    printf("UNEXPECTED stat_b: wanted mode 600 and 2 links\n");
    failures++;
  }

  unlink("b");
  unlink("c");
  unlink("d");
  unlink("p");
  expect("chdir_root", syscall(SYS_chdir, "/"), 0);
  expect("rmdir_tmp", syscall(SYS_rmdir, dir), 0);
  printf("failures=%d\n", failures);
  return failures == 0 ? 0 : 1;
}
