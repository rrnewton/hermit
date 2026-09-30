// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

// ONE FILE IDENTITY IS A DEVICE AND AN INODE, NEVER AN INODE ALONE.
//
// Usage: cross_device_inode_identity probe|check DIR_A DIR_B
//
// DIR_A and DIR_B must be the roots of two fresh tmpfs mounts. Since Linux 5.9
// each tmpfs numbers its own inodes from 1, so the first file created in each,
// DIR_A/f and DIR_B/g, carries the same inode number on two different
// devices. That is ordinary Linux: inode numbers are unique per filesystem.
//
// Detcore used to key its deterministic inodes on the raw inode number alone
// (https://github.com/rrnewton/hermit/issues/3307). The two files became one
// object to it, so a write to DIR_A/f changed the modification time `stat`
// reported for DIR_B/g, a file nobody wrote. Across runs the same conflation
// made /proc/self/maps lines depend on which host counters happened to
// coincide.
//
// `probe` prints the identities it observes and checks nothing. Run under
// `--no-virtualize-metadata`, it shows the raw collision this fixture relies
// on, so the owning test can refuse a host where the collision does not occur
// instead of passing without having exercised it.
//
// `check` asserts only things that hold on native Linux too: writing one file
// changes that file's mtime and not the other's, and each file's
// /proc/self/maps line reports the device and inode that `stat` reports.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <time.h>
#include <unistd.h>

#define FILE_SIZE 4096

#define CHECK(condition)                                                    \
  do {                                                                      \
    if (!(condition)) {                                                     \
      fprintf(                                                              \
          stderr, "line %d: %s (errno=%d)\n", __LINE__, #condition, errno); \
      exit(1);                                                              \
    }                                                                       \
  } while (0)

static int create_sized(const char* dir, const char* name) {
  char path[4096];
  CHECK(snprintf(path, sizeof(path), "%s/%s", dir, name) < (int)sizeof(path));
  int fd = open(path, O_RDWR | O_CREAT | O_EXCL | O_CLOEXEC, 0644);
  CHECK(fd >= 0);
  CHECK(ftruncate(fd, FILE_SIZE) == 0);
  return fd;
}

static struct stat fstat_or_die(int fd) {
  struct stat st;
  CHECK(fstat(fd, &st) == 0);
  return st;
}

static int same_mtime(struct stat left, struct stat right) {
  return left.st_mtim.tv_sec == right.st_mtim.tv_sec &&
      left.st_mtim.tv_nsec == right.st_mtim.tv_nsec;
}

// Find the /proc/self/maps header covering `addr`; return its dev and inode.
static void
maps_identity(const void* addr, dev_t* dev_out, unsigned long* inode_out) {
  FILE* maps = fopen("/proc/self/maps", "r");
  CHECK(maps != NULL);
  char line[4096];
  while (fgets(line, sizeof(line), maps)) {
    uintptr_t start = 0, end = 0;
    unsigned dev_major = 0, dev_minor = 0;
    unsigned long inode = 0;
    if (sscanf(
            line,
            "%lx-%lx %*4s %*x %x:%x %lu",
            &start,
            &end,
            &dev_major,
            &dev_minor,
            &inode) != 5) {
      continue;
    }
    if ((uintptr_t)addr >= start && (uintptr_t)addr < end) {
      *dev_out = makedev(dev_major, dev_minor);
      *inode_out = inode;
      fclose(maps);
      return;
    }
  }
  fclose(maps);
  fprintf(stderr, "no /proc/self/maps line covers %p\n", addr);
  exit(1);
}

static void check_maps_agrees_with_stat(
    const char* name,
    const void* mapping,
    struct stat st) {
  dev_t dev = 0;
  unsigned long inode = 0;
  maps_identity(mapping, &dev, &inode);
  if (dev != st.st_dev || inode != (unsigned long)st.st_ino) {
    fprintf(
        stderr,
        "%s: maps reports %x:%x inode %lu but stat reports %x:%x inode "
        "%lu\n",
        name,
        major(dev),
        minor(dev),
        inode,
        major(st.st_dev),
        minor(st.st_dev),
        (unsigned long)st.st_ino);
    exit(1);
  }
}

int main(int argc, char** argv) {
  if (argc != 4 ||
      (strcmp(argv[1], "probe") != 0 && strcmp(argv[1], "check") != 0)) {
    fprintf(stderr, "usage: %s probe|check DIR_A DIR_B\n", argv[0]);
    return 2;
  }
  int fd_f = create_sized(argv[2], "f");
  int fd_g = create_sized(argv[3], "g");
  struct stat f_before = fstat_or_die(fd_f);
  struct stat g_before = fstat_or_die(fd_g);

  if (strcmp(argv[1], "probe") == 0) {
    printf(
        "f dev=%x:%x ino=%lu\n",
        major(f_before.st_dev),
        minor(f_before.st_dev),
        (unsigned long)f_before.st_ino);
    printf(
        "g dev=%x:%x ino=%lu\n",
        major(g_before.st_dev),
        minor(g_before.st_dev),
        (unsigned long)g_before.st_ino);
    return 0;
  }

  // The two files live on the two mounts; distinct devices are what make an
  // equal raw inode number an ordinary, legal coincidence.
  CHECK(f_before.st_dev != g_before.st_dev);

  void* map_f = mmap(NULL, FILE_SIZE, PROT_READ, MAP_SHARED, fd_f, 0);
  CHECK(map_f != MAP_FAILED);
  void* map_g = mmap(NULL, FILE_SIZE, PROT_READ, MAP_SHARED, fd_g, 0);
  CHECK(map_g != MAP_FAILED);
  check_maps_agrees_with_stat("f", map_f, f_before);
  check_maps_agrees_with_stat("g", map_g, g_before);

  // Let the clock move past the creation timestamps (a coarse host clock
  // would otherwise leave f's mtime unchanged on native Linux).
  struct timespec pause = {.tv_sec = 0, .tv_nsec = 20 * 1000 * 1000};
  CHECK(nanosleep(&pause, NULL) == 0);
  CHECK(pwrite(fd_f, "x", 1, 0) == 1);

  struct stat f_after = fstat_or_die(fd_f);
  struct stat g_after = fstat_or_die(fd_g);
  // Without this the check below could pass because no write was observed.
  if (same_mtime(f_before, f_after)) {
    fprintf(
        stderr,
        "writing f did not change f's mtime (%ld.%09ld)\n",
        (long)f_after.st_mtim.tv_sec,
        f_after.st_mtim.tv_nsec);
    return 1;
  }
  if (!same_mtime(g_before, g_after)) {
    fprintf(
        stderr,
        "writing f changed the mtime of g, a different file on another "
        "device: %ld.%09ld -> %ld.%09ld\n",
        (long)g_before.st_mtim.tv_sec,
        g_before.st_mtim.tv_nsec,
        (long)g_after.st_mtim.tv_sec,
        g_after.st_mtim.tv_nsec);
    return 1;
  }
  check_maps_agrees_with_stat("f", map_f, f_after);
  check_maps_agrees_with_stat("g", map_g, g_after);

  printf("cross-device files keep separate identities\n");
  return 0;
}
