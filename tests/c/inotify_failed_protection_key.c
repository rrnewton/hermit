/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A protection-key allocation that fails allocates nothing, so it must not
// change what happens to a later inotify read that faults. Linux refuses
// pkey_alloc with an unknown flag (EINVAL), and a backend without protection
// keys refuses every call; either way no key exists, and the read below,
// which faults 40 bytes before a read-only page after copying one event,
// leaves that event with the cookie every other instance sees for its rename.
//
// The guest prints the cookies themselves. Linux draws them from one
// host-wide counter, so natively they are large and differ from run to run;
// under Hermit they are the run's virtual cookies, numbered from 1 in the
// order the run first reads them, so the two renames here are 1 and 2, and a
// host cookie left in place shows as a large number that differs between the
// two runs of a verify pair.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

#define EVENT 32

static char dir[] = "/tmp/inotify-failed-protection-key-XXXXXX";

static int watch(void) {
  int fd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
  if (fd < 0 || inotify_add_watch(fd, dir, IN_MOVED_FROM | IN_MOVED_TO) < 0) exit(2);
  return fd;
}

static void rename_in_dir(const char *from, const char *to) {
  char old[64], new[64];
  snprintf(old, sizeof old, "%s/%s", dir, from);
  snprintf(new, sizeof new, "%s/%s", dir, to);
  if (rename(old, new) != 0) exit(3);
}

static void report(const char *what, int ok, int *all) {
  printf("%s: %s\n", what, ok ? "yes" : "no");
  *all = *all && ok;
}

static uint32_t cookie_at(const char *p) {
  return ((const struct inotify_event *)p)->cookie;
}

int main(void) {
  int all = 1;
  long page = sysconf(_SC_PAGESIZE);
  if (!mkdtemp(dir)) return 1;
  char path[64];
  snprintf(path, sizeof path, "%s/f0", dir);
  int fd = open(path, O_CREAT | O_WRONLY | O_CLOEXEC, 0644);
  if (fd < 0 || close(fd) != 0) return 1;

  int reference = watch(), faulting = watch();
  rename_in_dir("f0", "f1");
  rename_in_dir("f1", "f2");
  char whole[4 * EVENT] __attribute__((aligned(8)));
  int ok = read(reference, whole, sizeof whole) == 4 * EVENT;
  uint32_t c1 = cookie_at(whole);
  report("the reference instance reads both renames",
         ok && c1 && c1 == cookie_at(whole + EVENT) && cookie_at(whole + 2 * EVENT) &&
             cookie_at(whole + 2 * EVENT) == cookie_at(whole + 3 * EVENT),
         &all);
  printf("the renames' cookies: %u %u\n", (unsigned)c1, (unsigned)cookie_at(whole + 2 * EVENT));

  // The raw call, so the C library cannot refuse the flag itself. Only the
  // failure is printed: its errno differs between kernels and backends.
  long key = syscall(SYS_pkey_alloc, 1UL, 0UL);
  report("a protection-key allocation with an unknown flag fails", key == -1, &all);

  // 40 writable bytes before a read-only page: one event, then the next
  // event's wd and mask.
  char *map = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (map == MAP_FAILED || mprotect(map + page, page, PROT_READ) != 0) return 4;
  char *at = map + page - 40;
  memset(at, 0xa5, 40);
  report("a read that faults after one event fails with EFAULT",
         read(faulting, at, 2 * EVENT) == -1 && errno == EFAULT, &all);
  report("the event it copied has the rename's cookie", cookie_at(at) == c1, &all);
  printf("its cookie: %u\n", (unsigned)cookie_at(at));

  snprintf(path, sizeof path, "%s/f2", dir);
  unlink(path);
  rmdir(dir);
  return all ? 0 : 9;
}
