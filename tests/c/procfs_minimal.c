/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/vfs.h>
#include <unistd.h>

#define MAPS_CONTENT "00400000-00401000 r-xp 00000000 00:00 0 [hermit]\n"

#ifndef RESOLVE_BENEATH
#define RESOLVE_BENEATH 0x08
#endif

struct open_how_compat {
  uint64_t flags;
  uint64_t mode;
  uint64_t resolve;
};

struct open_how_large {
  struct open_how_compat how;
  uint64_t extra;
};

static void fail(const char *operation) {
  perror(operation);
  exit(1);
}

static void require_errno(const char *operation, long result, int expected) {
  if (result != -1 || errno != expected) {
    fprintf(stderr, "%s: result=%ld errno=%d expected=%d\n", operation, result,
            errno, expected);
    exit(1);
  }
}

static void require_file_type(const char *operation, int result, mode_t actual,
                              mode_t expected) {
  if (result != 0 || (actual & S_IFMT) != expected) {
    fprintf(stderr, "%s: result=%d errno=%d mode=%o expected=%o\n", operation,
            result, errno, actual, expected);
    exit(1);
  }
}

static void check_maps_fd(int fd) {
  char buffer[sizeof(MAPS_CONTENT)] = {0};
  const size_t expected_size = sizeof(MAPS_CONTENT) - 1;
  struct stat metadata;
  if (fstat(fd, &metadata) != 0) {
    fail("fstat maps");
  }
  if (!S_ISREG(metadata.st_mode) || (size_t)metadata.st_size != expected_size) {
    fprintf(stderr, "unexpected maps metadata: mode=%o size=%ld\n",
            metadata.st_mode, (long)metadata.st_size);
    exit(1);
  }
  ssize_t size = read(fd, buffer, sizeof(buffer));
  if (size != (ssize_t)expected_size ||
      memcmp(buffer, MAPS_CONTENT, expected_size) != 0) {
    fprintf(stderr, "read returned unexpected maps content\n");
    exit(1);
  }
  if (read(fd, buffer, sizeof(buffer)) != 0) {
    fprintf(stderr, "maps did not reach EOF\n");
    exit(1);
  }
  memset(buffer, 0, sizeof(buffer));
  size = pread(fd, buffer, sizeof(buffer), 0);
  if (size != (ssize_t)expected_size ||
      memcmp(buffer, MAPS_CONTENT, expected_size) != 0) {
    fprintf(stderr, "pread returned unexpected maps content\n");
    exit(1);
  }
}

int main(void) {
  int fd = open("/proc/self/maps", O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    fail("open maps");
  }
  check_maps_fd(fd);
  if (close(fd) != 0) {
    fail("close maps");
  }

  errno = 0;
  require_errno("write-open maps", open("/proc/self/maps", O_WRONLY), EACCES);
  errno = 0;
  require_errno("open hidden", open("/proc/meminfo", O_RDONLY), ENOENT);
  errno = 0;
  require_errno("normalized open hidden",
                open("/tmp/../proc/meminfo", O_RDONLY), ENOENT);

  char alias[128];
  char aliased_meminfo[160];
  snprintf(alias, sizeof(alias), "/tmp/hermit-procfs-alias-%ld",
           (long)getpid());
  unlink(alias);
  if (symlink("/proc", alias) != 0) {
    fail("create proc alias");
  }
  snprintf(aliased_meminfo, sizeof(aliased_meminfo), "%s/meminfo", alias);
  errno = 0;
  require_errno("aliased open hidden", open(aliased_meminfo, O_RDONLY),
                ENOENT);
  if (unlink(alias) != 0) {
    fail("remove proc alias");
  }

  if (chdir("/proc") != 0) {
    fail("chdir proc");
  }
  errno = 0;
  require_errno("relative open hidden", open("meminfo", O_RDONLY), ENOENT);
  fd = open("self/maps", O_RDONLY);
  if (fd < 0) {
    fail("relative open maps");
  }
  check_maps_fd(fd);
  if (close(fd) != 0 || chdir("/") != 0) {
    fail("restore cwd");
  }

  int rootfd = open("/", O_PATH | O_DIRECTORY);
  if (rootfd < 0) {
    fail("open root dirfd");
  }
  errno = 0;
  require_errno("dirfd open hidden",
                openat(rootfd, "proc/self/environ", O_RDONLY), ENOENT);
  if (close(rootfd) != 0) {
    fail("close root dirfd");
  }

  struct stat metadata;
  if (stat("/proc/self/stat", &metadata) != 0 ||
      !S_ISREG(metadata.st_mode) || metadata.st_size <= 0) {
    fail("stat virtual file");
  }
  errno = 0;
  require_errno("stat hidden", stat("/proc/meminfo", &metadata), ENOENT);
  errno = 0;
  require_errno("fstatat hidden",
                fstatat(AT_FDCWD, "/proc/self/environ", &metadata, 0), ENOENT);

  int metadata_result = stat("/proc/self/exe", &metadata);
  require_file_type("stat self exe", metadata_result, metadata.st_mode, S_IFREG);
  metadata_result = lstat("/proc/self/exe", &metadata);
  require_file_type("lstat self exe", metadata_result, metadata.st_mode, S_IFLNK);
  metadata_result = fstatat(AT_FDCWD, "/proc/self/exe", &metadata, 0);
  require_file_type(
      "fstatat follow self exe", metadata_result, metadata.st_mode, S_IFREG);
  metadata_result = fstatat(AT_FDCWD, "/proc/self/exe", &metadata,
                            AT_SYMLINK_NOFOLLOW);
  require_file_type(
      "fstatat nofollow self exe", metadata_result, metadata.st_mode, S_IFLNK);

  struct statfs statfs_buffer;
  errno = 0;
  require_errno("statfs hidden", statfs("/proc/meminfo", &statfs_buffer), ENOENT);
  errno = 0;
  require_errno("statfs virtual", statfs("/proc/self/maps", &statfs_buffer), ENOENT);

  char link_buffer[4096];
  ssize_t link_size =
      readlink("/proc/self/exe", link_buffer, sizeof(link_buffer) - 1);
  if (link_size <= 0) {
    fail("readlink self exe");
  }
  link_buffer[link_size] = '\0';
  if (strstr(link_buffer, "procfs-minimal") == NULL) {
    fprintf(stderr, "unexpected self exe target: %s\n", link_buffer);
    exit(1);
  }
  errno = 0;
  require_errno("readlinkat virtual file",
                readlinkat(AT_FDCWD, "/proc/self/maps", link_buffer,
                           sizeof(link_buffer)),
                EINVAL);

  if (access("/proc/cpuinfo", R_OK) != 0) {
    fail("access virtual file");
  }
  errno = 0;
  require_errno("access virtual write", access("/proc/cpuinfo", W_OK),
                EACCES);
  errno = 0;
  require_errno("faccessat hidden",
                syscall(SYS_faccessat, AT_FDCWD, "/proc/meminfo", F_OK),
                ENOENT);

#ifdef SYS_faccessat2
  if (syscall(SYS_faccessat2, AT_FDCWD, "/proc/cpuinfo", R_OK, 0) != 0) {
    fail("faccessat2 virtual file");
  }
  errno = 0;
  require_errno("faccessat2 hidden",
                syscall(SYS_faccessat2, AT_FDCWD, "/proc/meminfo", F_OK, 0),
                ENOENT);
  errno = 0;
  require_errno("faccessat2 invalid flags",
                syscall(SYS_faccessat2, AT_FDCWD, "/proc/cpuinfo", F_OK, 1 << 30),
                EINVAL);
#endif

#ifdef SYS_statx
  struct statx statx_buffer;
  if (syscall(SYS_statx, AT_FDCWD, "/proc/cpuinfo", 0, STATX_BASIC_STATS,
              &statx_buffer) != 0 || statx_buffer.stx_size == 0) {
    fail("statx virtual file");
  }
  errno = 0;
  require_errno("statx hidden",
                syscall(SYS_statx, AT_FDCWD, "/proc/meminfo", 0,
                        STATX_BASIC_STATS, &statx_buffer),
                ENOENT);

  int statx_result = syscall(SYS_statx, AT_FDCWD, "/proc/self/exe", 0,
                             STATX_TYPE, &statx_buffer);
  require_file_type("statx follow self exe", statx_result, statx_buffer.stx_mode,
                    S_IFREG);
  statx_result = syscall(SYS_statx, AT_FDCWD, "/proc/self/exe",
                         AT_SYMLINK_NOFOLLOW, STATX_TYPE, &statx_buffer);
  require_file_type("statx nofollow self exe", statx_result,
                    statx_buffer.stx_mode, S_IFLNK);
#endif

#ifdef SYS_openat2
  struct open_how_compat how = {.flags = O_RDONLY | O_CLOEXEC};
  errno = 0;
  require_errno("openat2 size zero",
                syscall(SYS_openat2, AT_FDCWD, "/proc/self/maps", &how, 0),
                EINVAL);
  how.flags = UINT64_C(1) << 63;
  errno = 0;
  require_errno("openat2 unknown flags",
                syscall(SYS_openat2, AT_FDCWD, "/proc/self/maps", &how,
                        sizeof(how)),
                EINVAL);
  how.flags = O_RDONLY | O_CLOEXEC;
  how.mode = 0600;
  errno = 0;
  require_errno("openat2 mode without create",
                syscall(SYS_openat2, AT_FDCWD, "/proc/self/maps", &how,
                        sizeof(how)),
                EINVAL);
  how.mode = 0;
  how.resolve = RESOLVE_BENEATH;
  errno = 0;
  require_errno("openat2 resolve beneath",
                syscall(SYS_openat2, AT_FDCWD, "/proc/self/maps", &how,
                        sizeof(how)),
                EXDEV);
  how.resolve = UINT64_C(1) << 63;
  errno = 0;
  require_errno("openat2 unknown resolve",
                syscall(SYS_openat2, AT_FDCWD, "/proc/self/maps", &how,
                        sizeof(how)),
                EINVAL);
  how.resolve = 0;
  struct open_how_large too_large = {.how = how, .extra = 1};
  errno = 0;
  require_errno("openat2 nonzero extension",
                syscall(SYS_openat2, AT_FDCWD, "/proc/self/maps", &too_large,
                        sizeof(too_large)),
                E2BIG);

  fd = syscall(SYS_openat2, AT_FDCWD, "/proc/self/maps", &how, sizeof(how));
  if (fd < 0) {
    fail("openat2 maps");
  }
  check_maps_fd(fd);
  if (close(fd) != 0) {
    fail("close openat2 maps");
  }
  errno = 0;
  require_errno("openat2 hidden",
                syscall(SYS_openat2, AT_FDCWD, "/proc/meminfo", &how,
                        sizeof(how)),
                ENOENT);
#endif

  puts("procfs-policy:ok");
  return 0;
}
