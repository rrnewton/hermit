#define _GNU_SOURCE

/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Network record/replay bracket. The controller is a loopback TCP server
 * with a fixed protocol; the client connects to it and two threads, one on
 * the socket and one on a dup of it, race to poll and receive the two
 * three-byte inputs. Which thread wins depends on the schedule; the bytes
 * the client sends and receives do not.
 *
 * The other client modes each do one thing network record/replay must
 * refuse: end before sending the whole request, send with sendmsg or
 * sendfile, connect to the unspecified address, send UDP, or listen. The
 * guard modes each try a way around the channel before any connection: a
 * 24-byte IPv6 connect, a netlink socket, an abstract AF_UNIX connect, a
 * socket receive timeout, an IPv4 socket received through SCM_RIGHTS, epoll
 * registration, or signal-driven I/O.
 */

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/sendfile.h>
#include <sys/epoll.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <sys/uio.h>
#include <sys/time.h>
#include <sys/types.h>
#include <unistd.h>

#define PAYLOAD_SIZE 3
#define FIXTURE_DEADLINE_SECONDS 8
#define CONTROLLER_ACCEPT_DEADLINE_SECONDS 30

static const char REQUEST[] = "request\n";
static const char PROGRESS[] = "next\n";
static const char COMPLETION[] = "done\n";
static const char FIRST_INPUT[] = "abc";
static const char SECOND_INPUT[] = "def";
static const char OUTBOUND_HEX[] = "726571756573740a6e6578740a646f6e650a";

static void fail(const char *operation) {
  perror(operation);
  exit(1);
}

static void fail_message(const char *message) {
  fprintf(stderr, "%s\n", message);
  exit(1);
}

static void set_deadline(void) {
  signal(SIGPIPE, SIG_IGN);
  alarm(FIXTURE_DEADLINE_SECONDS);
}

static void set_socket_timeouts(int fd) {
  const struct timeval timeout = {.tv_sec = 5, .tv_usec = 0};
  if (setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) != 0 ||
      setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout)) != 0) {
    fail("setsockopt timeout");
  }
}

static void send_all(int fd, const void *raw, size_t length) {
  const char *bytes = raw;
  while (length != 0) {
    ssize_t sent = send(fd, bytes, length, MSG_NOSIGNAL);
    if (sent < 0 && errno == EINTR)
      continue;
    if (sent <= 0)
      fail("send");
    bytes += sent;
    length -= (size_t)sent;
  }
}

static void receive_exact(int fd, void *raw, size_t length) {
  char *bytes = raw;
  while (length != 0) {
    ssize_t received = recv(fd, bytes, length, 0);
    if (received < 0 && errno == EINTR)
      continue;
    if (received < 0)
      fail("recv");
    if (received == 0)
      fail_message("peer closed before the fixed protocol message completed");
    bytes += received;
    length -= (size_t)received;
  }
}

static uint64_t fnv1a64_update(uint64_t digest, const void *raw, size_t length) {
  const unsigned char *bytes = raw;
  for (size_t index = 0; index < length; ++index) {
    digest ^= bytes[index];
    digest *= UINT64_C(1099511628211);
  }
  return digest;
}

static uint64_t expected_outbound_digest(void) {
  uint64_t digest = UINT64_C(14695981039346656037);
  digest = fnv1a64_update(digest, REQUEST, sizeof(REQUEST) - 1);
  digest = fnv1a64_update(digest, PROGRESS, sizeof(PROGRESS) - 1);
  return fnv1a64_update(digest, COMPLETION, sizeof(COMPLETION) - 1);
}

static void publish_text(const char *path, const char *text) {
  char temporary[4096];
  int count = snprintf(temporary, sizeof(temporary), "%s.tmp.%ld", path,
                       (long)getpid());
  if (count < 0 || (size_t)count >= sizeof(temporary))
    fail_message("publication path is too long");

  int fd = open(temporary, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0600);
  if (fd < 0)
    fail("open publication");
  size_t length = strlen(text);
  const char *cursor = text;
  while (length != 0) {
    ssize_t written = write(fd, cursor, length);
    if (written < 0 && errno == EINTR)
      continue;
    if (written <= 0)
      fail("write publication");
    cursor += written;
    length -= (size_t)written;
  }
  if (close(fd) != 0)
    fail("close publication");
  if (rename(temporary, path) != 0)
    fail("rename publication");
}

static int run_controller(const char *port_path, const char *report_path,
                          const char *contact_path) {
  signal(SIGPIPE, SIG_IGN);
  alarm(CONTROLLER_ACCEPT_DEADLINE_SECONDS);
  int listener = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
  if (listener < 0)
    fail("socket controller");

  struct sockaddr_in address = {
      .sin_family = AF_INET,
      .sin_port = htons(0),
      .sin_addr.s_addr = htonl(INADDR_LOOPBACK),
  };
  if (bind(listener, (struct sockaddr *)&address, sizeof(address)) != 0)
    fail("bind controller");
  socklen_t address_length = sizeof(address);
  if (getsockname(listener, (struct sockaddr *)&address, &address_length) != 0)
    fail("getsockname controller");
  if (listen(listener, 1) != 0)
    fail("listen controller");

  char port[32];
  int port_length = snprintf(port, sizeof(port), "%u\n", ntohs(address.sin_port));
  if (port_length <= 0 || (size_t)port_length >= sizeof(port))
    fail_message("controller port did not fit its publication buffer");
  publish_text(port_path, port);

  int client = accept4(listener, NULL, NULL, SOCK_CLOEXEC);
  if (client < 0)
    fail("accept controller");
  // Hermit startup happens before the guest can connect. Once accepted, the
  // exact stream protocol retains its original, shorter wall bound.
  alarm(FIXTURE_DEADLINE_SECONDS);
  publish_text(contact_path, "accepted\n");
  set_socket_timeouts(client);
  close(listener);

  char request[sizeof(REQUEST) - 1];
  receive_exact(client, request, sizeof(request));
  if (memcmp(request, REQUEST, sizeof(request)) != 0)
    fail_message("outbound request mismatch");
  send_all(client, FIRST_INPUT, sizeof(FIRST_INPUT) - 1);

  char progress[sizeof(PROGRESS) - 1];
  receive_exact(client, progress, sizeof(progress));
  if (memcmp(progress, PROGRESS, sizeof(progress)) != 0)
    fail_message("outbound progress mismatch");
  send_all(client, SECOND_INPUT, sizeof(SECOND_INPUT) - 1);

  char completion[sizeof(COMPLETION) - 1];
  receive_exact(client, completion, sizeof(completion));
  if (memcmp(completion, COMPLETION, sizeof(completion)) != 0)
    fail_message("outbound completion mismatch");
  if (shutdown(client, SHUT_WR) != 0)
    fail("shutdown controller");

  char unexpected;
  for (;;) {
    ssize_t received = recv(client, &unexpected, 1, 0);
    if (received < 0 && errno == EINTR)
      continue;
    if (received < 0)
      fail("recv controller tail");
    if (received != 0)
      fail_message("unexpected outbound bytes followed the fixed request");
    break;
  }
  close(client);

  char report[256];
  int report_length = snprintf(
      report, sizeof(report),
      "controller=complete\noutbound_hex=%s\noutbound_fnv1a64=%016llx\n",
      OUTBOUND_HEX, (unsigned long long)expected_outbound_digest());
  if (report_length <= 0 || (size_t)report_length >= sizeof(report))
    fail_message("controller report did not fit its fixed buffer");
  publish_text(report_path, report);
  return 0;
}

struct reader {
  int fd;
  int index;
  pthread_barrier_t *start;
  char bytes[PAYLOAD_SIZE + 1];
  ssize_t received;
  short readiness;
  int error;
};

static void *run_reader(void *raw) {
  struct reader *reader = raw;
  int barrier = pthread_barrier_wait(reader->start);
  if (barrier != 0 && barrier != PTHREAD_BARRIER_SERIAL_THREAD) {
    reader->error = barrier;
    return NULL;
  }

  struct pollfd interest = {.fd = reader->fd, .events = POLLIN};
  int ready;
  do {
    ready = poll(&interest, 1, 5000);
  } while (ready < 0 && errno == EINTR);
  if (ready < 0) {
    reader->error = errno;
    return NULL;
  }
  if (ready != 1) {
    reader->error = ETIMEDOUT;
    return NULL;
  }
  reader->readiness = interest.revents;
  if (reader->readiness != POLLIN) {
    reader->error = EIO;
    return NULL;
  }

  do {
    reader->received = recv(reader->fd, reader->bytes, PAYLOAD_SIZE, 0);
  } while (reader->received < 0 && errno == EINTR);
  if (reader->received < 0) {
    reader->error = errno;
    return NULL;
  }
  if (reader->received == PAYLOAD_SIZE)
    reader->bytes[PAYLOAD_SIZE] = '\0';
  if (reader->received == PAYLOAD_SIZE &&
      memcmp(reader->bytes, FIRST_INPUT, PAYLOAD_SIZE) == 0) {
    send_all(reader->fd, PROGRESS, sizeof(PROGRESS) - 1);
  }
  return NULL;
}

static uint16_t parse_port(const char *port_text) {
  char *end = NULL;
  unsigned long parsed = strtoul(port_text, &end, 10);
  if (end == port_text || *end != '\0' || parsed == 0 || parsed > UINT16_MAX)
    fail_message("invalid controller port");
  return (uint16_t)parsed;
}

static struct sockaddr_in loopback_address(uint16_t port) {
  struct sockaddr_in address = {
      .sin_family = AF_INET,
      .sin_port = htons(port),
      .sin_addr.s_addr = htonl(INADDR_LOOPBACK),
  };
  return address;
}

/* A client that each mode ends differently, all of them refused by network
 * record/replay. Returns only if no refusal ended the run. */
/* Pass `fd` across a socket pair with SCM_RIGHTS and return the copy. */
static int pass_descriptor(int fd) {
  int pair[2];
  if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair) != 0)
    fail("socketpair");
  char byte = 'x';
  struct iovec part = {.iov_base = &byte, .iov_len = 1};
  union {
    struct cmsghdr header;
    char bytes[CMSG_SPACE(sizeof(int))];
  } control;
  memset(&control, 0, sizeof(control));
  struct msghdr message = {.msg_iov = &part,
                           .msg_iovlen = 1,
                           .msg_control = control.bytes,
                           .msg_controllen = sizeof(control.bytes)};
  struct cmsghdr *header = CMSG_FIRSTHDR(&message);
  header->cmsg_level = SOL_SOCKET;
  header->cmsg_type = SCM_RIGHTS;
  header->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(header), &fd, sizeof(int));
  if (sendmsg(pair[0], &message, 0) != 1)
    fail("sendmsg SCM_RIGHTS");
  memset(&control, 0, sizeof(control));
  message.msg_controllen = sizeof(control.bytes);
  if (recvmsg(pair[1], &message, MSG_CMSG_CLOEXEC) != 1)
    fail("recvmsg SCM_RIGHTS");
  header = CMSG_FIRSTHDR(&message);
  if (header == NULL || header->cmsg_type != SCM_RIGHTS)
    fail_message("no descriptor received");
  int received;
  memcpy(&received, CMSG_DATA(header), sizeof(int));
  return received;
}

/* Try one way around the channel; returns 1 if `mode` is not a guard mode. */
static int run_guard_probe(uint16_t port, const char *mode) {
  if (strcmp(mode, "ipv6-24") == 0) {
    /* Linux accepts the RFC 2133 length, without sin6_scope_id. */
    int fd = socket(AF_INET6, SOCK_STREAM | SOCK_CLOEXEC, 0);
    struct sockaddr_in6 address = {.sin6_family = AF_INET6,
                                   .sin6_port = htons(port)};
    if (fd < 0)
      fail("socket ipv6");
    if (connect(fd, (struct sockaddr *)&address, 24) != 0)
      fail("connect ipv6-24");
    return 0;
  }
  if (strcmp(mode, "netlink") == 0) {
    if (socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, 0) < 0)
      fail("socket netlink");
    return 0;
  }
  if (strcmp(mode, "abstract") == 0) {
    int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    struct sockaddr_un address = {.sun_family = AF_UNIX};
    static const char name[] = "hermit-network-replay-probe";
    if (fd < 0)
      fail("socket unix");
    memcpy(address.sun_path + 1, name, sizeof(name) - 1);
    connect(fd, (struct sockaddr *)&address,
            offsetof(struct sockaddr_un, sun_path) + sizeof(name));
    return 0;
  }
  /* Other modes must not open a socket here: replay matches sockets by
     their creation order. */
  if (strcmp(mode, "rcvtimeo") != 0 && strcmp(mode, "scm-rights") != 0 &&
      strcmp(mode, "epoll") != 0 && strcmp(mode, "async") != 0)
    return 1;
  int fd = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
  if (fd < 0)
    fail("socket guard");
  if (strcmp(mode, "rcvtimeo") == 0) {
    set_socket_timeouts(fd);
  } else if (strcmp(mode, "scm-rights") == 0) {
    pass_descriptor(fd);
  } else if (strcmp(mode, "epoll") == 0) {
    int epoll = epoll_create1(EPOLL_CLOEXEC);
    struct epoll_event event = {.events = EPOLLIN};
    if (epoll < 0)
      fail("epoll_create1");
    if (epoll_ctl(epoll, EPOLL_CTL_ADD, fd, &event) != 0)
      fail("epoll_ctl");
  } else {
    if (fcntl(fd, F_SETFL, O_ASYNC) != 0)
      fail("fcntl O_ASYNC");
  }
  return 0;
}

static int run_refused_client(const char *port_text, const char *mode) {
  set_deadline();
  if (run_guard_probe(parse_port(port_text), mode) == 0)
    return 0;
  struct sockaddr_in address = loopback_address(parse_port(port_text));
  if (strcmp(mode, "udp") == 0) {
    /* A resolver's IPv6 probe: creating and closing a socket stays allowed. */
    int probe = socket(AF_INET6, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    if (probe < 0)
      fail("socket probe");
    close(probe);
    int udp = socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    if (udp < 0)
      fail("socket udp");
    if (sendto(udp, REQUEST, sizeof(REQUEST) - 1, 0, (struct sockaddr *)&address,
               sizeof(address)) < 0)
      fail("sendto udp");
    return 0;
  }
  int socket_fd = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
  if (socket_fd < 0)
    fail("socket client");
  if (strcmp(mode, "listen") == 0) {
    address.sin_port = 0;
    if (bind(socket_fd, (struct sockaddr *)&address, sizeof(address)) != 0)
      fail("bind client");
    if (listen(socket_fd, 1) != 0)
      fail("listen client");
    return 0;
  }
  if (strcmp(mode, "unspecified") == 0)
    address.sin_addr.s_addr = htonl(INADDR_ANY);
  if (connect(socket_fd, (struct sockaddr *)&address, sizeof(address)) != 0)
    fail("connect client");
  if (strcmp(mode, "truncated") == 0) {
    send_all(socket_fd, REQUEST, 3);
  } else if (strcmp(mode, "sendmsg") == 0) {
    struct iovec part = {.iov_base = (void *)REQUEST, .iov_len = sizeof(REQUEST) - 1};
    struct msghdr message = {.msg_iov = &part, .msg_iovlen = 1};
    if (sendmsg(socket_fd, &message, MSG_NOSIGNAL) < 0)
      fail("sendmsg client");
  } else if (strcmp(mode, "sendfile") == 0) {
    int source = open("/dev/zero", O_RDONLY | O_CLOEXEC);
    if (source < 0)
      fail("open /dev/zero");
    if (sendfile(socket_fd, source, NULL, 1) < 0)
      fail("sendfile client");
  }
  close(socket_fd);
  return 0;
}

static int run_client(const char *port_text, int mismatch) {
  set_deadline();
  uint16_t port = parse_port(port_text);

  int socket_fd = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
  if (socket_fd < 0)
    fail("socket client");
  int low_water = PAYLOAD_SIZE;
  if (setsockopt(socket_fd, SOL_SOCKET, SO_RCVLOWAT, &low_water,
                 sizeof(low_water)) != 0)
    fail("setsockopt SO_RCVLOWAT");
  struct sockaddr_in address = loopback_address(port);
  if (connect(socket_fd, (struct sockaddr *)&address, sizeof(address)) != 0)
    fail("connect client");

  int alias = dup(socket_fd);
  if (alias < 0)
    fail("dup client socket");
  pthread_barrier_t start;
  if (pthread_barrier_init(&start, NULL, 3) != 0)
    fail_message("pthread_barrier_init failed");
  struct reader readers[2] = {
      {.fd = socket_fd, .index = 0, .start = &start},
      {.fd = alias, .index = 1, .start = &start},
  };
  pthread_t threads[2];
  for (size_t index = 0; index < 2; ++index) {
    int created = pthread_create(&threads[index], NULL, run_reader, &readers[index]);
    if (created != 0) {
      errno = created;
      fail("pthread_create");
    }
  }
  int barrier = pthread_barrier_wait(&start);
  if (barrier != 0 && barrier != PTHREAD_BARRIER_SERIAL_THREAD)
    fail_message("client start barrier failed");

  char request[sizeof(REQUEST) - 1];
  memcpy(request, REQUEST, sizeof(request));
  if (mismatch)
    request[3] ^= 0x20;
  send_all(socket_fd, request, sizeof(request));

  for (size_t index = 0; index < 2; ++index) {
    int joined = pthread_join(threads[index], NULL);
    if (joined != 0) {
      errno = joined;
      fail("pthread_join");
    }
    if (readers[index].error != 0) {
      errno = readers[index].error;
      fail("competing reader");
    }
    if (readers[index].received != PAYLOAD_SIZE)
      fail_message("competing reader did not consume exactly one input chunk");
  }
  pthread_barrier_destroy(&start);

  int first = memcmp(readers[0].bytes, FIRST_INPUT, PAYLOAD_SIZE) == 0 ? 0 : 1;
  int second = 1 - first;
  if (memcmp(readers[first].bytes, FIRST_INPUT, PAYLOAD_SIZE) != 0 ||
      memcmp(readers[second].bytes, SECOND_INPUT, PAYLOAD_SIZE) != 0)
    fail_message("competing readers did not consume the exact abc/def stream");

  send_all(socket_fd, COMPLETION, sizeof(COMPLETION) - 1);

  char eof_byte;
  ssize_t eof;
  do {
    eof = recv(socket_fd, &eof_byte, 1, 0);
  } while (eof < 0 && errno == EINTR);
  if (eof != 0)
    fail_message("peer half-close did not produce EOF after abcdef");

  printf("aggregate=abcdef readiness=pollin,pollin eof=1 outbound_hex=%s "
         "outbound_fnv1a64=%016llx\n",
         OUTBOUND_HEX, (unsigned long long)expected_outbound_digest());
  printf("reader-marker=%d:abc,%d:def\n", readers[first].index,
         readers[second].index);
  close(alias);
  close(socket_fd);
  return 0;
}

int main(int argc, char **argv) {
  if (argc == 5 && strcmp(argv[1], "controller") == 0)
    return run_controller(argv[2], argv[3], argv[4]);
  if (argc == 4 && strcmp(argv[1], "client") == 0) {
    if (strcmp(argv[3], "match") == 0)
      return run_client(argv[2], 0);
    if (strcmp(argv[3], "mismatch") == 0)
      return run_client(argv[2], 1);
    const char *refused[] = {"truncated", "sendmsg",    "sendfile", "unspecified",
                             "udp",       "listen",     "ipv6-24",  "netlink",
                             "abstract",  "rcvtimeo",   "scm-rights", "epoll",
                             "async"};
    for (size_t index = 0; index < sizeof(refused) / sizeof(refused[0]); ++index)
      if (strcmp(argv[3], refused[index]) == 0)
        return run_refused_client(argv[2], argv[3]);
  }
  fprintf(stderr,
          "usage: %s controller PORT_FILE REPORT_FILE CONTACT_FILE | client PORT "
          "match|mismatch|truncated|sendmsg|sendfile|unspecified|udp|listen|"
          "ipv6-24|netlink|abstract|rcvtimeo|scm-rights|epoll|async\n",
          argv[0]);
  return 2;
}
