/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Inotify rename cookies: two instances watch one directory while it renames
// a file twice. Instance A is read with read(2), instance B with readv(2)
// split across three iovecs. Linux pairs each IN_MOVED_FROM with its
// IN_MOVED_TO by a cookie drawn from one host-wide counter, so the cookie
// values depend on every other rename on the host; the guest prints only
// what Linux guarantees: each pair shares a nonzero cookie, the two renames
// differ, and both instances see the same cookie for a rename. Under Hermit
// the values themselves must also be the same in every run, which
// `--verify-strict` checks through the logged read buffers.

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
#include <unistd.h>

#define MAX_EVENTS 8

struct observed {
  uint32_t mask[MAX_EVENTS];
  uint32_t cookie[MAX_EVENTS];
  char name[MAX_EVENTS][16];
  int count;
};

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

int main(void) {
  char dir[] = "/tmp/inotify-cookies-XXXXXX";
  if (!mkdtemp(dir)) return 1;
  int a = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
  int b = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
  if (a < 0 || b < 0) return 2;
  if (inotify_add_watch(a, dir, IN_MOVED_FROM | IN_MOVED_TO) < 0) return 3;
  if (inotify_add_watch(b, dir, IN_MOVED_FROM | IN_MOVED_TO) < 0) return 3;

  char first[64], second[64], third[64];
  snprintf(first, sizeof first, "%s/first", dir);
  snprintf(second, sizeof second, "%s/second", dir);
  snprintf(third, sizeof third, "%s/third", dir);
  int fd = open(first, O_CREAT | O_WRONLY | O_CLOEXEC, 0644);
  if (fd < 0 || close(fd) != 0) return 4;
  if (rename(first, second) != 0 || rename(second, third) != 0) return 5;

  char abuf[4096] __attribute__((aligned(8)));
  ssize_t alen = read(a, abuf, sizeof abuf);
  // Three iovecs. Inotify has no read_iter, so readv reads each iovec with
  // its own read, which takes only whole events: one event (32 bytes with
  // these names) fills the first iovec, two the second, and the rest go to
  // the third, so the cookies are scattered over all three.
  char bbuf[4096] __attribute__((aligned(8)));
  struct iovec iov[3] = {
      {bbuf, 32},
      {bbuf + 32, 64},
      {bbuf + 96, sizeof bbuf - 96},
  };
  ssize_t blen = readv(b, iov, 3);
  if (alen <= 0 || blen <= 0) {
    printf("read a=%zd b=%zd errno=%d\n", alen, blen, errno);
    return 6;
  }

  struct observed oa, ob;
  if (parse(abuf, alen, &oa) != 0 || parse(bbuf, blen, &ob) != 0) return 7;
  printf("events a=%d b=%d\n", oa.count, ob.count);
  for (int i = 0; i < oa.count; i++) {
    printf("a %d %s %s\n", i, oa.mask[i] & IN_MOVED_FROM ? "from" : "to", oa.name[i]);
  }
  int ok = oa.count == 4 && ob.count == 4;
  for (int i = 0; ok && i < 4; i++) {
    ok = oa.cookie[i] != 0 && oa.cookie[i] == ob.cookie[i] && oa.mask[i] == ob.mask[i] &&
         strcmp(oa.name[i], ob.name[i]) == 0;
  }
  ok = ok && oa.cookie[0] == oa.cookie[1] && oa.cookie[2] == oa.cookie[3] &&
       oa.cookie[0] != oa.cookie[2];
  printf("pairs share a cookie, renames differ, instances agree: %s\n", ok ? "yes" : "no");

  unlink(third);
  rmdir(dir);
  return ok ? 0 : 8;
}
