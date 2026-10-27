/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Inotify reads whose copy Hermit cannot account for, which must leave the
// buffer exactly as Linux left it.
//
// Reads of a blocking inotify descriptor (inotify_init's default) that Linux
// refuses before it reads the queue: a count that runs past the end of the
// address space, a readv whose second iovec lies outside it, and a readv whose
// iovec array the guest cannot read. Each fails with EFAULT, copies nothing
// and dequeues nothing, so the buffer, which already holds events read
// earlier, cookies included, must stay exactly as it was, and the queue must
// still hold every event. A later read returns them with the cookies another
// instance sees for their renames.
//
// And a read into a page that a protection key keeps the guest from writing:
// Linux takes the event from the queue, faults at the buffer's first byte and
// copies nothing, so the twelve bytes of an earlier event header there, cookie
// included, must stay as they were.

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
#include <sys/uio.h>
#include <unistd.h>

// Every name here is two characters, which Linux pads to 16 bytes, so every
// event is 32 bytes.
#define EVENT 32

static char dir[] = "/tmp/inotify-blocking-faults-XXXXXX";

static int watch(int flags) {
  int fd = inotify_init1(flags | IN_CLOEXEC);
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

  // A (nonblocking) and X (blocking) see the same two renames.
  int a = watch(IN_NONBLOCK), x = watch(0);
  rename_in_dir("f0", "f1");
  rename_in_dir("f1", "f2");
  char stale[4 * EVENT] __attribute__((aligned(8)));
  int ok = read(a, stale, sizeof stale) == 4 * EVENT;
  uint32_t c1 = cookie_at(stale), c2 = cookie_at(stale + 2 * EVENT);
  report("the reference instance reads both renames",
         ok && c1 && c1 == cookie_at(stale + EVENT) && c2 && c2 == cookie_at(stale + 3 * EVENT) &&
             c1 != c2,
         &all);

  // The buffer holds the events A read.
  char buffer[4 * EVENT] __attribute__((aligned(8)));
  memcpy(buffer, stale, sizeof buffer);
  // The raw call: the C library's declaration lets the compiler refuse a
  // count larger than the buffer, which is the point here.
  ok = syscall(SYS_read, x, buffer, (size_t)1 << 62) == -1 && errno == EFAULT;
  report("a read whose count runs past the address space fails with EFAULT and leaves the buffer",
         ok && memcmp(buffer, stale, sizeof buffer) == 0, &all);

  struct iovec bad[2] = {{buffer, sizeof buffer}, {(void *)0xffff800000000000ull, EVENT}};
  ok = readv(x, bad, 2) == -1 && errno == EFAULT;
  report("a readv with a second iovec outside the address space fails with EFAULT and leaves the buffer",
         ok && memcmp(buffer, stale, sizeof buffer) == 0, &all);

  struct iovec *hidden = mmap(NULL, page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (hidden == MAP_FAILED) return 4;
  hidden[0] = (struct iovec){buffer, sizeof buffer};
  if (mprotect(hidden, page, PROT_NONE) != 0) return 4;
  ok = readv(x, hidden, 1) == -1 && errno == EFAULT;
  report("a readv whose iovec array the guest cannot read fails with EFAULT and leaves the buffer",
         ok && memcmp(buffer, stale, sizeof buffer) == 0, &all);

  // Nothing was dequeued: one read returns all four events, which are
  // queued, so it does not wait.
  char later[4 * EVENT] __attribute__((aligned(8)));
  ok = read(x, later, sizeof later) == 4 * EVENT;
  report("the queue still holds every event, with the renames' cookies",
         ok && cookie_at(later) == c1 && cookie_at(later + EVENT) == c1 &&
             cookie_at(later + 2 * EVENT) == c2 && cookie_at(later + 3 * EVENT) == c2,
         &all);

  // One 16-byte IN_IGNORED event, queued on a nonblocking instance, read into
  // twelve stale header bytes before a read-only page; a protection key keeps
  // the guest from writing the buffer's own page.
  int n = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
  int w = n < 0 ? -1 : inotify_add_watch(n, dir, IN_ATTRIB);
  if (w < 0 || inotify_rm_watch(n, w) != 0) return 5;
  char *map = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (map == MAP_FAILED || mprotect(map + page, page, PROT_READ) != 0) return 4;
  char *header = map + page - 12;
  struct inotify_event earlier = {1, IN_MOVED_FROM, 4242, 0};
  memcpy(header, &earlier, 12);
  int pkey = pkey_alloc(0, PKEY_DISABLE_WRITE);
  if (pkey < 0 || pkey_mprotect(map, page, PROT_READ | PROT_WRITE, pkey) != 0) return 6;
  ok = read(n, header, 16) == -1 && errno == EFAULT;
  report("a read into a page a protection key keeps the guest from writing fails with EFAULT and leaves the buffer",
         ok && memcmp(header, &earlier, 12) == 0, &all);

  snprintf(path, sizeof path, "%s/f2", dir);
  unlink(path);
  rmdir(dir);
  return all ? 0 : 9;
}
