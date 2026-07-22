/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Regression test for reconstructing recvmsg SCM_RIGHTS ancillary fds.
 *
 * A Unix domain socket carries a one-byte payload plus THREE file descriptors
 * in a single SCM_RIGHTS control message. After recvmsg, each received
 * descriptor is *read* (not just mmapped), which forces Detcore to have
 * registered it in its fd table -- otherwise the read fails with EBADF. The
 * test also checks the payload, the control-message header fields, and the
 * message flags, exercising the full descriptor-plus-payload reconstruction
 * path in both `hermit run` and record/replay.
 */

#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#define FD_COUNT 3

static void fail(const char *message) {
  perror(message);
  exit(EXIT_FAILURE);
}

int main(void) {
  int sockets[2];
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, sockets) != 0) {
    fail("socketpair");
  }

  /* Open the same stable ELF binary several times to obtain distinct fds. */
  int sources[FD_COUNT];
  for (int i = 0; i < FD_COUNT; i++) {
    sources[i] = open("/bin/sh", O_RDONLY);
    if (sources[i] < 0) {
      fail("open");
    }
  }

  char byte = 'z';
  struct iovec send_iov = {.iov_base = &byte, .iov_len = sizeof(byte)};
  char send_control[CMSG_SPACE(sizeof(sources))] = {0};
  struct msghdr send_message = {
      .msg_iov = &send_iov,
      .msg_iovlen = 1,
      .msg_control = send_control,
      .msg_controllen = sizeof(send_control),
  };
  struct cmsghdr *send_cmsg = CMSG_FIRSTHDR(&send_message);
  send_cmsg->cmsg_level = SOL_SOCKET;
  send_cmsg->cmsg_type = SCM_RIGHTS;
  send_cmsg->cmsg_len = CMSG_LEN(sizeof(sources));
  memcpy(CMSG_DATA(send_cmsg), sources, sizeof(sources));

  if (sendmsg(sockets[0], &send_message, 0) != sizeof(byte)) {
    fail("sendmsg");
  }

  char received_byte = 0;
  struct iovec receive_iov = {
      .iov_base = &received_byte,
      .iov_len = sizeof(received_byte),
  };
  char receive_control[CMSG_SPACE(sizeof(sources))] = {0};
  struct msghdr receive_message = {
      .msg_iov = &receive_iov,
      .msg_iovlen = 1,
      .msg_control = receive_control,
      .msg_controllen = sizeof(receive_control),
  };

  if (recvmsg(sockets[1], &receive_message, 0) != sizeof(received_byte)) {
    fail("recvmsg");
  }

  /* Validate payload and message flags. */
  if (received_byte != byte) {
    fputs("recvmsg payload mismatch\n", stderr);
    return EXIT_FAILURE;
  }
  if ((receive_message.msg_flags & MSG_CTRUNC) != 0) {
    fputs("recvmsg unexpectedly truncated the control message\n", stderr);
    return EXIT_FAILURE;
  }

  /* Validate the ancillary header. */
  struct cmsghdr *receive_cmsg = CMSG_FIRSTHDR(&receive_message);
  if (receive_cmsg == NULL || receive_cmsg->cmsg_level != SOL_SOCKET ||
      receive_cmsg->cmsg_type != SCM_RIGHTS ||
      receive_cmsg->cmsg_len != CMSG_LEN(sizeof(sources))) {
    fputs("invalid recvmsg control header\n", stderr);
    return EXIT_FAILURE;
  }

  int received[FD_COUNT];
  memcpy(received, CMSG_DATA(receive_cmsg), sizeof(received));

  /* Read each received descriptor and confirm it is the ELF we passed. This
   * step is what requires the received fds to be registered with Detcore. */
  for (int i = 0; i < FD_COUNT; i++) {
    char header[4] = {0};
    ssize_t got = read(received[i], header, sizeof(header));
    if (got < 0) {
      fail("read received fd");
    }
    if (got != (ssize_t)sizeof(header) || memcmp(header, "\x7f"
                                                         "ELF",
                                                 4) != 0) {
      fprintf(stderr, "received fd %d did not contain an ELF header\n", i);
      return EXIT_FAILURE;
    }
  }

  for (int i = 0; i < FD_COUNT; i++) {
    if (close(received[i]) != 0 || close(sources[i]) != 0) {
      fail("close");
    }
  }
  if (close(sockets[0]) != 0 || close(sockets[1]) != 0) {
    fail("close socket");
  }

  puts("recvmsg-scm-rights-multi-ok");
  return EXIT_SUCCESS;
}
