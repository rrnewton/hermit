/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Exercises sendmsg/recvmsg with scatter-gather buffers over an AF_UNIX
 * socketpair. The message is sent from two segments and received into two
 * differently-sized buffers, so replay must scatter the recorded payload back
 * across the caller's iovecs exactly for `record --verify` to succeed. */

#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <unistd.h>

int main(void) {
  int sv[2];
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) {
    perror("socketpair");
    return 1;
  }

  char part0[] = "vectored ";
  char part1[] = "message";
  struct iovec send_iov[2];
  send_iov[0].iov_base = part0;
  send_iov[0].iov_len = strlen(part0);
  send_iov[1].iov_base = part1;
  send_iov[1].iov_len = strlen(part1);

  struct msghdr send_msg;
  memset(&send_msg, 0, sizeof(send_msg));
  send_msg.msg_iov = send_iov;
  send_msg.msg_iovlen = 2;

  ssize_t sent = sendmsg(sv[0], &send_msg, 0);
  if (sent < 0) {
    perror("sendmsg");
    return 1;
  }

  /* Deliberately split the receive across two small buffers so the scatter
   * boundary falls in the middle of the message. */
  char buf0[8];
  char buf1[16];
  memset(buf0, 0, sizeof(buf0));
  memset(buf1, 0, sizeof(buf1));
  struct iovec recv_iov[2];
  recv_iov[0].iov_base = buf0;
  recv_iov[0].iov_len = sizeof(buf0);
  recv_iov[1].iov_base = buf1;
  recv_iov[1].iov_len = sizeof(buf1);

  struct msghdr recv_msg;
  memset(&recv_msg, 0, sizeof(recv_msg));
  recv_msg.msg_iov = recv_iov;
  recv_msg.msg_iovlen = 2;

  ssize_t got = recvmsg(sv[1], &recv_msg, 0);
  if (got < 0) {
    perror("recvmsg");
    return 1;
  }

  /* Reassemble the received message from the scatter buffers. */
  char out[32];
  memset(out, 0, sizeof(out));
  size_t off = 0;
  size_t remaining = (size_t)got;
  for (int i = 0; i < 2 && remaining > 0; i++) {
    size_t take =
        remaining < recv_iov[i].iov_len ? remaining : recv_iov[i].iov_len;
    memcpy(out + off, recv_iov[i].iov_base, take);
    off += take;
    remaining -= take;
  }

  printf("recvmsg got %zd bytes: %s\n", got, out);
  return 0;
}
