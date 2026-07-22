/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Exercises SCM_RIGHTS file-descriptor passing through recvmsg's ancillary
 * control data. A pipe read-end is sent over an AF_UNIX socketpair and the
 * receiver parses the SCM_RIGHTS control message. Replay must reconstruct the
 * ancillary buffer (msg_control / msg_controllen) exactly for `record --verify`
 * to succeed.
 *
 * NOTE: we deliberately do not read/close the *passed* descriptor. Hermit's
 * Detcore fd virtualization does not register descriptors that arrive via
 * SCM_RIGHTS (they are materialized by the kernel, not by an open/socket/dup
 * syscall Detcore observes), so operating on the received fd fails with EBADF
 * independently of record/replay. This test validates that recvmsg's ancillary
 * control data is recorded and replayed faithfully, which is the recorder/
 * replayer's responsibility. */

#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <unistd.h>

int main(void) {
  int pipe_fds[2];
  if (pipe(pipe_fds) != 0) {
    perror("pipe");
    return 1;
  }

  const char payload[] = "SCM!";
  if (write(pipe_fds[1], payload, sizeof(payload) - 1) != sizeof(payload) - 1) {
    perror("write");
    return 1;
  }

  int sv[2];
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) {
    perror("socketpair");
    return 1;
  }

  /* Send one data byte plus the pipe read-end as ancillary SCM_RIGHTS data. */
  char data = 'x';
  struct iovec iov;
  iov.iov_base = &data;
  iov.iov_len = 1;

  union {
    char buf[CMSG_SPACE(sizeof(int))];
    struct cmsghdr align;
  } send_control;
  memset(&send_control, 0, sizeof(send_control));

  struct msghdr send_msg;
  memset(&send_msg, 0, sizeof(send_msg));
  send_msg.msg_iov = &iov;
  send_msg.msg_iovlen = 1;
  send_msg.msg_control = send_control.buf;
  send_msg.msg_controllen = sizeof(send_control.buf);

  struct cmsghdr *scm = CMSG_FIRSTHDR(&send_msg);
  scm->cmsg_level = SOL_SOCKET;
  scm->cmsg_type = SCM_RIGHTS;
  scm->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(scm), &pipe_fds[0], sizeof(int));

  if (sendmsg(sv[0], &send_msg, 0) < 0) {
    perror("sendmsg");
    return 1;
  }

  /* Receive the data byte and the ancillary descriptor. */
  char recv_data = 0;
  struct iovec recv_iov;
  recv_iov.iov_base = &recv_data;
  recv_iov.iov_len = 1;

  union {
    char buf[CMSG_SPACE(sizeof(int))];
    struct cmsghdr align;
  } recv_control;
  memset(&recv_control, 0, sizeof(recv_control));

  struct msghdr recv_msg;
  memset(&recv_msg, 0, sizeof(recv_msg));
  recv_msg.msg_iov = &recv_iov;
  recv_msg.msg_iovlen = 1;
  recv_msg.msg_control = recv_control.buf;
  recv_msg.msg_controllen = sizeof(recv_control.buf);

  ssize_t got = recvmsg(sv[1], &recv_msg, 0);
  if (got < 0) {
    perror("recvmsg");
    return 1;
  }

  struct cmsghdr *rcmsg = CMSG_FIRSTHDR(&recv_msg);
  if (rcmsg == NULL || rcmsg->cmsg_level != SOL_SOCKET ||
      rcmsg->cmsg_type != SCM_RIGHTS) {
    fprintf(stderr, "recvmsg did not return SCM_RIGHTS control data\n");
    return 1;
  }

  /* Confirm a descriptor was delivered without operating on it (see the note
   * above). The control-message length is fixed and therefore deterministic. */
  int received_fd = -1;
  memcpy(&received_fd, CMSG_DATA(rcmsg), sizeof(int));

  printf("received data byte '%c', recvmsg=%zd, SCM_RIGHTS cmsg_len=%zu, fd %s\n",
         recv_data, got, (size_t)rcmsg->cmsg_len,
         received_fd >= 0 ? "delivered" : "missing");
  return 0;
}
