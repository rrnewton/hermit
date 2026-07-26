/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <unistd.h>

static void require_zero(long result, const char *name) {
  if (result != 0) {
    fprintf(stderr, "%s failed: %s\n", name, strerror(errno));
    exit(1);
  }
}

int main(void) {
  uid_t ruid = (uid_t)-1;
  uid_t euid = (uid_t)-1;
  uid_t suid = (uid_t)-1;
  gid_t rgid = (gid_t)-1;
  gid_t egid = (gid_t)-1;
  gid_t sgid = (gid_t)-1;

  require_zero(syscall(SYS_getresuid, &ruid, &euid, &suid), "getresuid");
  require_zero(syscall(SYS_getresgid, &rgid, &egid, &sgid), "getresgid");
  if (ruid == (uid_t)-1 || euid == (uid_t)-1 || suid == (uid_t)-1 ||
      rgid == (gid_t)-1 || egid == (gid_t)-1 || sgid == (gid_t)-1) {
    fputs("credential syscall did not initialize every output\n", stderr);
    return 1;
  }

  /*
   * Credential-setting family. Hermit runs the guest as uid 0 inside a
   * single-uid user namespace, so setting the mapped id 0 succeeds and is
   * deterministic on every run. These previously classified as Unsupported and
   * fail-closed --strict (aborting runuser/setpriv/su); they now pass through
   * like the getresuid/getresgid read side and capset above. setgroups is
   * deliberately excluded: a single-uid user namespace pins /proc/self/setgroups
   * to "deny", so it returns EPERM (faithfully and deterministically) rather
   * than succeeding.
   */
  require_zero(syscall(SYS_setuid, 0), "setuid");
  require_zero(syscall(SYS_setgid, 0), "setgid");
  require_zero(syscall(SYS_setreuid, 0, 0), "setreuid");
  require_zero(syscall(SYS_setregid, 0, 0), "setregid");
  require_zero(syscall(SYS_setresuid, 0, 0, 0), "setresuid");
  require_zero(syscall(SYS_setresgid, 0, 0, 0), "setresgid");
  /* setfsuid/setfsgid never fail; they return the previous id, so just run them
   * to confirm they are no longer fail-closed. */
  syscall(SYS_setfsuid, 0);
  syscall(SYS_setfsgid, 0);

  size_t page_size = (size_t)sysconf(_SC_PAGESIZE);
  void *page = mmap(NULL, page_size, PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (page == MAP_FAILED) {
    perror("mmap");
    return 1;
  }
  require_zero(syscall(SYS_munlock, page, page_size), "munlock");
  require_zero(syscall(SYS_munlockall), "munlockall");
  if (munmap(page, page_size) != 0) {
    perror("munmap");
    return 1;
  }

  char path[128];
  snprintf(path, sizeof(path), "/tmp/hermit-syscall-quick-wins-%ld",
           (long)getpid());
  int fd = open(path, O_CREAT | O_TRUNC | O_RDWR, 0600);
  if (fd < 0 || write(fd, "x", 1) != 1) {
    perror("open/write");
    return 1;
  }
  require_zero(syscall(SYS_fsync, fd), "fsync");
  if (close(fd) != 0 || unlink(path) != 0) {
    perror("close/unlink");
    return 1;
  }
  printf("syscall-quick-wins-ok uids=%u:%u:%u gids=%u:%u:%u vm=ok fs=ok\n",
         ruid, euid, suid, rgid, egid, sgid);
  return 0;
}
