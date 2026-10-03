/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Socket setup and batched message calls that ptrace replay used to run live
 * (https://github.com/rrnewton/hermit/issues/3550): bind, listen, accept4 with
 * a truncated peer address, shutdown, socketpair, sendmmsg and recvmmsg with
 * SCM_RIGHTS. It also sets options whose result depends on the bound state
 * (IPV6_V6ONLY after bind, AF_ALG's ALG_SET_KEY), which replay runs live, and
 * IPV6_ADDRFORM, which depends on a connection replay does not make. Two
 * recvmmsg edge cases return a count or an error after partial side effects:
 * an unmapped header after the received ones, and a read-only timeout.
 */

#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/if_alg.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

#ifndef SOL_ALG
#define SOL_ALG 279
#endif

#define CHECK(expr)                                                     \
  do {                                                                  \
    if (!(expr)) {                                                      \
      fprintf(stderr, "%s:%d: %s: %s\n", __FILE__, __LINE__, #expr,     \
              strerror(errno));                                         \
      return 1;                                                         \
    }                                                                   \
  } while (0)

static int batched_messages(void) {
  int sv[2];
  CHECK(socketpair(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0, sv) == 0);
  printf("socketpair %d %d\n", sv[0], sv[1]);

  int passed = open("/dev/null", O_RDONLY | O_CLOEXEC);
  CHECK(passed >= 0);
  const char* payloads[3] = {"one", "two!", "three"};
  struct iovec send_iov[3];
  struct mmsghdr send_msgs[3];
  char control[CMSG_SPACE(sizeof(int))];
  memset(send_msgs, 0, sizeof send_msgs);
  memset(control, 0, sizeof control);
  for (int i = 0; i < 3; i++) {
    send_iov[i].iov_base = (void*)payloads[i];
    send_iov[i].iov_len = strlen(payloads[i]);
    send_msgs[i].msg_hdr.msg_iov = &send_iov[i];
    send_msgs[i].msg_hdr.msg_iovlen = 1;
  }
  send_msgs[1].msg_hdr.msg_control = control;
  send_msgs[1].msg_hdr.msg_controllen = sizeof control;
  struct cmsghdr* cmsg = CMSG_FIRSTHDR(&send_msgs[1].msg_hdr);
  cmsg->cmsg_level = SOL_SOCKET;
  cmsg->cmsg_type = SCM_RIGHTS;
  cmsg->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(cmsg), &passed, sizeof(int));
  int sent = sendmmsg(sv[0], send_msgs, 3, 0);
  CHECK(sent == 3);
  printf("sendmmsg %d lens %u %u %u\n", sent, send_msgs[0].msg_len,
         send_msgs[1].msg_len, send_msgs[2].msg_len);

  /* Ask for four messages: only three are queued, so the count is partial. */
  char buffers[4][16];
  char received_control[4][CMSG_SPACE(sizeof(int))];
  struct iovec recv_iov[4];
  struct mmsghdr recv_msgs[4];
  memset(buffers, 0, sizeof buffers);
  memset(received_control, 0, sizeof received_control);
  memset(recv_msgs, 0, sizeof recv_msgs);
  for (int i = 0; i < 4; i++) {
    recv_iov[i].iov_base = buffers[i];
    recv_iov[i].iov_len = sizeof buffers[i];
    recv_msgs[i].msg_hdr.msg_iov = &recv_iov[i];
    recv_msgs[i].msg_hdr.msg_iovlen = 1;
    recv_msgs[i].msg_hdr.msg_control = received_control[i];
    recv_msgs[i].msg_hdr.msg_controllen = sizeof received_control[i];
  }
  struct timespec timeout = {0, 1000000};
  int received =
      recvmmsg(sv[1], recv_msgs, 4, MSG_CMSG_CLOEXEC | MSG_DONTWAIT, &timeout);
  CHECK(received == 3);
  printf("recvmmsg %d\n", received);
  for (int i = 0; i < received; i++) {
    int fd = -1;
    struct cmsghdr* got = CMSG_FIRSTHDR(&recv_msgs[i].msg_hdr);
    if (got != NULL && got->cmsg_type == SCM_RIGHTS) {
      memcpy(&fd, CMSG_DATA(got), sizeof fd);
    }
    printf("  message %d len %u data %.*s fd %d cloexec %d\n", i,
           recv_msgs[i].msg_len, (int)recv_msgs[i].msg_len, buffers[i], fd,
           fd >= 0 ? (fcntl(fd, F_GETFD) & FD_CLOEXEC) : -1);
  }

  CHECK(shutdown(sv[0], SHUT_RDWR) == 0);
  printf("shutdown socketpair ok\n");
  return 0;
}

/* Queue `count` datagrams "m0", "m1", ... on a fresh socketpair. */
static int queued_pair(int sv[2], int count) {
  CHECK(socketpair(AF_UNIX, SOCK_DGRAM, 0, sv) == 0);
  for (int i = 0; i < count; i++) {
    char payload[3] = {'m', (char)('0' + i), 0};
    CHECK(send(sv[0], payload, 2, 0) == 2);
  }
  return 0;
}

static void prepare_receive(struct mmsghdr* message, struct iovec* iov,
                            char* buffer, size_t length) {
  memset(message, 0, sizeof *message);
  iov->iov_base = buffer;
  iov->iov_len = length;
  message->msg_hdr.msg_iov = iov;
  message->msg_hdr.msg_iovlen = 1;
  message->msg_len = 77;
}

static int partial_side_effects(void) {
  long page = sysconf(_SC_PAGESIZE);
  char* pages = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  CHECK(pages != MAP_FAILED);
  CHECK(mprotect(pages + page, page, PROT_NONE) == 0);

  /* Two headers end the readable page; the third is unmapped. */
  int sv[2];
  CHECK(queued_pair(sv, 2) == 0);
  struct mmsghdr* tail = (struct mmsghdr*)(pages + page) - 2;
  struct iovec iov[2];
  char buffers[2][8] = {{0}};
  for (int i = 0; i < 2; i++) {
    prepare_receive(&tail[i], &iov[i], buffers[i], sizeof buffers[i]);
  }
  int received = recvmmsg(sv[1], tail, 4, MSG_DONTWAIT, NULL);
  printf("recvmmsg unmapped tail %d errno %d lens %u %u data %.2s %.2s\n",
         received, received < 0 ? errno : 0, tail[0].msg_len, tail[1].msg_len,
         buffers[0], buffers[1]);
  CHECK(received == 2 && tail[0].msg_len == 2 && tail[1].msg_len == 2);
  /* Linux reports the fault on the next receive. */
  char next[8];
  ssize_t result = recv(sv[1], next, sizeof next, MSG_DONTWAIT);
  printf("next recv %zd errno %d\n", result, result < 0 ? errno : 0);
  CHECK(result == -1 && errno == EFAULT);
  close(sv[0]);
  close(sv[1]);

  /* Both messages are received before the timeout cannot be written back. */
  CHECK(queued_pair(sv, 2) == 0);
  struct timespec* timeout = (struct timespec*)pages;
  timeout->tv_sec = 5;
  timeout->tv_nsec = 0;
  CHECK(mprotect(pages, page, PROT_READ) == 0);
  struct mmsghdr messages[3];
  struct iovec message_iov[3];
  char message_buffers[3][8] = {{0}};
  for (int i = 0; i < 3; i++) {
    prepare_receive(&messages[i], &message_iov[i], message_buffers[i],
                    sizeof message_buffers[i]);
  }
  received = recvmmsg(sv[1], messages, 3, MSG_DONTWAIT, timeout);
  printf(
      "recvmmsg read-only timeout %d errno %d lens %u %u %u data %.2s %.2s\n",
      received, received < 0 ? errno : 0, messages[0].msg_len,
      messages[1].msg_len, messages[2].msg_len, message_buffers[0],
      message_buffers[1]);
  CHECK(received == -1 && errno == EFAULT);
  CHECK(messages[0].msg_len == 2 && messages[1].msg_len == 2 &&
        messages[2].msg_len == 77);
  close(sv[0]);
  close(sv[1]);
  CHECK(munmap(pages, 2 * page) == 0);
  return 0;
}

static int tcp_server(void) {
  int listener = socket(AF_INET, SOCK_STREAM, 0);
  CHECK(listener >= 0);
  struct sockaddr_in address = {
      .sin_family = AF_INET,
      .sin_addr.s_addr = htonl(INADDR_LOOPBACK),
  };
  CHECK(bind(listener, (struct sockaddr*)&address, sizeof address) == 0);
  socklen_t length = sizeof address;
  CHECK(getsockname(listener, (struct sockaddr*)&address, &length) == 0);
  CHECK(listen(listener, 4) == 0);

  int client = socket(AF_INET, SOCK_STREAM, 0);
  CHECK(client >= 0);
  CHECK(connect(client, (struct sockaddr*)&address, sizeof address) == 0);

  /* Four bytes hold the family and port; the kernel reports the full length. */
  struct sockaddr_in peer;
  memset(&peer, 0, sizeof peer);
  socklen_t peer_length = 4;
  int accepted =
      accept4(listener, (struct sockaddr*)&peer, &peer_length, SOCK_CLOEXEC);
  CHECK(accepted >= 0);
  printf("accept4 fd %d length %u family %d cloexec %d\n", accepted,
         peer_length, peer.sin_family,
         fcntl(accepted, F_GETFD) & FD_CLOEXEC);

  int domain = -1;
  socklen_t domain_length = sizeof domain;
  CHECK(getsockopt(accepted, SOL_SOCKET, SO_DOMAIN, &domain,
                   &domain_length) == 0);
  int one = 1;
  CHECK(setsockopt(accepted, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one) ==
        0);
  printf("accepted domain %d nodelay set\n", domain);

  CHECK(write(client, "ping", 4) == 4);
  char reply[8] = {0};
  CHECK(read(accepted, reply, sizeof reply) == 4);
  printf("read %.4s\n", reply);
  CHECK(shutdown(accepted, SHUT_WR) == 0);
  printf("shutdown accepted ok\n");
  return 0;
}

/* IPV6_ADDRFORM converts a connected socket with a v4-mapped peer to AF_INET. */
static int connected_state_options(void) {
  int listener = socket(AF_INET6, SOCK_STREAM, 0);
  if (listener < 0) {
    printf("ipv6 addrform unavailable errno %d\n", errno);
    return 0;
  }
  struct sockaddr_in6 address = {.sin6_family = AF_INET6};
  CHECK(inet_pton(AF_INET6, "::ffff:127.0.0.1", &address.sin6_addr) == 1);
  if (bind(listener, (struct sockaddr*)&address, sizeof address) != 0) {
    printf("ipv6 addrform bind unavailable errno %d\n", errno);
    close(listener);
    return 0;
  }
  socklen_t length = sizeof address;
  CHECK(getsockname(listener, (struct sockaddr*)&address, &length) == 0);
  CHECK(listen(listener, 1) == 0);

  int client = socket(AF_INET6, SOCK_STREAM, 0);
  CHECK(client >= 0);
  CHECK(connect(client, (struct sockaddr*)&address, sizeof address) == 0);
  int accepted = accept(listener, NULL, NULL);
  CHECK(accepted >= 0);

  int inet = AF_INET;
  int on_accepted =
      setsockopt(accepted, IPPROTO_IPV6, IPV6_ADDRFORM, &inet, sizeof inet);
  int accepted_errno = on_accepted < 0 ? errno : 0;
  if (accepted_errno == ENOPROTOOPT) {
    printf("ipv6 addrform unsupported\n");
    close(accepted);
    close(client);
    close(listener);
    return 0;
  }
  int on_client =
      setsockopt(client, IPPROTO_IPV6, IPV6_ADDRFORM, &inet, sizeof inet);
  int client_errno = on_client < 0 ? errno : 0;
  int on_listener =
      setsockopt(listener, IPPROTO_IPV6, IPV6_ADDRFORM, &inet, sizeof inet);
  int listener_errno = on_listener < 0 ? errno : 0;
  printf("ipv6_addrform accepted %d errno %d client %d errno %d listener %d "
         "errno %d\n",
         on_accepted, accepted_errno, on_client, client_errno, on_listener,
         listener_errno);
  /* ADDRFORM needs a connected socket mapped to IPv4. */
  CHECK(on_accepted == 0 && on_client == 0);
  CHECK(on_listener == -1 && listener_errno == ENOTCONN);
  /* Linux refuses listen on a connected socket. */
  int relisten = listen(accepted, 1);
  printf("listen on accepted %d errno %d\n", relisten,
         relisten < 0 ? errno : 0);
  CHECK(relisten == -1 && errno == EINVAL);
  close(accepted);
  close(client);
  close(listener);
  return 0;
}

static int bound_state_options(void) {
  /* Linux refuses IPV6_V6ONLY once the socket is bound. */
  int v6 = socket(AF_INET6, SOCK_STREAM, 0);
  if (v6 < 0) {
    printf("ipv6 unavailable errno %d\n", errno);
  } else {
    struct sockaddr_in6 address = {
        .sin6_family = AF_INET6,
        .sin6_addr = IN6ADDR_LOOPBACK_INIT,
    };
    CHECK(bind(v6, (struct sockaddr*)&address, sizeof address) == 0);
    int one = 1;
    int result = setsockopt(v6, IPPROTO_IPV6, IPV6_V6ONLY, &one, sizeof one);
    printf("ipv6_v6only after bind %d errno %d\n", result,
           result < 0 ? errno : 0);
  }

  /* AF_ALG accepts a key only after bind selects the algorithm. */
  int alg = socket(AF_ALG, SOCK_SEQPACKET, 0);
  if (alg < 0) {
    printf("af_alg unavailable errno %d\n", errno);
    return 0;
  }
  struct sockaddr_alg algorithm = {
      .salg_family = AF_ALG,
      .salg_type = "hash",
      .salg_name = "hmac(sha256)",
  };
  if (bind(alg, (struct sockaddr*)&algorithm, sizeof algorithm) != 0) {
    printf("hmac(sha256) unavailable errno %d\n", errno);
    return 0;
  }
  CHECK(setsockopt(alg, SOL_ALG, ALG_SET_KEY, "key", 3) == 0);
  int operation = accept(alg, NULL, NULL);
  CHECK(operation >= 0);
  CHECK(write(operation, "hello", 5) == 5);
  unsigned char digest[32];
  CHECK(read(operation, digest, sizeof digest) == (ssize_t)sizeof digest);
  printf("hmac ");
  for (size_t i = 0; i < sizeof digest; i++) {
    printf("%02x", digest[i]);
  }
  printf("\n");
  return 0;
}

int main(void) {
  if (batched_messages() != 0 || partial_side_effects() != 0 ||
      tcp_server() != 0 || connected_state_options() != 0 ||
      bound_state_options() != 0) {
    return 1;
  }
  return 0;
}
