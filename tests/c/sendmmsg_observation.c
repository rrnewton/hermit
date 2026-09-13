/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <unistd.h>

static int pair(int sockets[2]) {
  return socketpair(AF_UNIX, SOCK_DGRAM | SOCK_NONBLOCK, 0, sockets);
}

static int received(int fd, const void *expected, size_t length) {
  unsigned char bytes[64];
  ssize_t count = recv(fd, bytes, sizeof(bytes), MSG_DONTWAIT);
  return count == (ssize_t)length && memcmp(bytes, expected, length) == 0;
}

static void evidence(const char *name, const void *address, const void *bytes, size_t length) {
  printf("sendmmsg-evidence:%s:%lx:%zu:", name, (unsigned long)address, length);
  for (size_t i = 0; i < length; ++i) printf("%02x", ((const unsigned char *)bytes)[i]);
  puts("");
}

static int own_iovec(void) {
  int sockets[2];
  if (pair(sockets)) return 1;
  union {
    unsigned char bytes[sizeof(struct mmsghdr) + sizeof(struct iovec)];
    struct mmsghdr alignment;
  } area = {0};
  struct mmsghdr *message = (struct mmsghdr *)area.bytes;
  struct iovec *iov = (struct iovec *)(area.bytes + offsetof(struct mmsghdr, msg_len));
  char payload[16] = "alias-payload";
  message->msg_hdr.msg_iov = iov;
  message->msg_hdr.msg_iovlen = 1;
  const struct iovec initial_iov = {.iov_base = payload, .iov_len = sizeof(payload)};
  memcpy(iov, &initial_iov, sizeof(initial_iov));
  long count = syscall(SYS_sendmmsg, sockets[0], message, 1, 0);
  int ok = count == 1 && message->msg_len == 16 && received(sockets[1], payload, 16);
  evidence("own-iovec", payload, payload, 16);
  close(sockets[0]); close(sockets[1]);
  return ok ? 0 : 2;
}

static int later_iovec(void) {
  int sockets[2];
  if (pair(sockets)) return 1;
  struct mmsghdr messages[2] = {0};
  char first[12] = "12345678901";
  char second[16] = "abcdefghijklmnop";
  struct iovec first_iov = {.iov_base = first, .iov_len = sizeof(first)};
  struct iovec *second_iov = (struct iovec *)((unsigned char *)messages +
      offsetof(struct mmsghdr, msg_len) - offsetof(struct iovec, iov_len));
  messages[0].msg_hdr.msg_iov = &first_iov;
  messages[0].msg_hdr.msg_iovlen = 1;
  const struct iovec initial_second_iov = {.iov_base = second, .iov_len = 4};
  memcpy(second_iov, &initial_second_iov, sizeof(initial_second_iov));
  messages[1].msg_hdr.msg_iov = second_iov;
  messages[1].msg_hdr.msg_iovlen = 1;
  long count = syscall(SYS_sendmmsg, sockets[0], messages, 2, 0);
  int ok = count == 2 && messages[0].msg_len == 12 && messages[1].msg_len == 12 &&
      received(sockets[1], first, 12) && received(sockets[1], second, 12);
  evidence("earlier-message", first, first, 12);
  evidence("later-iovec", second, second, 12);
  close(sockets[0]); close(sockets[1]);
  return ok ? 0 : 3;
}

static int output_payload(void) {
  int sockets[2];
  if (pair(sockets)) return 1;
  struct mmsghdr messages[2] = {0};
  messages[0].msg_len = 0x41414141;
  struct iovec iov = {.iov_base = &messages[0].msg_len, .iov_len = 4};
  for (int i = 0; i < 2; ++i) {
    messages[i].msg_hdr.msg_iov = &iov;
    messages[i].msg_hdr.msg_iovlen = 1;
  }
  const uint32_t original = 0x41414141;
  const uint32_t completed = 4;
  long count = syscall(SYS_sendmmsg, sockets[0], messages, 2, 0);
  int ok = count == 2 && messages[0].msg_len == 4 && messages[1].msg_len == 4 &&
      received(sockets[1], &original, 4) && received(sockets[1], &completed, 4);
  evidence("own-output-payload", iov.iov_base, &original, 4);
  evidence("earlier-output-payload", iov.iov_base, &completed, 4);
  close(sockets[0]); close(sockets[1]);
  return ok ? 0 : 4;
}

static int unreadable_tail(void) {
  int sockets[2];
  if (pair(sockets)) return 1;
  long page = sysconf(_SC_PAGESIZE);
  if (page <= 0) return 5;
  unsigned char *area = mmap(NULL, (size_t)page * 2, PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (area == MAP_FAILED || mprotect(area + page, (size_t)page, PROT_NONE)) return 5;
  struct mmsghdr *message = (struct mmsghdr *)(area + page - sizeof(struct mmsghdr));
  char payload[5] = "hello";
  struct iovec iov = {.iov_base = payload, .iov_len = sizeof(payload)};
  message->msg_hdr.msg_iov = &iov;
  message->msg_hdr.msg_iovlen = 1;
  long count = syscall(SYS_sendmmsg, sockets[0], message, 2, 0);
  int ok = count == 1 && message->msg_len == 5 && received(sockets[1], payload, 5);
  evidence("readable-prefix", payload, payload, 5);
  errno = 0;
  long invalid = syscall(SYS_sendmmsg, -1, area + page, 2, 0);
  ok = ok && invalid == -1 && errno == EBADF;
  munmap(area, (size_t)page * 2);
  close(sockets[0]); close(sockets[1]);
  return ok ? 0 : 5;
}

int main(void) {
  int result;
  if ((result = own_iovec()) || (result = later_iovec()) ||
      (result = output_payload()) || (result = unreadable_tail())) {
    fprintf(stderr, "sendmmsg observation case failed: %d errno=%d\n", result, errno);
    return result;
  }
  puts("sendmmsg-output-aliases-ok");
  return 0;
}
