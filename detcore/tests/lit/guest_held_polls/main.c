/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Which blocking polls Detcore classifies as waiting on guest state only
// (`Resources::polls_guest_state_only`), the property that lets the scheduler
// treat an idle poller like a blocked thread, and when the scheduler stops
// trusting that. Each mode polls one descriptor until a peer acts:
//
//   same        an accepted socket whose peer this process connected:
//               guest-held.
//   peer-exits  the same, and the peer thread closes its end instead of
//               writing: still guest-held, and the poll sees the hangup.
//   child       an accepted socket whose peer is a forked child, which exits
//               without writing: not provably guest-held, never classified.
//   exported    a same-process pair, polled after the process passes a
//               descriptor over SCM_RIGHTS on a forked child's connection:
//               still classified, but the scheduler no longer trusts any
//               guest-held mark.
//   connect     the connecting end of a connection a forked child accepts.
//               Its SO_PEERCRED names this process, which called listen: only
//               guests hold that listener, so the acceptor is a guest, and the
//               poll is classified.
//   listener-export a second listener, passed to a forked child over a
//               plain connection, which accepts this process's connection
//               to it: the export revokes every mark, and the listener is no
//               longer one only guests hold, so the connection is never
//               classified.
//   exported-listener-kept a third listener, passed to a forked child over
//               a plain connection, which this process keeps, connects to and
//               accepts from itself: the export revokes every mark, and the
//               listener is no longer one only guests hold, so neither end of
//               the connection is classified.
//   dgram-named a same-process pair, polled after the process passes an
//               eventfd on a datagram socketpair end to a named socket of a
//               forked child: a datagram end is never guest-held, since it can
//               send to any name, so the send revokes.
//   bound-pair  a datagram socketpair end, bound to a name so that any
//               process may reach it: its poll is never classified.
//   crossexport a pipe whose write end a forked child passes, over a plain
//               connection, to another child that writes: the poll on the
//               read end is classified, and the export revokes every mark.
//   self-join   a pipe's read end, passed by this process to itself over a
//               connection it made (a guest-held channel) and received:
//               the received descriptor joins the sender's open file and
//               keeps its mark, so its poll is classified, and nothing is
//               revoked.
//   export-mmsg the same pair as exported, but the descriptor leaves the
//               process through sendmmsg: the export revokes every mark.
//   unspec-pair a datagram socketpair end that disconnects (connect with
//               AF_UNSPEC), which its old peer still reaches: its mark is
//               withdrawn, so its poll is never classified.
//   self-listener a listener this process passes to itself over a
//               connection it made, closing the original before it receives
//               it, so the received copy is a description of its own: its
//               credentials name this process, but any process may connect
//               to it, so its poll is never classified.
//   joined-listener the same, with the original kept open, so the received
//               copy joins it: neither copy's poll is classified.
//   received-connect an unconnected socket a forked child made and passed
//               over a socketpair (a guest-held channel), closing its copy.
//               The queued message keeps the child's open file, so the
//               received socket joins it, as Linux installs the sender's
//               file, and shares its provenance: a guest made it, and it
//               crossed only a guest-held channel, so every holder of it has
//               been a guest. This process connects it to its own listener,
//               and both its poll and the accepted end's are classified.
//   received-listener a socket received the same way, which this process
//               binds and listens on: a client this process makes and
//               connects to it, and the accepted end, are classified, for
//               the same reason.
//   forwarded-socket a client socket a forked child connected to this
//               process's listener, passed over a socketpair (a guest-held
//               channel) after the child closed its own copy. The received
//               copy joins the child's open file, which the queued message
//               keeps, and shares its mark: none, since the child's connect
//               found the listener's credentials naming this process, not the
//               child. A receipt never marks a socket from its credentials,
//               which name its peer, not every holder of its open file, so
//               its poll is never classified.
//
// Use a bogus 'RUN' directive so this test isn't disabled:
// RUN: true

#define _GNU_SOURCE
#include <assert.h>
#include <poll.h>
#include <pthread.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int listener;
static struct sockaddr_un address;
static socklen_t address_length;

// A listener on abstract name `name`, so the test needs no file system path.
static int listen_on(const char* name, struct sockaddr_un* at, socklen_t* at_length) {
  int fd = socket(AF_UNIX, SOCK_STREAM, 0);
  assert(fd >= 0);
  memset(at, 0, sizeof(*at));
  at->sun_family = AF_UNIX;
  memcpy(at->sun_path + 1, name, strlen(name));
  *at_length = offsetof(struct sockaddr_un, sun_path) + 1 + strlen(name);
  assert(bind(fd, (struct sockaddr*)at, *at_length) == 0);
  assert(listen(fd, 4) == 0);
  return fd;
}

static void listen_on_abstract_name(void) {
  listener = listen_on("hermit-guest-held-polls", &address, &address_length);
}

static int connect_to(const struct sockaddr_un* at, socklen_t at_length) {
  int fd = socket(AF_UNIX, SOCK_STREAM, 0);
  assert(fd >= 0);
  assert(connect(fd, (const struct sockaddr*)at, at_length) == 0);
  return fd;
}

static int connect_to_listener(void) {
  return connect_to(&address, address_length);
}

static void sleep_ms(long ms) {
  struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
  assert(nanosleep(&ts, NULL) == 0);
}

struct peer {
  int fd;
  int close_instead;
};

// Acts on the peer end after a pause, so the poller retries meanwhile.
static void* act_later(void* arg) {
  struct peer* peer = arg;
  sleep_ms(20);
  if (peer->close_instead) {
    assert(close(peer->fd) == 0);
  } else {
    assert(write(peer->fd, "x", 1) == 1);
  }
  return NULL;
}

static short poll_one(int fd) {
  struct pollfd entry = {.fd = fd, .events = POLLIN};
  assert(poll(&entry, 1, -1) == 1);
  return entry.revents;
}

struct pair {
  struct peer connecting;
  int accepted;
};

// A connection whose two ends this process holds.
static struct pair same_process_pair(int close_instead) {
  struct pair pair = {{connect_to_listener(), close_instead}, -1};
  pair.accepted = accept(listener, NULL, NULL);
  assert(pair.accepted >= 0);
  return pair;
}

// Polls the accepted end of `pair` until a thread acts on the connecting end.
static short poll_pair(struct pair* pair) {
  pthread_t thread;
  assert(pthread_create(&thread, NULL, act_later, &pair->connecting) == 0);
  short revents = poll_one(pair->accepted);
  assert(pthread_join(thread, NULL) == 0);
  return revents;
}

// Passes descriptor `passed` over SCM_RIGHTS on `fd`, to `to` if not NULL.
static void pass_descriptor_to(int fd, int passed, const struct sockaddr_un* to, socklen_t to_length);

static void pass_descriptor(int fd, int passed) {
  pass_descriptor_to(fd, passed, NULL, 0);
}

static void pass_descriptor_to(int fd, int passed, const struct sockaddr_un* to, socklen_t to_length) {
  char control[CMSG_SPACE(sizeof(int))];
  memset(control, 0, sizeof(control));
  char byte = 'p';
  struct iovec iov = {.iov_base = &byte, .iov_len = 1};
  struct msghdr message = {
      .msg_name = (void*)to,
      .msg_namelen = to_length,
      .msg_iov = &iov,
      .msg_iovlen = 1,
      .msg_control = control,
      .msg_controllen = sizeof(control)};
  struct cmsghdr* header = CMSG_FIRSTHDR(&message);
  header->cmsg_level = SOL_SOCKET;
  header->cmsg_type = SCM_RIGHTS;
  header->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(header), &passed, sizeof(int));
  assert(sendmsg(fd, &message, 0) == 1);
}

// Passes an eventfd over SCM_RIGHTS on `fd`.
static void pass_a_descriptor(int fd) {
  int passed = eventfd(0, 0);
  assert(passed >= 0);
  pass_descriptor(fd, passed);
  assert(close(passed) == 0);
}

// Passes an eventfd over SCM_RIGHTS on `fd`, through sendmmsg.
static void pass_a_descriptor_mmsg(int fd) {
  int passed = eventfd(0, 0);
  assert(passed >= 0);
  char control[CMSG_SPACE(sizeof(int))];
  memset(control, 0, sizeof(control));
  char byte = 'm';
  struct iovec iov = {.iov_base = &byte, .iov_len = 1};
  struct mmsghdr entry;
  memset(&entry, 0, sizeof(entry));
  entry.msg_hdr.msg_iov = &iov;
  entry.msg_hdr.msg_iovlen = 1;
  entry.msg_hdr.msg_control = control;
  entry.msg_hdr.msg_controllen = sizeof(control);
  struct cmsghdr* header = CMSG_FIRSTHDR(&entry.msg_hdr);
  header->cmsg_level = SOL_SOCKET;
  header->cmsg_type = SCM_RIGHTS;
  header->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(header), &passed, sizeof(int));
  assert(sendmmsg(fd, &entry, 1, 0) == 1);
  assert(close(passed) == 0);
}

// Connects to `at` after a pause, so the listener's poller retries meanwhile.
struct delayed_connect {
  const struct sockaddr_un* at;
  socklen_t at_length;
};

static void* connect_later(void* arg) {
  struct delayed_connect* target = arg;
  sleep_ms(20);
  int fd = connect_to(target->at, target->at_length);
  (void)fd;
  return NULL;
}

// Polls listener `fd` until a thread connects to `at`, then accepts.
static short poll_listener(int fd, struct delayed_connect* target) {
  pthread_t thread;
  assert(pthread_create(&thread, NULL, connect_later, target) == 0);
  short revents = poll_one(fd);
  assert(pthread_join(thread, NULL) == 0);
  assert(accept(fd, NULL, NULL) >= 0);
  return revents;
}

// Receives one descriptor passed over SCM_RIGHTS on `fd`.
static int receive_descriptor(int fd) {
  char control[CMSG_SPACE(sizeof(int))];
  memset(control, 0, sizeof(control));
  char byte;
  struct iovec iov = {.iov_base = &byte, .iov_len = 1};
  struct msghdr message = {
      .msg_iov = &iov,
      .msg_iovlen = 1,
      .msg_control = control,
      .msg_controllen = sizeof(control)};
  assert(recvmsg(fd, &message, 0) == 1);
  struct cmsghdr* header = CMSG_FIRSTHDR(&message);
  assert(header && header->cmsg_type == SCM_RIGHTS);
  int received;
  memcpy(&received, CMSG_DATA(header), sizeof(int));
  return received;
}

static void reap(pid_t child);

// A stream socket a forked child made and passed over a socketpair, which
// the child closes, exiting, before this process receives it: the queued
// message keeps the child's open file, which the received socket joins.
static int receive_a_childs_socket(void) {
  int pair[2];
  assert(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0);
  pid_t child = fork();
  assert(child >= 0);
  if (child == 0) {
    assert(close(pair[0]) == 0);
    int made = socket(AF_UNIX, SOCK_STREAM, 0);
    assert(made >= 0);
    pass_descriptor(pair[1], made);
    assert(close(made) == 0);
    _exit(0);
  }
  assert(close(pair[1]) == 0);
  reap(child);
  int received = receive_descriptor(pair[0]);
  assert(close(pair[0]) == 0);
  return received;
}

static void reap(pid_t child) {
  int status;
  assert(waitpid(child, &status, 0) == child && WIFEXITED(status));
}

// Forks a child that connects, waits for one byte or end of file, pauses and
// exits; returns the parent's end of that connection in `child_end_of_parent`.
static int fork_connected_child(int* child_end_of_parent) {
  pid_t child = fork();
  assert(child >= 0);
  if (child == 0) {
    int fd = connect_to_listener();
    char byte;
    // Ends when the parent passes its byte or closes the connection.
    (void)read(fd, &byte, 1);
    sleep_ms(20);
    _exit(0);
  }
  *child_end_of_parent = accept(listener, NULL, NULL);
  assert(*child_end_of_parent >= 0);
  return child;
}

int main(int argc, char** argv) {
  assert(argc == 2);
  const char* mode = argv[1];
  listen_on_abstract_name();
  if (strcmp(mode, "same") == 0) {
    struct pair pair = same_process_pair(0);
    printf("same: revents %#x\n", poll_pair(&pair));
  } else if (strcmp(mode, "peer-exits") == 0) {
    struct pair pair = same_process_pair(1);
    short revents = poll_pair(&pair);
    printf("peer-exits: hangup %d\n", (revents & POLLHUP) != 0);
  } else if (strcmp(mode, "child") == 0) {
    int to_child;
    pid_t child = fork_connected_child(&to_child);
    assert(write(to_child, "go", 1) == 1);
    short revents = poll_one(to_child);
    printf("child: hangup %d\n", (revents & POLLHUP) != 0);
    int status;
    assert(waitpid(child, &status, 0) == child && WIFEXITED(status));
  } else if (strcmp(mode, "exported") == 0) {
    // The pair exists before the descriptor leaves the process; a connection
    // made afterwards would be classified afresh.
    struct pair pair = same_process_pair(0);
    int to_child;
    pid_t child = fork_connected_child(&to_child);
    pass_a_descriptor(to_child);
    printf("exported: revents %#x\n", poll_pair(&pair));
    int status;
    assert(waitpid(child, &status, 0) == child && WIFEXITED(status));
  } else if (strcmp(mode, "connect") == 0) {
    pid_t acceptor = fork();
    assert(acceptor >= 0);
    if (acceptor == 0) {
      int accepted = accept(listener, NULL, NULL);
      assert(accepted >= 0);
      sleep_ms(20);
      assert(write(accepted, "x", 1) == 1);
      _exit(0);
    }
    int connecting = connect_to_listener();
    // The acceptor may also have exited by then: report readability only.
    printf("connect: readable %d\n", (poll_one(connecting) & POLLIN) != 0);
    reap(acceptor);
  } else if (strcmp(mode, "listener-export") == 0) {
    struct sockaddr_un second_address;
    socklen_t second_length;
    int second = listen_on("hermit-guest-held-polls-2", &second_address, &second_length);
    pid_t child = fork();
    assert(child >= 0);
    if (child == 0) {
      assert(close(second) == 0);
      int passed = receive_descriptor(connect_to_listener());
      int accepted = accept(passed, NULL, NULL);
      assert(accepted >= 0);
      sleep_ms(20);
      assert(write(accepted, "x", 1) == 1);
      _exit(0);
    }
    int to_child = accept(listener, NULL, NULL);
    assert(to_child >= 0);
    pass_descriptor(to_child, second);
    assert(close(second) == 0);
    int connecting = connect_to(&second_address, second_length);
    // The acceptor may also have exited by then: report readability only.
    printf("listener-export: readable %d\n", (poll_one(connecting) & POLLIN) != 0);
    reap(child);
  } else if (strcmp(mode, "exported-listener-kept") == 0) {
    struct sockaddr_un third_address;
    socklen_t third_length;
    int third = listen_on("hermit-guest-held-polls-6", &third_address, &third_length);
    pid_t child = fork();
    assert(child >= 0);
    if (child == 0) {
      assert(close(third) == 0);
      int passed = receive_descriptor(connect_to_listener());
      assert(close(passed) == 0);
      _exit(0);
    }
    int to_child = accept(listener, NULL, NULL);
    assert(to_child >= 0);
    pass_descriptor(to_child, third);
    reap(child);
    int connecting = connect_to(&third_address, third_length);
    int accepted = accept(third, NULL, NULL);
    assert(accepted >= 0);
    struct peer writer = {accepted, 0};
    pthread_t thread;
    assert(pthread_create(&thread, NULL, act_later, &writer) == 0);
    short connected = poll_one(connecting);
    assert(pthread_join(thread, NULL) == 0);
    struct peer other_writer = {connecting, 0};
    assert(pthread_create(&thread, NULL, act_later, &other_writer) == 0);
    short accepted_end = poll_one(accepted);
    assert(pthread_join(thread, NULL) == 0);
    printf("exported-listener-kept: revents %#x %#x\n", connected, accepted_end);
  } else if (strcmp(mode, "dgram-named") == 0) {
    // A named datagram socket in a child, which takes one message.
    struct sockaddr_un named;
    socklen_t named_length;
    int receiver_fd = socket(AF_UNIX, SOCK_DGRAM, 0);
    assert(receiver_fd >= 0);
    memset(&named, 0, sizeof(named));
    named.sun_family = AF_UNIX;
    const char name[] = "hermit-guest-held-polls-dgram";
    memcpy(named.sun_path + 1, name, sizeof(name) - 1);
    named_length = offsetof(struct sockaddr_un, sun_path) + 1 + sizeof(name) - 1;
    assert(bind(receiver_fd, (struct sockaddr*)&named, named_length) == 0);
    pid_t receiver = fork();
    assert(receiver >= 0);
    if (receiver == 0) {
      (void)receive_descriptor(receiver_fd);
      _exit(0);
    }
    assert(close(receiver_fd) == 0);
    int ends[2];
    assert(socketpair(AF_UNIX, SOCK_DGRAM, 0, ends) == 0);
    int passed = eventfd(0, 0);
    assert(passed >= 0);
    pass_descriptor_to(ends[0], passed, &named, named_length);
    struct pair pair = same_process_pair(0);
    printf("dgram-named: revents %#x\n", poll_pair(&pair));
    reap(receiver);
  } else if (strcmp(mode, "bound-pair") == 0) {
    int ends[2];
    assert(socketpair(AF_UNIX, SOCK_DGRAM, 0, ends) == 0);
    struct sockaddr_un named;
    memset(&named, 0, sizeof(named));
    named.sun_family = AF_UNIX;
    const char name[] = "hermit-guest-held-polls-bound";
    memcpy(named.sun_path + 1, name, sizeof(name) - 1);
    assert(bind(ends[1], (struct sockaddr*)&named,
                offsetof(struct sockaddr_un, sun_path) + 1 + sizeof(name) - 1) == 0);
    struct peer writer = {ends[0], 0};
    pthread_t thread;
    assert(pthread_create(&thread, NULL, act_later, &writer) == 0);
    printf("bound-pair: revents %#x\n", poll_one(ends[1]));
    assert(pthread_join(thread, NULL) == 0);
  } else if (strcmp(mode, "crossexport") == 0) {
    int ends[2];
    assert(pipe(ends) == 0);
    pid_t receiver = fork();
    assert(receiver >= 0);
    if (receiver == 0) {
      assert(close(ends[0]) == 0 && close(ends[1]) == 0);
      int accepted = accept(listener, NULL, NULL);
      assert(accepted >= 0);
      int write_end = receive_descriptor(accepted);
      sleep_ms(30);
      assert(write(write_end, "x", 1) == 1);
      _exit(0);
    }
    pid_t sender = fork();
    assert(sender >= 0);
    if (sender == 0) {
      assert(close(ends[0]) == 0);
      pass_descriptor(connect_to_listener(), ends[1]);
      _exit(0);
    }
    assert(close(ends[1]) == 0);
    // The writer may also have exited by then: report readability only.
    printf("crossexport: readable %d\n", (poll_one(ends[0]) & POLLIN) != 0);
    reap(sender);
    reap(receiver);
  } else if (strcmp(mode, "self-join") == 0) {
    int ends[2];
    assert(pipe(ends) == 0);
    struct pair channel = same_process_pair(0);
    pass_descriptor(channel.connecting.fd, ends[0]);
    int received = receive_descriptor(channel.accepted);
    assert(close(ends[0]) == 0);
    struct peer writer = {ends[1], 0};
    pthread_t thread;
    assert(pthread_create(&thread, NULL, act_later, &writer) == 0);
    printf("self-join: revents %#x\n", poll_one(received));
    assert(pthread_join(thread, NULL) == 0);
  } else if (strcmp(mode, "export-mmsg") == 0) {
    struct pair pair = same_process_pair(0);
    int to_child;
    pid_t child = fork_connected_child(&to_child);
    pass_a_descriptor_mmsg(to_child);
    printf("export-mmsg: revents %#x\n", poll_pair(&pair));
    reap(child);
  } else if (strcmp(mode, "unspec-pair") == 0) {
    int ends[2];
    assert(socketpair(AF_UNIX, SOCK_DGRAM, 0, ends) == 0);
    struct sockaddr unspec;
    memset(&unspec, 0, sizeof(unspec));
    unspec.sa_family = AF_UNSPEC;
    assert(connect(ends[1], &unspec, sizeof(unspec)) == 0);
    struct peer writer = {ends[0], 0};
    pthread_t thread;
    assert(pthread_create(&thread, NULL, act_later, &writer) == 0);
    printf("unspec-pair: revents %#x\n", poll_one(ends[1]));
    assert(pthread_join(thread, NULL) == 0);
  } else if (strcmp(mode, "self-listener") == 0) {
    struct delayed_connect target;
    struct sockaddr_un second_address;
    target.at = &second_address;
    int second = listen_on("hermit-guest-held-polls-3", &second_address, &target.at_length);
    struct pair channel = same_process_pair(0);
    pass_descriptor(channel.connecting.fd, second);
    assert(close(second) == 0);
    int received = receive_descriptor(channel.accepted);
    printf("self-listener: revents %#x\n", poll_listener(received, &target));
  } else if (strcmp(mode, "joined-listener") == 0) {
    struct delayed_connect target;
    struct sockaddr_un second_address;
    target.at = &second_address;
    int second = listen_on("hermit-guest-held-polls-4", &second_address, &target.at_length);
    struct pair channel = same_process_pair(0);
    pass_descriptor(channel.connecting.fd, second);
    int received = receive_descriptor(channel.accepted);
    short first = poll_listener(received, &target);
    short original = poll_listener(second, &target);
    printf("joined-listener: revents %#x %#x\n", first, original);
  } else if (strcmp(mode, "received-connect") == 0) {
    int received = receive_a_childs_socket();
    assert(connect(received, (const struct sockaddr*)&address, address_length) == 0);
    int accepted = accept(listener, NULL, NULL);
    assert(accepted >= 0);
    struct peer writer = {accepted, 0};
    pthread_t thread;
    assert(pthread_create(&thread, NULL, act_later, &writer) == 0);
    short connected = poll_one(received);
    assert(pthread_join(thread, NULL) == 0);
    struct peer other_writer = {received, 0};
    assert(pthread_create(&thread, NULL, act_later, &other_writer) == 0);
    short accepted_end = poll_one(accepted);
    assert(pthread_join(thread, NULL) == 0);
    printf("received-connect: revents %#x %#x\n", connected, accepted_end);
  } else if (strcmp(mode, "received-listener") == 0) {
    int received = receive_a_childs_socket();
    const char* name = "hermit-guest-held-polls-5";
    struct sockaddr_un at;
    memset(&at, 0, sizeof(at));
    at.sun_family = AF_UNIX;
    memcpy(at.sun_path + 1, name, strlen(name));
    socklen_t at_length = offsetof(struct sockaddr_un, sun_path) + 1 + strlen(name);
    assert(bind(received, (struct sockaddr*)&at, at_length) == 0);
    assert(listen(received, 4) == 0);
    int client = connect_to(&at, at_length);
    int accepted = accept(received, NULL, NULL);
    assert(accepted >= 0);
    struct peer writer = {accepted, 0};
    pthread_t thread;
    assert(pthread_create(&thread, NULL, act_later, &writer) == 0);
    short connected = poll_one(client);
    assert(pthread_join(thread, NULL) == 0);
    struct peer other_writer = {client, 0};
    assert(pthread_create(&thread, NULL, act_later, &other_writer) == 0);
    short accepted_end = poll_one(accepted);
    assert(pthread_join(thread, NULL) == 0);
    printf("received-listener: revents %#x %#x\n", connected, accepted_end);
  } else if (strcmp(mode, "forwarded-socket") == 0) {
    int pair[2];
    assert(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0);
    pid_t child = fork();
    assert(child >= 0);
    if (child == 0) {
      assert(close(pair[0]) == 0);
      int client = connect_to_listener();
      pass_descriptor(pair[1], client);
      assert(close(client) == 0);
      _exit(0);
    }
    assert(close(pair[1]) == 0);
    int accepted = accept(listener, NULL, NULL);
    assert(accepted >= 0);
    reap(child);
    int received = receive_descriptor(pair[0]);
    struct peer writer = {accepted, 0};
    pthread_t thread;
    assert(pthread_create(&thread, NULL, act_later, &writer) == 0);
    printf("forwarded-socket: revents %#x\n", poll_one(received));
    assert(pthread_join(thread, NULL) == 0);
  } else {
    fprintf(stderr, "unknown mode %s\n", mode);
    return 2;
  }
  return 0;
}
