/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Path queries and legacy path mutations whose results must come from the
// recording. Replay runs in a chroot where host files such as /etc/passwd and
// directories such as /usr/lib are absent, so a live faccessat, chdir or
// getcwd answers differently there. Raw syscall numbers keep libc from
// substituting the *at forms.

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

static void report(const char* name, long result) {
  if (result < 0) {
    printf("%s=-1 errno=%s\n", name, strerrorname_np(errno));
  } else {
    printf("%s=%ld\n", name, result);
  }
}

int main(void) {
  report(
      "faccessat_passwd_r",
      syscall(SYS_faccessat, AT_FDCWD, "/etc/passwd", R_OK));
  report(
      "faccessat2_usr_bin_x",
      syscall(SYS_faccessat2, AT_FDCWD, "/usr/bin", X_OK, AT_EACCESS));
  report(
      "faccessat_missing",
      syscall(SYS_faccessat, AT_FDCWD, "/nonexistent/hermit", F_OK));

  char cwd[4096];
  report("chdir_usr_lib", syscall(SYS_chdir, "/usr/lib"));
  long length = syscall(SYS_getcwd, cwd, sizeof(cwd));
  report("getcwd", length);
  if (length > 0) {
    printf("cwd=%s\n", cwd);
  }
  report("getcwd_small", syscall(SYS_getcwd, cwd, 2));
  report("chdir_missing", syscall(SYS_chdir, "/nonexistent/hermit"));

  char dir[] = "/tmp/hermit-rr-paths.XXXXXX";
  if (mkdtemp(dir) == NULL) {
    perror("mkdtemp");
    return 1;
  }
  report("chdir_tmp", syscall(SYS_chdir, dir));
  int fd = open("a", O_CREAT | O_WRONLY | O_EXCL, 0644);
  report("create_a", fd < 0 ? -1 : 0);
  if (fd >= 0) {
    close(fd);
  }
  report("chmod", syscall(SYS_chmod, "a", 0600));
  report("rename", syscall(SYS_rename, "a", "b"));
  report("link", syscall(SYS_link, "b", "c"));
  report("symlink", syscall(SYS_symlink, "b", "d"));
  report("chown", syscall(SYS_chown, "b", -1, -1));
  report("lchown", syscall(SYS_lchown, "d", -1, -1));
  report("mknod", syscall(SYS_mknod, "p", S_IFIFO | 0600, 0));
  report("mkdir", mkdir("e", 0755));
  report("rmdir", syscall(SYS_rmdir, "e"));
  report("rename_missing", syscall(SYS_rename, "missing", "x"));

  struct stat st;
  if (stat("b", &st) == 0) {
    printf("b_mode=%o b_nlink=%lu\n", st.st_mode & 07777, st.st_nlink);
  }

  unlink("b");
  unlink("c");
  unlink("d");
  unlink("p");
  report("chdir_root", syscall(SYS_chdir, "/"));
  report("rmdir_tmp", syscall(SYS_rmdir, dir));
  return 0;
}
