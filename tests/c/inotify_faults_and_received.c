/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Inotify rename cookies in the reads that are easy to miss: a read that
// faults after copying an event, a readv whose second iovec faults after
// copying one, and reads through a descriptor received over SCM_RIGHTS. Linux
// copies each event whole before it moves to the next, so the events copied
// before a fault are in the buffer, with the cookie every other instance
// sees for that rename; that agreement is all the guest prints (Linux draws
// cookies from one host-wide counter, so their values differ from run to run;
// under Hermit, `--verify-strict` also compares them).

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <unistd.h>

// Every name here is two characters, which Linux pads to 16 bytes, so every
// event is 32 bytes.
#define EVENT 32

static char dir[] = "/tmp/inotify-faults-XXXXXX";

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

// The event at `p`: whether it is `mask` for `name`, and its cookie.
static int event_is(const char *p, uint32_t mask, const char *name, uint32_t *cookie) {
  const struct inotify_event *event = (const struct inotify_event *)p;
  *cookie = event->cookie;
  return (event->mask & mask) && event->len == 16 && !strcmp(event->name, name) && event->cookie;
}

static void report(const char *what, int ok, int *all) {
  printf("%s: %s\n", what, ok ? "yes" : "no");
  *all = *all && ok;
}

// A descriptor sent to this process over `pair` and received back.
static int pass(int pair[2], int fd) {
  char byte = 0;
  struct iovec data = {&byte, 1};
  union {
    struct cmsghdr header;
    char space[CMSG_SPACE(sizeof(int))];
  } control;
  memset(&control, 0, sizeof control);
  struct msghdr message = {.msg_iov = &data, .msg_iovlen = 1, .msg_control = &control,
                           .msg_controllen = sizeof control};
  struct cmsghdr *header = CMSG_FIRSTHDR(&message);
  header->cmsg_level = SOL_SOCKET;
  header->cmsg_type = SCM_RIGHTS;
  header->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(header), &fd, sizeof fd);
  if (sendmsg(pair[0], &message, 0) != 1) exit(4);
  memset(&control, 0, sizeof control);
  message.msg_controllen = sizeof control;
  if (recvmsg(pair[1], &message, MSG_CMSG_CLOEXEC) != 1) exit(4);
  header = CMSG_FIRSTHDR(&message);
  if (!header || header->cmsg_type != SCM_RIGHTS) exit(4);
  int received;
  memcpy(&received, CMSG_DATA(header), sizeof received);
  return received;
}

int main(void) {
  int all = 1;
  if (!mkdtemp(dir)) return 1;
  char first[64];
  snprintf(first, sizeof first, "%s/f0", dir);
  int fd = open(first, O_CREAT | O_WRONLY | O_CLOEXEC, 0644);
  if (fd < 0 || close(fd) != 0) return 1;
  int pair[2];
  if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0, pair) != 0) return 1;
  // A reads every rename whole; the others must agree with it.
  int a = watch(), f = watch(), v = watch();
  int r = pass(pair, watch());

  // Two pages, the second inaccessible: a buffer 40 bytes before it holds one
  // event and the first 8 bytes of the next.
  long page = sysconf(_SC_PAGESIZE);
  char *pages = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (pages == MAP_FAILED || mprotect(pages + page, page, PROT_NONE) != 0) return 1;
  char *edge = pages + page - 40;

  rename_in_dir("f0", "f1");
  rename_in_dir("f1", "f2");
  char abuf[4 * EVENT] __attribute__((aligned(8)));
  uint32_t c1 = 0, c1to = 0, c2 = 0, c2to = 0;
  int ok = read(a, abuf, sizeof abuf) == 4 * EVENT && event_is(abuf, IN_MOVED_FROM, "f0", &c1) &&
           event_is(abuf + EVENT, IN_MOVED_TO, "f1", &c1to) && c1 == c1to &&
           event_is(abuf + 2 * EVENT, IN_MOVED_FROM, "f1", &c2) &&
           event_is(abuf + 3 * EVENT, IN_MOVED_TO, "f2", &c2to) && c2 == c2to && c1 != c2;
  report("the reference instance reads both renames", ok, &all);

  // A read that faults copying its second event fails with EFAULT, and the
  // first event it copied keeps the rename's cookie.
  memset(edge, 0xa5, 40);
  ssize_t length = read(f, edge, 2 * EVENT);
  int fault = errno;
  uint32_t cookie = 0;
  report("a read that faults after one event fails with EFAULT", length == -1 && fault == EFAULT,
         &all);
  report("the event it copied has the rename's cookie",
         event_is(edge, IN_MOVED_FROM, "f0", &cookie) && cookie == c1, &all);

  // A readv whose second iovec faults after one event returns the first
  // iovec's event, and the event in the second iovec keeps its cookie.
  char vbuf[EVENT] __attribute__((aligned(8)));
  memset(edge, 0xa5, 40);
  struct iovec iov[2] = {{vbuf, EVENT}, {edge, 2 * EVENT}};
  length = readv(v, iov, 2);
  uint32_t vfrom = 0, vto = 0;
  report("a readv whose second iovec faults returns the first iovec",
         length == EVENT && event_is(vbuf, IN_MOVED_FROM, "f0", &vfrom) && vfrom == c1, &all);
  report("the event the faulting iovec holds has the rename's cookie",
         event_is(edge, IN_MOVED_TO, "f1", &vto) && vto == c1, &all);

  // The descriptor received over SCM_RIGHTS reads both renames.
  char rbuf[4 * EVENT] __attribute__((aligned(8)));
  uint32_t r1 = 0, r1to = 0, r2 = 0, r2to = 0;
  ok = read(r, rbuf, sizeof rbuf) == 4 * EVENT && event_is(rbuf, IN_MOVED_FROM, "f0", &r1) &&
       event_is(rbuf + EVENT, IN_MOVED_TO, "f1", &r1to) &&
       event_is(rbuf + 2 * EVENT, IN_MOVED_FROM, "f1", &r2) &&
       event_is(rbuf + 3 * EVENT, IN_MOVED_TO, "f2", &r2to) && r1 == c1 && r1to == c1 &&
       r2 == c2 && r2to == c2;
  report("a descriptor received over SCM_RIGHTS agrees", ok, &all);

  char last[64];
  snprintf(last, sizeof last, "%s/f2", dir);
  unlink(last);
  rmdir(dir);
  return all ? 0 : 9;
}
