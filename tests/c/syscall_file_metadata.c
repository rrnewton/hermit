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
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/xattr.h>
#include <unistd.h>

#ifndef SYS_fchmodat2
#define SYS_fchmodat2 452
#endif

#ifndef SYNC_FILE_RANGE_WRITE
#define SYNC_FILE_RANGE_WRITE 2
#endif

static void require_zero(long result, const char *name) {
  if (result != 0) {
    perror(name);
    exit(1);
  }
}

static int expected_xattr_error(int error) {
  return error == EACCES || error == ENODATA || error == ENOTSUP ||
         error == EOPNOTSUPP || error == EPERM;
}

static void require_xattr_result(ssize_t result, const char *name) {
  if (result < 0 && !expected_xattr_error(errno)) {
    perror(name);
    exit(1);
  }
}

int main(void) {
  char path[128];
  char hardlink_path[128];
  char symlink_path[128];
  long pid = (long)getpid();
  snprintf(path, sizeof(path), "/tmp/hermit-file-metadata-%ld", pid);
  snprintf(hardlink_path, sizeof(hardlink_path),
           "/tmp/hermit-file-metadata-%ld-hard", pid);
  snprintf(symlink_path, sizeof(symlink_path),
           "/tmp/hermit-file-metadata-%ld-sym", pid);

  unlink(symlink_path);
  unlink(hardlink_path);
  unlink(path);

  int fd = open(path, O_CREAT | O_TRUNC | O_RDWR, 0600);
  if (fd < 0) {
    perror("open");
    return 1;
  }
  require_zero(ftruncate(fd, 4096), "ftruncate");
  off_t offset_before = lseek(fd, 0, SEEK_CUR);
  if (offset_before < 0 || pwrite(fd, "metadata", 8, 0) != 8) {
    perror("pwrite");
    return 1;
  }
  off_t offset_after = lseek(fd, 0, SEEK_CUR);
  if (offset_after != offset_before) {
    fprintf(stderr, "pwrite changed file offset: %ld -> %ld\n",
            (long)offset_before, (long)offset_after);
    return 1;
  }
  char readback[8] = {0};
  if (pread(fd, readback, sizeof(readback), 0) != (ssize_t)sizeof(readback) ||
      memcmp(readback, "metadata", sizeof(readback)) != 0) {
    perror("pread after pwrite");
    return 1;
  }

  uid_t uid = getuid();
  gid_t gid = getgid();
  require_zero(fchmod(fd, 0600), "fchmod");
  require_zero(fchown(fd, uid, gid), "fchown");
  require_zero(syscall(SYS_fchownat, AT_FDCWD, path, uid, gid, 0), "fchownat");
  require_zero(syscall(SYS_faccessat, AT_FDCWD, path, R_OK | W_OK),
               "faccessat");

  long fchmodat2_result = syscall(SYS_fchmodat2, AT_FDCWD, path, 0600, 0);
  if (fchmodat2_result != 0 && errno != ENOSYS) {
    perror("fchmodat2");
    return 1;
  }

  require_zero(link(path, hardlink_path), "link");
  require_zero(symlink(path, symlink_path), "symlink");
  require_zero(lchown(symlink_path, uid, gid), "lchown");

  const char *name = "user.hermit";
  const char value[] = "metadata";
  char value_buffer[32] = {0};
  char list_buffer[128] = {0};
  require_xattr_result(setxattr(path, name, value, sizeof(value), 0),
                       "setxattr");
  require_xattr_result(getxattr(path, name, value_buffer, sizeof(value_buffer)),
                       "getxattr");
  require_xattr_result(listxattr(path, list_buffer, sizeof(list_buffer)),
                       "listxattr");
  require_xattr_result(removexattr(path, name), "removexattr");

  require_xattr_result(fsetxattr(fd, name, value, sizeof(value), 0),
                       "fsetxattr");
  require_xattr_result(fgetxattr(fd, name, value_buffer, sizeof(value_buffer)),
                       "fgetxattr");
  require_xattr_result(flistxattr(fd, list_buffer, sizeof(list_buffer)),
                       "flistxattr");
  require_xattr_result(fremovexattr(fd, name), "fremovexattr");

  require_xattr_result(lsetxattr(symlink_path, name, value, sizeof(value), 0),
                       "lsetxattr");
  require_xattr_result(
      lgetxattr(symlink_path, name, value_buffer, sizeof(value_buffer)),
      "lgetxattr");
  require_xattr_result(
      llistxattr(symlink_path, list_buffer, sizeof(list_buffer)), "llistxattr");
  require_xattr_result(lremovexattr(symlink_path, name), "lremovexattr");

  void *mapping = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  if (mapping == MAP_FAILED) {
    perror("mmap");
    return 1;
  }
  memcpy(mapping, "sync", 4);
  require_zero(msync(mapping, 4096, MS_SYNC), "msync");
  require_zero(munmap(mapping, 4096), "munmap");

  require_zero(syscall(SYS_readahead, fd, 0, 4096), "readahead");
  require_zero(syscall(SYS_sync_file_range, fd, 0, 4096, SYNC_FILE_RANGE_WRITE),
               "sync_file_range");
  require_zero(syscall(SYS_syncfs, fd), "syncfs");

  require_zero(close(fd), "close");
  require_zero(unlink(symlink_path), "unlink symlink");
  require_zero(unlink(hardlink_path), "unlink hardlink");
  require_zero(unlink(path), "unlink file");
  puts("syscall-file-metadata-ok count=21");
  return 0;
}
