/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Inotify rename cookies. Linux pairs each IN_MOVED_FROM with its IN_MOVED_TO
// by a cookie drawn from one host-wide counter, so the values depend on every
// other rename on the host. The guest prints only what Linux guarantees about
// them; under Hermit, `--verify-strict` also checks the values themselves
// through the logged read buffers. What Linux guarantees, checked here:
// - a pair shares one nonzero cookie, even when its halves come from two reads;
// - two renames have different cookies, even when separate reads return them;
// - every instance, in this process or another, sees a rename's cookie.
// Three renames are watched by instance A (read with read(2), then preadv2(2)
// at offset -1), instance B (read last, with readv(2) over three iovecs),
// instance C (created after the first two renames) and instance D (created
// and read by a forked child).

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

#define MAX_EVENTS 8
// Every name here is two characters, which Linux pads to 16 bytes, so every
// event is 32 bytes.
#define EVENT 32
#define BUFFER 4096

struct observed {
  uint32_t mask[MAX_EVENTS];
  uint32_t cookie[MAX_EVENTS];
  char name[MAX_EVENTS][16];
  int count;
};

static char dir[] = "/tmp/inotify-cookies-XXXXXX";

static int parse(const char *buffer, ssize_t length, struct observed *out) {
  out->count = 0;
  for (const char *p = buffer; p < buffer + length;) {
    const struct inotify_event *event = (const struct inotify_event *)p;
    if (out->count == MAX_EVENTS) return -1;
    out->mask[out->count] = event->mask;
    out->cookie[out->count] = event->cookie;
    snprintf(out->name[out->count], sizeof out->name[0], "%s", event->len ? event->name : "");
    out->count++;
    p += sizeof *event + event->len;
  }
  return 0;
}

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

// One read(2) of at most `size` bytes, parsed into `out`.
static void read_events(int fd, size_t size, struct observed *out) {
  char buffer[BUFFER] __attribute__((aligned(8)));
  ssize_t length = read(fd, buffer, size);
  if (length <= 0) {
    printf("read %zd errno %d\n", length, errno);
    exit(4);
  }
  if (parse(buffer, length, out) != 0) exit(5);
}

// Whether `o` holds exactly the pair `from` -> `to`, with one nonzero cookie.
static int is_pair(const struct observed *o, const char *from, const char *to) {
  return o->count == 2 && (o->mask[0] & IN_MOVED_FROM) && (o->mask[1] & IN_MOVED_TO) &&
         !strcmp(o->name[0], from) && !strcmp(o->name[1], to) && o->cookie[0] != 0 &&
         o->cookie[0] == o->cookie[1];
}

static void report(const char *what, int ok, int *all) {
  printf("%s: %s\n", what, ok ? "yes" : "no");
  *all = *all && ok;
}

int main(void) {
  int all = 1;
  if (!mkdtemp(dir)) return 1;
  int a = watch(), b = watch();
  char first[64];
  snprintf(first, sizeof first, "%s/f0", dir);
  int fd = open(first, O_CREAT | O_WRONLY | O_CLOEXEC, 0644);
  if (fd < 0 || close(fd) != 0) return 1;

  // Rename 1, its halves read by two 32-byte reads: one event each.
  rename_in_dir("f0", "f1");
  struct observed from, to;
  read_events(a, EVENT, &from);
  read_events(a, EVENT, &to);
  uint32_t c1 = from.cookie[0];
  report("a pair split over two reads shares one cookie",
         from.count == 1 && to.count == 1 && (from.mask[0] & IN_MOVED_FROM) &&
             (to.mask[0] & IN_MOVED_TO) && c1 != 0 && c1 == to.cookie[0],
         &all);

  // Rename 2, read by a separate read.
  rename_in_dir("f1", "f2");
  struct observed second;
  read_events(a, BUFFER, &second);
  uint32_t c2 = second.cookie[0];
  report("a rename read separately has a cookie of its own", is_pair(&second, "f1", "f2") && c2 != c1,
         &all);

  // Instance C sees only rename 3. So does instance D, which a child creates
  // and reads.
  int c = watch();
  int ready[2], go[2], answer[2];
  if (pipe(ready) || pipe(go) || pipe(answer)) return 1;
  // Nothing buffered for the child to print again if it exits early.
  fflush(stdout);
  pid_t child = fork();
  if (child < 0) return 1;
  if (child == 0) {
    int d = watch();
    char byte = 0;
    if (write(ready[1], &byte, 1) != 1 || read(go[0], &byte, 1) != 1) _exit(1);
    struct observed third;
    read_events(d, BUFFER, &third);
    uint32_t cookie = is_pair(&third, "f2", "f3") ? third.cookie[0] : 0;
    _exit(write(answer[1], &cookie, sizeof cookie) == sizeof cookie ? 0 : 1);
  }
  char byte = 0;
  if (read(ready[0], &byte, 1) != 1) return 1;
  // Rename 3.
  rename_in_dir("f2", "f3");
  if (write(go[1], &byte, 1) != 1) return 1;
  uint32_t child_cookie = 0;
  int status = 0;
  if (read(answer[0], &child_cookie, sizeof child_cookie) != sizeof child_cookie ||
      waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    return 6;
  }

  // Instance A reads rename 3 with preadv2 at the current offset.
  char abuf[BUFFER] __attribute__((aligned(8)));
  struct iovec whole = {abuf, sizeof abuf};
  ssize_t alen = preadv2(a, &whole, 1, -1, 0);
  struct observed third;
  if (alen <= 0 || parse(abuf, alen, &third) != 0) {
    printf("preadv2 %zd errno %d\n", alen, errno);
    return 7;
  }
  uint32_t c3 = third.cookie[0];
  report("preadv2 at offset -1 reads a pair", is_pair(&third, "f2", "f3"), &all);
  report("three renames have three cookies", c3 != c1 && c3 != c2, &all);

  struct observed later;
  read_events(c, BUFFER, &later);
  report("an instance created later agrees", is_pair(&later, "f2", "f3") && later.cookie[0] == c3,
         &all);
  report("a child's instance agrees", child_cookie == c3, &all);

  // Instance B, read last, holds all six events. Three iovecs: inotify has no
  // read_iter, so readv reads each iovec with its own read, which takes only
  // whole events: one event fills the first iovec, two the second, and the
  // other three go to the third.
  char bbuf[BUFFER] __attribute__((aligned(8)));
  struct iovec iov[3] = {
      {bbuf, EVENT},
      {bbuf + EVENT, 2 * EVENT},
      {bbuf + 3 * EVENT, sizeof bbuf - 3 * EVENT},
  };
  ssize_t blen = readv(b, iov, 3);
  struct observed all_b;
  if (blen <= 0 || parse(bbuf, blen, &all_b) != 0) {
    printf("readv %zd errno %d\n", blen, errno);
    return 8;
  }
  const uint32_t expected[6] = {c1, c1, c2, c2, c3, c3};
  int agrees = all_b.count == 6;
  for (int i = 0; agrees && i < 6; i++) agrees = all_b.cookie[i] == expected[i];
  printf("readv events %d:", all_b.count);
  for (int i = 0; i < all_b.count; i++) {
    printf(" %s %s", all_b.mask[i] & IN_MOVED_FROM ? "from" : "to", all_b.name[i]);
  }
  printf("\n");
  report("an instance read last agrees on every rename", agrees, &all);

  char last[64];
  snprintf(last, sizeof last, "%s/f3", dir);
  unlink(last);
  rmdir(dir);
  return all ? 0 : 9;
}
