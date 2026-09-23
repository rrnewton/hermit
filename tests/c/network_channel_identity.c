#define _GNU_SOURCE
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#define CHECK(condition) do { if (!(condition)) { fprintf(stderr, "identity assertion line=%d: %s errno=%d\n", __LINE__, #condition, errno); exit(1); } } while (0)

static void transfer(int fd, void *bytes, size_t count, int sending) {
  char *cursor = bytes;
  while (count) {
    ssize_t result = sending ? write(fd, cursor, count) : read(fd, cursor, count);
    if (result < 0 && errno == EINTR) continue;
    CHECK(result > 0);
    cursor += result;
    count -= (size_t)result;
  }
}

static void publish(const char *path, const char *text) {
  char temporary[4096];
  int count = snprintf(temporary, sizeof(temporary), "%s.tmp.%ld", path, (long)getpid());
  CHECK(count > 0 && (size_t)count < sizeof(temporary));
  int fd = open(temporary, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0600);
  CHECK(fd >= 0);
  transfer(fd, (void *)text, strlen(text), 1);
  CHECK(close(fd) == 0);
  CHECK(rename(temporary, path) == 0);
}

static int socket_stream(void) {
  int fd = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
  CHECK(fd >= 0);
  int options[3] = {SO_DOMAIN, SO_TYPE, SO_PROTOCOL};
  int expected[3] = {AF_INET, SOCK_STREAM, IPPROTO_TCP};
  for (int i = 0; i < 3; ++i) {
    int value = -1; socklen_t length = sizeof(value);
    CHECK(getsockopt(fd, SOL_SOCKET, options[i], &value, &length) == 0);
    CHECK(length == sizeof(value) && value == expected[i]);
  }
  return fd;
}

static int listen_at(unsigned short *port) {
  int fd = socket_stream();
  struct sockaddr_in address = {.sin_family=AF_INET, .sin_addr.s_addr=htonl(INADDR_LOOPBACK)};
  CHECK(bind(fd, (struct sockaddr *)&address, sizeof(address)) == 0);
  socklen_t length = sizeof(address);
  CHECK(getsockname(fd, (struct sockaddr *)&address, &length) == 0);
  CHECK(length == sizeof(address));
  CHECK(listen(fd, 4) == 0);
  *port = ntohs(address.sin_port);
  return fd;
}

static int controller(const char *scenario, const char *ports, const char *report) {
  CHECK(!strcmp(scenario,"distinct") || !strcmp(scenario,"same") || !strcmp(scenario,"watch"));
  unsigned short port[2]; int listener[2];
  listener[0] = listen_at(&port[0]);
  if (!strcmp(scenario, "distinct")) listener[1] = listen_at(&port[1]);
  else { listener[1] = listener[0]; port[1] = port[0]; }
  char text[64]; snprintf(text, sizeof(text), "%u %u\n", port[0], port[1]); publish(ports, text);
  if (!strcmp(scenario, "watch")) {
    struct pollfd controls[2] = {
      {.fd=listener[0], .events=POLLIN}, {.fd=STDIN_FILENO, .events=POLLIN},
    };
    CHECK(poll(controls, 2, -1) > 0);
    if (!(controls[0].revents & POLLIN)) {
      CHECK(controls[1].revents & POLLIN);
      char stop; transfer(STDIN_FILENO, &stop, 1, 0); CHECK(stop == 'S');
      // The caller sends stop only after the guest has terminated. Account for
      // queued connections before acknowledging the absence of contact.
      CHECK(poll(controls, 1, 0) >= 0);
    }
    if (controls[0].revents & POLLIN) {
      int fd = accept4(listener[0], NULL, NULL, SOCK_CLOEXEC); CHECK(fd >= 0);
      publish(report, "contact=accepted\n"); CHECK(close(fd) == 0);
    } else {
      CHECK(controls[0].revents == 0); publish(report, "contact=none\n");
    }
    CHECK(close(listener[0]) == 0); return 0;
  }
  unsigned seen = 0;
  for (int ordinal = 0; ordinal < 2; ++ordinal) {
    int i = ordinal;
    if (listener[0] != listener[1]) {
      struct pollfd polls[2] = {
        {.fd=(seen & 1) ? -1 : listener[0], .events=POLLIN},
        {.fd=(seen & 2) ? -1 : listener[1], .events=POLLIN},
      };
      CHECK(poll(polls, 2, -1) > 0);
      i = (polls[0].revents & POLLIN) ? 0 : 1;
      CHECK(polls[i].revents == POLLIN);
    }
    CHECK(!(seen & (1U << i))); seen |= 1U << i;
    int fd = accept4(listener[i], NULL, NULL, SOCK_CLOEXEC); CHECK(fd >= 0);
    char request; transfer(fd, &request, 1, 0); CHECK(request == 'A' + i);
    char reply[3]; memcpy(reply, i ? "two" : "one", sizeof(reply)); transfer(fd, reply, sizeof(reply), 1);
    char done; transfer(fd, &done, 1, 0); CHECK(done == '!');
    CHECK(shutdown(fd, SHUT_WR) == 0); CHECK(close(fd) == 0);
  }
  CHECK(seen == 3);
  CHECK(close(listener[0]) == 0);
  if (listener[1] != listener[0]) CHECK(close(listener[1]) == 0);
  publish(report, "accepted=2\nrequests_by_peer=A!,B!\nresponses=one,two\n");
  return 0;
}

static unsigned short port_number(const char *value) {
  char *end = NULL; unsigned long port = strtoul(value, &end, 10);
  CHECK(end && !*end && port > 0 && port <= 65535); return (unsigned short)port;
}

static int connect_at(int fd, unsigned short port) {
  struct sockaddr_in peer = {.sin_family=AF_INET, .sin_port=htons(port), .sin_addr.s_addr=htonl(INADDR_LOOPBACK)};
  return connect(fd, (struct sockaddr *)&peer, sizeof(peer));
}

static int client(char **argv) {
  unsigned short ports[2] = {port_number(argv[2]), port_number(argv[3])};
  const char *creation = argv[4], *connection = argv[5], *wrong_bound = argv[6];
  CHECK(!strcmp(creation,"ab") || !strcmp(creation,"ba"));
  CHECK(!strcmp(connection,"ab") || !strcmp(connection,"ba"));
  int fd[2];
  // These are genuinely different socket() creation orders, not dup aliases or
  // a reordered array after creating the sockets in the same order.
  if (!strcmp(creation,"ab")) { fd[0] = socket_stream(); fd[1] = socket_stream(); CHECK(fd[0] < fd[1]); }
  else { fd[1] = socket_stream(); fd[0] = socket_stream(); CHECK(fd[1] < fd[0]); }
  char observed[2][4] = {{0}};
  for (int ordinal = 0; ordinal < 2; ++ordinal) {
    int i = connection[ordinal] - 'a'; CHECK(i == 0 || i == 1);
    CHECK(connect_at(fd[i], ports[i]) == 0);
    char request = 'A' + i; transfer(fd[i], &request, 1, 1);
    transfer(fd[i], observed[i], 3, 0); CHECK(!memcmp(observed[i], i ? "two" : "one", 3));
    char done = '!'; transfer(fd[i], &done, 1, 1);
    char eof; CHECK(read(fd[i], &eof, 1) == 0);
    if (strcmp(wrong_bound,"0")) {
      // A return here, even EISCONN from a kernel call, is not the requested
      // engine refusal. The Rust oracle requires ChannelEndpointMismatch.
      (void)connect_at(fd[i], port_number(wrong_bound));
      fprintf(stderr, "already-bound wrong-peer connect unexpectedly returned\n"); return 99;
    }
  }
  CHECK(close(fd[0]) == 0); CHECK(close(fd[1]) == 0);
  printf("created=%s connected=%s payloads=%s,%s eof=2 requests_by_peer=A!,B!\n", creation, connection, observed[0], observed[1]);
  return 0;
}

int main(int argc, char **argv) {
  signal(SIGPIPE, SIG_IGN); alarm(8);
  CHECK(argc >= 2);
  if (!strcmp(argv[1],"controller")) { CHECK(argc == 5); return controller(argv[2],argv[3],argv[4]); }
  CHECK(argc == 7 && !strcmp(argv[1],"client")); return client(argv);
}
