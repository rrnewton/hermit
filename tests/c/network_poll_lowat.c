/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

/* An additive LOWAT regression. The original TCP bracket is unchanged. */
#define FIXTURE_DEADLINE_SECONDS 8
#define POLL_DEADLINE_MS 1000
#define OPPORTUNITY_NS 20000000L

static void require(int condition, const char *message) {
  if (!condition) {
    fprintf(stderr, "LOWAT fixture failure: %s (errno=%d)\n", message, errno);
    exit(1);
  }
}
static void send_exact(int fd, const char *bytes, size_t count) {
  while (count) {
    ssize_t n = send(fd, bytes, count, MSG_NOSIGNAL);
    if (n < 0 && errno == EINTR) continue;
    require(n > 0, "send exact protocol");
    bytes += n;
    count -= (size_t)n;
  }
}
static void receive_exact(int fd, char *bytes, size_t count) {
  while (count) {
    ssize_t n = recv(fd, bytes, count, 0);
    if (n < 0 && errno == EINTR) continue;
    require(n > 0, "receive exact protocol");
    bytes += n;
    count -= (size_t)n;
  }
}
static void expect_command(int fd, const char *expected) {
  char bytes[32] = {0};
  size_t n = strlen(expected);
  require(n < sizeof(bytes), "bounded command");
  receive_exact(fd, bytes, n);
  require(memcmp(bytes, expected, n) == 0, "unchanged outbound protocol");
}
static void opportunity(void) {
  struct timespec duration = {.tv_sec = 0, .tv_nsec = OPPORTUNITY_NS};
  require(nanosleep(&duration, NULL) == 0, "guest logical opportunity timer");
}

struct reader_state {
  int fd;
  int receive_mode;
  atomic_int intent;
  atomic_int completed;
  long tid;
};
static void *reader(void *raw) {
  struct reader_state *state = raw;
  state->tid = syscall(SYS_gettid);
  atomic_store_explicit(&state->intent, 1, memory_order_release);
  char bytes[11] = {0};
  if (state->receive_mode) {
    /* Default receive flags: the entry target is LOWAT10, not MSG_WAITALL. */
    ssize_t n = recv(state->fd, bytes, 10, 0);
    require(n == 10, "in-progress recv retains its original target10");
    require(memcmp(bytes, "abcdefghij", 10) == 0, "exact recv payload");
  } else {
    struct pollfd wait = {.fd = state->fd, .events = POLLIN};
    require(poll(&wait, 1, POLL_DEADLINE_MS) == 1, "lowering LOWAT wakes poll");
    require(wait.revents == POLLIN, "poll has exactly POLLIN, no terminal shortcut");
    ssize_t n = recv(state->fd, bytes, 3, MSG_DONTWAIT);
    require(n == 3 && memcmp(bytes, "abc", 3) == 0, "poll preserved all three bytes");
  }
  atomic_store_explicit(&state->completed, 1, memory_order_release);
  return NULL;
}

static void native_blocked_witness(long tid, int fd, int receive_mode) {
  char path[128];
  require(snprintf(path, sizeof(path), "/proc/self/task/%ld/syscall", tid) > 0, "native task path");
  for (;;) {
    FILE *file = fopen(path, "r");
    require(file != NULL, "native task syscall witness open");
    char line[512] = {0};
    require(fgets(line, sizeof(line), file) != NULL && fclose(file) == 0, "native task syscall witness read");
    long number = -1;
    unsigned long long first = 0;
    if (sscanf(line, "%ld %llx", &number, &first) == 2 &&
        ((!receive_mode && number == SYS_poll) ||
         (receive_mode && number == SYS_recvfrom && first == (unsigned)fd))) {
      printf("native-blocked reader=%ld syscall=%ld\n", tid, number);
      return;
    }
    struct timespec pause = {.tv_sec = 0, .tv_nsec = 1000000};
    require(nanosleep(&pause, NULL) == 0, "native witness bounded wait");
  }
}

static void client(int port, int receive_mode, int native_witness) {
  int fd = socket(AF_INET, SOCK_STREAM, 0);
  require(fd >= 0, "create client socket");
  struct sockaddr_in address = {.sin_family = AF_INET, .sin_port = htons((unsigned short)port)};
  require(inet_pton(AF_INET, "127.0.0.1", &address.sin_addr) == 1, "loopback address");
  require(connect(fd, (struct sockaddr *)&address, sizeof(address)) == 0, "connect client");
  int buffer = 131072;
  require(setsockopt(fd, SOL_SOCKET, SO_RCVBUF, &buffer, sizeof(buffer)) == 0, "user-lock receive buffer");
  socklen_t length = sizeof(buffer);
  require(getsockopt(fd, SOL_SOCKET, SO_RCVBUF, &buffer, &length) == 0 && length == sizeof(buffer) && buffer >= 20,
          "receive buffer can represent LOWAT10");
  int lowat = 10;
  require(setsockopt(fd, SOL_SOCKET, SO_RCVLOWAT, &lowat, sizeof(lowat)) == 0, "set initial LOWAT10");
  send_exact(fd, receive_mode ? "recv\n" : "poll\n", 5);
  char peek[3];
  for (;;) {
    ssize_t n = recv(fd, peek, sizeof(peek), MSG_PEEK | MSG_DONTWAIT);
    if (n == 3) break;
    require((n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) || (n > 0 && n < 3), "peek pending three bytes");
    sched_yield();
  }
  require(memcmp(peek, "abc", 3) == 0, "exact initial queued prefix");
  struct reader_state state = {.fd = fd, .receive_mode = receive_mode};
  atomic_init(&state.intent, 0);
  atomic_init(&state.completed, 0);
  pthread_t thread;
  require(pthread_create(&thread, NULL, reader, &state) == 0, "start reader");
  while (!atomic_load_explicit(&state.intent, memory_order_acquire)) sched_yield();
  if (native_witness) native_blocked_witness(state.tid, fd, receive_mode);
  else opportunity();
  require(!atomic_load_explicit(&state.completed, memory_order_acquire), "reader has not completed at LOWAT10");
  lowat = 1;
  require(setsockopt(fd, SOL_SOCKET, SO_RCVLOWAT, &lowat, sizeof(lowat)) == 0, "lower LOWAT1");
  if (receive_mode) {
    opportunity();
    require(!atomic_load_explicit(&state.completed, memory_order_acquire), "recv does not complete on lowat decrease alone");
    send_exact(fd, "tail\n", 5);
  }
  require(pthread_join(thread, NULL) == 0, "join reader");
  require(atomic_load_explicit(&state.completed, memory_order_acquire), "reader completion");
  send_exact(fd, "done\n", 5);
  printf("lowat-case=%s reader=%ld fd=%d prefix=abc result=%s\n",
         receive_mode ? "recv" : "poll", state.tid, fd, receive_mode ? "10:abcdefghij" : "POLLIN,3:abc");
  require(close(fd) == 0, "close client socket");
}

static void controller(const char *port_path, const char *report_path) {
  int server = socket(AF_INET, SOCK_STREAM, 0);
  require(server >= 0, "controller socket");
  struct sockaddr_in address = {.sin_family = AF_INET, .sin_port = 0};
  require(inet_pton(AF_INET, "127.0.0.1", &address.sin_addr) == 1, "controller loopback");
  require(bind(server, (struct sockaddr *)&address, sizeof(address)) == 0, "controller bind");
  require(listen(server, 1) == 0, "controller listen");
  socklen_t length = sizeof(address);
  require(getsockname(server, (struct sockaddr *)&address, &length) == 0, "controller port");
  FILE *port = fopen(port_path, "wx");
  require(port != NULL, "new controller port file");
  require(fprintf(port, "%u\n", ntohs(address.sin_port)) > 0 && fclose(port) == 0, "publish controller port");
  int peer = accept(server, NULL, NULL);
  require(peer >= 0, "controller accept");
  char mode[5];
  receive_exact(peer, mode, sizeof(mode));
  require(memcmp(mode, "poll\n", 5) == 0 || memcmp(mode, "recv\n", 5) == 0, "controller mode");
  int receive_mode = memcmp(mode, "recv\n", 5) == 0;
  send_exact(peer, "abc", 3);
  if (receive_mode) {
    expect_command(peer, "tail\n");
    send_exact(peer, "defghij", 7);
  }
  expect_command(peer, "done\n");
  char extra;
  require(recv(peer, &extra, 1, 0) == 0, "exact outbound end-of-stream");
  require(close(peer) == 0 && close(server) == 0, "close controller sockets");
  FILE *report = fopen(report_path, "wx");
  require(report != NULL, "new controller report");
  require(fprintf(report, "controller=complete case=%s outbound=%s\n", receive_mode ? "recv" : "poll",
                  receive_mode ? "recv,tail,done" : "poll,done") > 0 && fclose(report) == 0, "complete controller report");
}
int main(int argc, char **argv) {
  signal(SIGPIPE, SIG_IGN);
  alarm(FIXTURE_DEADLINE_SECONDS);
  if (argc == 4 && strcmp(argv[1], "controller") == 0) {
    controller(argv[2], argv[3]);
  } else if ((argc == 4 && strcmp(argv[1], "client") == 0) ||
             (argc == 5 && strcmp(argv[1], "client") == 0 && strcmp(argv[4], "native-witness") == 0)) {
    char *end = NULL;
    long port = strtol(argv[2], &end, 10);
    require(end && *end == '\0' && port > 0 && port <= 65535, "numeric controller port");
    require(strcmp(argv[3], "poll") == 0 || strcmp(argv[3], "recv") == 0, "client mode");
    client((int)port, strcmp(argv[3], "recv") == 0, argc == 5);
  } else {
    fprintf(stderr, "usage: %s controller PORT REPORT | client PORT poll|recv [native-witness]\n", argv[0]);
    return 2;
  }
  return 0;
}
