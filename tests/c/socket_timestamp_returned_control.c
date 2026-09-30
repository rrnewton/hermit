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
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

#define CHECK(condition)                                                       \
  do {                                                                         \
    if (!(condition)) {                                                        \
      fprintf(stderr, "%s:%d: %s errno=%d\n", __func__, __LINE__, #condition,    \
              errno);                                                          \
      exit(1);                                                                 \
    }                                                                          \
  } while (0)

#define TS_SPACE CMSG_SPACE(sizeof(struct timespec))
_Static_assert(sizeof(size_t) == 8, "this is the x86-64 receive ABI fixture");

static void put_stale_timestamp(unsigned char *control) {
  const struct cmsghdr header = {.cmsg_len = CMSG_LEN(sizeof(struct timespec)),
                                .cmsg_level = SOL_SOCKET,
                                .cmsg_type = SO_TIMESTAMPNS};
  const struct timespec value = {.tv_sec = 0x12345678, .tv_nsec = 123456789};
  memcpy(control, &header, sizeof(header));
  memcpy(control + CMSG_LEN(0), &value, sizeof(value));
}

static int compare_time(struct timespec a, struct timespec b) {
  if (a.tv_sec != b.tv_sec)
    return a.tv_sec < b.tv_sec ? -1 : 1;
  return (a.tv_nsec > b.tv_nsec) - (a.tv_nsec < b.tv_nsec);
}

static void check_timestamp(const unsigned char *control,
                            struct timespec before, struct timespec after) {
  struct cmsghdr header;
  struct timespec value;
  memcpy(&header, control, sizeof(header));
  CHECK(header.cmsg_level == SOL_SOCKET);
  CHECK(header.cmsg_type == SO_TIMESTAMPNS);
  CHECK(header.cmsg_len == CMSG_LEN(sizeof(value)));
  memcpy(&value, control + CMSG_LEN(0), sizeof(value));
  CHECK(value.tv_nsec >= 0 && value.tv_nsec < 1000000000);
  CHECK(compare_time(before, value) <= 0);
  CHECK(compare_time(value, after) <= 0);
}

static void send_right(int fd) {
  int donor[2];
  CHECK(pipe(donor) == 0);
  CHECK(write(donor[1], "R", 1) == 1);
  char payload = 'x';
  struct iovec iov = {.iov_base = &payload, .iov_len = 1};
  union {
    struct cmsghdr alignment;
    unsigned char bytes[CMSG_SPACE(sizeof(int))];
  } control = {0};
  struct msghdr message = {.msg_iov = &iov, .msg_iovlen = 1,
                          .msg_control = control.bytes,
                          .msg_controllen = sizeof(control.bytes)};
  struct cmsghdr *header = CMSG_FIRSTHDR(&message);
  header->cmsg_level = SOL_SOCKET;
  header->cmsg_type = SCM_RIGHTS;
  header->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(header), &donor[0], sizeof(int));
  CHECK(sendmsg(fd, &message, MSG_DONTWAIT) == 1);
  CHECK(close(donor[0]) == 0);
  CHECK(close(donor[1]) == 0);
}

static void receive_one(int fd, struct msghdr *message, int batch) {
  const int flags = MSG_DONTWAIT | MSG_CMSG_CLOEXEC;
  if (batch) {
    struct mmsghdr entry = {.msg_hdr = *message};
    CHECK(recvmmsg(fd, &entry, 1, flags, NULL) == 1);
    CHECK(entry.msg_len == 1);
    *message = entry.msg_hdr;
  } else {
    CHECK(recvmsg(fd, message, flags) == 1);
  }
}

/* Modes: no ancillary with a stale timestamp in RW/RO memory; a real timestamp
 * before a stale timestamp in an RO tail; a valid right before RO padding/tail.
 * These are exact byte/return-value checks, not a comparison of native time to
 * Hermit's logical epoch. */
static void ordinary_buffers(int batch, int mode) {
  int sockets[2];
  CHECK(socketpair(AF_UNIX, SOCK_DGRAM, 0, sockets) == 0);
  if (mode == 2) {
    int enabled = 1;
    CHECK(setsockopt(sockets[1], SOL_SOCKET, SO_TIMESTAMPNS, &enabled,
                     sizeof(enabled)) == 0);
  }
  long page = sysconf(_SC_PAGESIZE);
  CHECK(page >= 4096);
  unsigned char *area = mmap(NULL, (size_t)page * 2, PROT_READ | PROT_WRITE,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  CHECK(area != MAP_FAILED);
  memset(area, 0xa5, (size_t)page * 2);
  unsigned char *control;
  size_t capacity, written;
  if (mode < 2) {
    control = area + page;
    capacity = 2 * TS_SPACE;
    written = 0;
    put_stale_timestamp(control);
  } else if (mode == 2) {
    control = area + page - TS_SPACE;
    capacity = 2 * TS_SPACE;
    written = TS_SPACE;
    put_stale_timestamp(control + TS_SPACE);
  } else {
    control = area + page - CMSG_LEN(sizeof(int));
    capacity = CMSG_SPACE(sizeof(int)) + TS_SPACE;
    written = CMSG_LEN(sizeof(int));
    put_stale_timestamp(control + CMSG_SPACE(sizeof(int)));
  }
  unsigned char expected[2 * TS_SPACE];
  CHECK(capacity <= sizeof(expected));
  memcpy(expected, control, capacity);
  if (mode != 0)
    CHECK(mprotect(area + page, (size_t)page, PROT_READ) == 0);

  struct timespec before, after;
  CHECK(clock_gettime(CLOCK_REALTIME, &before) == 0);
  if (mode == 3)
    send_right(sockets[0]);
  else
    CHECK(send(sockets[0], "x", 1, MSG_DONTWAIT) == 1);
  char payload = 0;
  struct iovec iov = {.iov_base = &payload, .iov_len = 1};
  struct msghdr message = {.msg_iov = &iov, .msg_iovlen = 1,
                          .msg_control = control, .msg_controllen = capacity};
  receive_one(sockets[1], &message, batch);
  CHECK(clock_gettime(CLOCK_REALTIME, &after) == 0);
  CHECK(payload == 'x');
  CHECK((message.msg_flags & ~MSG_CMSG_CLOEXEC) == 0);
  CHECK(message.msg_control == control);
  CHECK(memcmp(control + written, expected + written, capacity - written) == 0);
  if (mode < 2) {
    CHECK(message.msg_controllen == 0);
  } else if (mode == 2) {
    CHECK(message.msg_controllen == TS_SPACE);
    check_timestamp(control, before, after);
  } else {
    struct cmsghdr header;
    int received;
    memcpy(&header, control, sizeof(header));
    CHECK(message.msg_controllen == CMSG_SPACE(sizeof(int)));
    CHECK(header.cmsg_level == SOL_SOCKET && header.cmsg_type == SCM_RIGHTS);
    CHECK(header.cmsg_len == CMSG_LEN(sizeof(int)));
    memcpy(&received, control + CMSG_LEN(0), sizeof(received));
    CHECK(fcntl(received, F_GETFD) == FD_CLOEXEC);
    char right_payload = 0;
    CHECK(read(received, &right_payload, 1) == 1 && right_payload == 'R');
    CHECK(read(received, &right_payload, 1) == 0);
    CHECK(close(received) == 0);
  }
  errno = 0;
  CHECK(recv(sockets[1], &payload, 1, MSG_DONTWAIT) == -1 && errno == EAGAIN);
  CHECK(close(sockets[0]) == 0);
  CHECK(close(sockets[1]) == 0);
  CHECK(munmap(area, (size_t)page * 2) == 0);
}

/* A later receive writes zero over the earlier returned length. Its actual
 * timestamp remains visible in separate memory. No signal, race, timeout,
 * scratch transaction, or kernel retry is needed to produce this overlap.
 * Final-length-only parsing skips entry0; main's capacity scan normalizes it.
 * This is deliberately part of the default test, not an ignored/exempt case. */
static void overwritten_batch_length(void) {
  int sockets[2], enabled = 1;
  CHECK(socketpair(AF_UNIX, SOCK_DGRAM, 0, sockets) == 0);
  CHECK(setsockopt(sockets[1], SOL_SOCKET, SO_TIMESTAMPNS, &enabled,
                   sizeof(enabled)) == 0);
  struct mmsghdr messages[2] = {0};
  unsigned char control[2][TS_SPACE];
  memset(control, 0xa5, sizeof(control));
  char payload = 0;
  struct iovec iov[2] = {
      {.iov_base = &payload, .iov_len = 1},
      {.iov_base = &messages[0].msg_hdr.msg_controllen, .iov_len = sizeof(size_t)}};
  for (int i = 0; i < 2; ++i) {
    messages[i].msg_hdr.msg_iov = &iov[i];
    messages[i].msg_hdr.msg_iovlen = 1;
    messages[i].msg_hdr.msg_control = control[i];
    messages[i].msg_hdr.msg_controllen = sizeof(control[i]);
  }
  struct timespec before, after;
  CHECK(clock_gettime(CLOCK_REALTIME, &before) == 0);
  const size_t zero = 0;
  CHECK(send(sockets[0], "a", 1, MSG_DONTWAIT) == 1);
  CHECK(send(sockets[0], &zero, sizeof(zero), MSG_DONTWAIT) == (ssize_t)sizeof(zero));
  CHECK(recvmmsg(sockets[1], messages, 2, MSG_DONTWAIT, NULL) == 2);
  CHECK(clock_gettime(CLOCK_REALTIME, &after) == 0);
  CHECK(payload == 'a');
  CHECK(messages[0].msg_len == 1 && messages[1].msg_len == sizeof(zero));
  CHECK(messages[0].msg_hdr.msg_controllen == 0);
  CHECK(messages[1].msg_hdr.msg_controllen == TS_SPACE);
  CHECK(messages[0].msg_hdr.msg_flags == 0 && messages[1].msg_hdr.msg_flags == 0);
  check_timestamp(control[0], before, after);
  check_timestamp(control[1], before, after);
  CHECK(close(sockets[0]) == 0);
  CHECK(close(sockets[1]) == 0);
}

int main(int argc, char **argv) {
  CHECK(argc == 1 || (argc == 2 &&
        (strcmp(argv[1], "--ordinary") == 0 || strcmp(argv[1], "--length-alias") == 0)));
  if (argc == 1 || strcmp(argv[1], "--length-alias") == 0)
    overwritten_batch_length();
  if (argc == 1 || strcmp(argv[1], "--ordinary") == 0)
    for (int batch = 0; batch < 2; ++batch)
      for (int mode = 0; mode < 4; ++mode)
        ordinary_buffers(batch, mode);
  puts("returned-control=ok");
  return 0;
}
