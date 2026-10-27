/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Inotify rename cookies in the events a read copies before it faults. Linux
// copies whole events in order and stores every byte up to the first one the
// guest cannot write, then fails with EFAULT (or, for a readv whose later
// iovec faults, returns the earlier iovecs' count). The events it copied are
// left in the buffer, with the cookie every other instance sees for their
// rename; that agreement is all the guest prints about rename cookies (Linux
// draws them from one host-wide counter, so their values differ from run to
// run; under Hermit, `--verify-strict` also compares them). Each case puts
// the end of the buffer at a different place: after one event and part of
// the next event's header, inside its cookie, inside its length, at a
// read-only page instead of an inaccessible one, and after more than 64 KiB
// of events. An IN_IGNORED event, whose cookie is 0, cut inside its cookie
// leaves the zeros Linux wrote.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/mman.h>
#include <sys/uio.h>
#include <unistd.h>

// Every name here is two characters, which Linux pads to 16 bytes, so every
// event is 32 bytes.
#define EVENT 32
// Enough renames that their events fill more than 64 KiB.
#define MANY 1100

static char dir[] = "/tmp/inotify-faults-XXXXXX";
static long page;

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

static uint32_t mask_at(const char *p) {
  return ((const struct inotify_event *)p)->mask;
}

// `pages` pages, the last one protected with `last`, and the address
// `writable` bytes before it, filled with 0xa5.
static char *edge(int pages, int last, int writable) {
  char *map = mmap(NULL, pages * page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (map == MAP_FAILED || mprotect(map + (pages - 1) * page, page, last) != 0) exit(4);
  char *at = map + (pages - 1) * page - writable;
  memset(at, 0xa5, writable);
  return at;
}

// One read of `count` bytes from `fd` into `at` that faults: whether it
// failed with EFAULT.
static int faults(int fd, char *at, size_t count) {
  return read(fd, at, count) == -1 && errno == EFAULT;
}

int main(void) {
  int all = 1;
  page = sysconf(_SC_PAGESIZE);
  if (!mkdtemp(dir)) return 1;
  char path[64];
  snprintf(path, sizeof path, "%s/f0", dir);
  int fd = open(path, O_CREAT | O_WRONLY | O_CLOEXEC, 0644);
  if (fd < 0 || close(fd) != 0) return 1;

  // Five instances see the same two renames (f0 -> f1, f1 -> f2): A reads
  // them whole; the others read them into buffers that end early.
  int a = watch(), b = watch(), c = watch(), d = watch(), v = watch();
  rename_in_dir("f0", "f1");
  rename_in_dir("f1", "f2");
  char whole[4 * EVENT] __attribute__((aligned(8)));
  int ok = read(a, whole, sizeof whole) == 4 * EVENT;
  uint32_t c1 = cookie_at(whole), c2 = cookie_at(whole + 2 * EVENT);
  report("the reference instance reads both renames",
         ok && c1 && c1 == cookie_at(whole + EVENT) && c2 && c2 == cookie_at(whole + 3 * EVENT) &&
             c1 != c2,
         &all);

  // 40 writable bytes: one event, then the next event's wd and mask.
  char *at = edge(2, PROT_NONE, 40);
  report("a read that faults after one event fails with EFAULT", faults(b, at, 2 * EVENT), &all);
  report("the event it copied has the rename's cookie", cookie_at(at) == c1, &all);

  // 44 writable bytes: the next event's cookie whole, then its length cut.
  at = edge(2, PROT_NONE, 44);
  report("a read that faults in the next event's length fails with EFAULT",
         faults(c, at, 2 * EVENT), &all);
  report("both cookies it copied whole are the rename's",
         cookie_at(at) == c1 && cookie_at(at + EVENT) == c1, &all);

  // 42 writable bytes before a read-only page: the next event's cookie cut.
  at = edge(2, PROT_READ, 42);
  report("a read that faults at a read-only page fails with EFAULT", faults(d, at, 2 * EVENT),
         &all);
  report("the event before the cut cookie has the rename's cookie", cookie_at(at) == c1, &all);

  // A readv whose first iovec faults fails with EFAULT, as a read does.
  at = edge(2, PROT_NONE, 40);
  char spare[EVENT];
  struct iovec iov[2] = {{at, 2 * EVENT}, {spare, sizeof spare}};
  report("a readv whose first iovec faults fails with EFAULT",
         readv(v, iov, 2) == -1 && errno == EFAULT, &all);
  report("the event its first iovec holds has the rename's cookie", cookie_at(at) == c1, &all);

  // An IN_IGNORED event, whose cookie is 0, read into a buffer that ends
  // inside its cookie: the bytes Linux wrote there are zeros.
  int g = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
  int w = g < 0 ? -1 : inotify_add_watch(g, dir, IN_ATTRIB);
  if (w < 0 || inotify_rm_watch(g, w) != 0) return 5;
  at = edge(2, PROT_NONE, 10);
  report("a read that faults inside an IN_IGNORED event's cookie fails with EFAULT",
         faults(g, at, 16), &all);
  report("the cookie bytes it wrote are zeros",
         (mask_at(at) & IN_IGNORED) && at[8] == 0 && at[9] == 0, &all);

  // More than 64 KiB of events before the fault: the renames f2 -> f3 and
  // back, MANY times, read whole by A and into 18 pages, the last one
  // inaccessible, by M.
  int m = watch();
  for (int i = 0; i < MANY; i++) rename_in_dir(i % 2 ? "f3" : "f2", i % 2 ? "f2" : "f3");
  size_t size = (size_t)2 * MANY * EVENT;
  char *reference = malloc(size);
  ok = reference && read(a, reference, size) == (ssize_t)size;
  at = edge(18, PROT_NONE, 17 * page);
  ok = ok && faults(m, at, size);
  // The last event the copy wrote whole, and with it the last whole cookie.
  size_t last = ((size_t)17 * page / EVENT - 1) * EVENT;
  report("a read that faults after more than 64 KiB of events fails with EFAULT", ok, &all);
  report("its first and last whole events have the renames' cookies",
         ok && cookie_at(at) == cookie_at(reference) && cookie_at(at + last) == cookie_at(reference + last),
         &all);

  snprintf(path, sizeof path, "%s/f2", dir);
  unlink(path);
  rmdir(dir);
  return all ? 0 : 9;
}
