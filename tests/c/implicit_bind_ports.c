// Prints the local ports Linux picks for sockets the guest never bound (an
// implicit bind) and, beside them, the ports their peers see.
//
// Linux binds a TCP or UDP socket that has no local port the first time it
// needs one: TCP at connect(2) and listen(2), UDP at connect(2) and at a send.
// Natively the pick is random, so the numbers differ from run to run; under
// Hermit, Detcore takes them from the allocator a bind to port 0 uses, so a
// run prints the same numbers every time.
//
// Modes:
//   wide    The default range. TCP and UDP over IPv4 and, when the host has
//           IPv6 loopback, IPv6: a client connect(2) to a listener bound to
//           port 0, an unbound UDP sendto(2), sendmsg(2), sendmmsg(2) and
//           connect(2), and a listen(2) without a bind. Each port is printed,
//           then checks that hold on Linux whatever it picks (the client's
//           port is the one accept(2) reports, a datagram's source is the
//           sender's port, ports held at once by one protocol differ).
//   forced  Narrows /proc/sys/net/ipv4/ip_local_port_range (the guest must be
//           able to write it: root in the network namespace's user namespace,
//           and under Hermit `--proc=standard`) so that each pick has one
//           answer, and prints them with the errno of each call that finds
//           no free port. A port a closed connection leaves in TIME_WAIT is
//           skipped, a socket's IP_LOCAL_PORT_RANGE narrows its pick, and a
//           call Linux fails before it picks takes no port. The output is
//           Linux's, so it can be compared byte for byte with a native run
//           (`unshare -Urn`, loopback up) that ends within the one second
//           after which Linux may reuse a TIME_WAIT port for the same peer.
#define _GNU_SOURCE // sendmmsg
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static int failures = 0;

static void check(int ok, const char* what) {
  if (!ok) {
    failures++;
    printf("check failed: %s\n", what);
  }
}

// "127.0.0.1:32768" or "[::1]:32768".
static const char* show(const struct sockaddr_storage* address) {
  static char text[2][INET6_ADDRSTRLEN + 16];
  static int next = 0;
  char* out = text[next++ % 2];
  char host[INET6_ADDRSTRLEN];
  if (address->ss_family == AF_INET) {
    const struct sockaddr_in* in = (const struct sockaddr_in*)address;
    inet_ntop(AF_INET, &in->sin_addr, host, sizeof host);
    snprintf(out, sizeof text[0], "%s:%d", host, ntohs(in->sin_port));
  } else if (address->ss_family == AF_INET6) {
    const struct sockaddr_in6* in6 = (const struct sockaddr_in6*)address;
    inet_ntop(AF_INET6, &in6->sin6_addr, host, sizeof host);
    snprintf(out, sizeof text[0], "[%s]:%d", host, ntohs(in6->sin6_port));
  } else {
    snprintf(out, sizeof text[0], "family-%d", address->ss_family);
  }
  return out;
}

static int port_of(const struct sockaddr_storage* address) {
  return address->ss_family == AF_INET6
      ? ntohs(((const struct sockaddr_in6*)address)->sin6_port)
      : ntohs(((const struct sockaddr_in*)address)->sin_port);
}

static struct sockaddr_storage local_of(int fd) {
  struct sockaddr_storage address;
  memset(&address, 0, sizeof address);
  socklen_t length = sizeof address;
  if (getsockname(fd, (struct sockaddr*)&address, &length) != 0) {
    address.ss_family = AF_UNSPEC;
  }
  return address;
}

static struct sockaddr_storage peer_of(int fd) {
  struct sockaddr_storage address;
  memset(&address, 0, sizeof address);
  socklen_t length = sizeof address;
  if (getpeername(fd, (struct sockaddr*)&address, &length) != 0) {
    address.ss_family = AF_UNSPEC;
  }
  return address;
}

static int local_port(int fd) {
  struct sockaddr_storage address = local_of(fd);
  return port_of(&address);
}

static socklen_t length_of(int family) {
  return family == AF_INET6 ? sizeof(struct sockaddr_in6)
                            : sizeof(struct sockaddr_in);
}

// The loopback address of `family` with port 0.
static struct sockaddr_storage loopback(int family) {
  struct sockaddr_storage address;
  memset(&address, 0, sizeof address);
  if (family == AF_INET6) {
    struct sockaddr_in6* in6 = (struct sockaddr_in6*)&address;
    in6->sin6_family = AF_INET6;
    in6->sin6_addr = in6addr_loopback;
  } else {
    struct sockaddr_in* in = (struct sockaddr_in*)&address;
    in->sin_family = AF_INET;
    in->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  }
  return address;
}

static const char* errno_name(int error) {
  switch (error) {
    case 0:
      return "0";
    case EAGAIN:
      return "EAGAIN";
    case EADDRINUSE:
      return "EADDRINUSE";
    case EADDRNOTAVAIL:
      return "EADDRNOTAVAIL";
    case EAFNOSUPPORT:
      return "EAFNOSUPPORT";
    default: {
      static char text[16];
      snprintf(text, sizeof text, "%d", error);
      return text;
    }
  }
}

// A listener bound to port 0 on `family`'s loopback address.
static int listener(int family) {
  int fd = socket(family, SOCK_STREAM, 0);
  struct sockaddr_storage address = loopback(family);
  if (fd < 0 || bind(fd, (struct sockaddr*)&address, length_of(family)) != 0 ||
      listen(fd, 8) != 0) {
    return -1;
  }
  return fd;
}

// Ports one protocol holds at once, which Linux keeps distinct.
struct held {
  int ports[16];
  int count;
};

static void hold(struct held* held, int port, const char* what) {
  check(port != 0, what);
  for (int i = 0; i < held->count; i++) {
    check(held->ports[i] != port, what);
  }
  held->ports[held->count++] = port;
}

// TCP on `family`: a listener, a client's implicit bind at connect, and the
// ports accept reports. Returns 0, or -1 when the family is unavailable.
static int tcp_round(int family, const char* name, struct held* tcp) {
  int server = listener(family);
  if (server < 0) {
    printf("%s unavailable\n", name);
    return -1;
  }
  struct sockaddr_storage served = local_of(server);
  printf("%s listener %s\n", name, show(&served));
  hold(tcp, port_of(&served), "listener port");

  int client = socket(family, SOCK_STREAM, 0);
  int rc = connect(client, (struct sockaddr*)&served, length_of(family));
  struct sockaddr_storage client_local = local_of(client);
  struct sockaddr_storage client_peer = peer_of(client);
  printf(
      "%s client connect=%d local %s peer %s\n",
      name,
      rc,
      show(&client_local),
      show(&client_peer));
  hold(tcp, port_of(&client_local), "client port");
  check(port_of(&client_peer) == port_of(&served), "client peer is listener");

  struct sockaddr_storage accepted_peer;
  memset(&accepted_peer, 0, sizeof accepted_peer);
  socklen_t length = sizeof accepted_peer;
  int accepted = accept(server, (struct sockaddr*)&accepted_peer, &length);
  struct sockaddr_storage accepted_local = local_of(accepted);
  printf(
      "%s accepted local %s peer %s\n",
      name,
      show(&accepted_local),
      show(&accepted_peer));
  check(port_of(&accepted_local) == port_of(&served), "accepted on listener");
  check(
      port_of(&accepted_peer) == port_of(&client_local),
      "accept reports the client's port");
  return 0;
}

// UDP on `family`: an unbound sender's implicit bind at sendto, seen as the
// datagram's source, and an unbound socket's at connect.
static void udp_round(int family, const char* name, struct held* udp) {
  int receiver = socket(family, SOCK_DGRAM, 0);
  struct sockaddr_storage address = loopback(family);
  bind(receiver, (struct sockaddr*)&address, length_of(family));
  struct sockaddr_storage received_on = local_of(receiver);
  printf("%s receiver %s\n", name, show(&received_on));
  hold(udp, port_of(&received_on), "receiver port");

  int sender = socket(family, SOCK_DGRAM, 0);
  int sent = (int)sendto(
      sender, "x", 1, 0, (struct sockaddr*)&received_on, length_of(family));
  struct sockaddr_storage sender_local = local_of(sender);
  struct sockaddr_storage source;
  memset(&source, 0, sizeof source);
  socklen_t length = sizeof source;
  char byte;
  recvfrom(receiver, &byte, 1, 0, (struct sockaddr*)&source, &length);
  printf(
      "%s sender sendto=%d local %s datagram-from %s\n",
      name,
      sent,
      show(&sender_local),
      show(&source));
  hold(udp, port_of(&sender_local), "sender port");
  check(port_of(&source) == port_of(&sender_local), "datagram source port");

  int connected = socket(family, SOCK_DGRAM, 0);
  int rc = connect(
      connected, (struct sockaddr*)&received_on, length_of(family));
  struct sockaddr_storage connected_local = local_of(connected);
  printf(
      "%s connected connect=%d local %s\n", name, rc, show(&connected_local));
  hold(udp, port_of(&connected_local), "connected port");

  // sendmsg(2) and sendmmsg(2) bind an unbound socket as sendto(2) does.
  for (int batched = 0; batched < 2; batched++) {
    int fd = socket(family, SOCK_DGRAM, 0);
    struct iovec payload = {.iov_base = "y", .iov_len = 1};
    struct mmsghdr message;
    memset(&message, 0, sizeof message);
    message.msg_hdr.msg_name = &received_on;
    message.msg_hdr.msg_namelen = length_of(family);
    message.msg_hdr.msg_iov = &payload;
    message.msg_hdr.msg_iovlen = 1;
    int count = batched ? sendmmsg(fd, &message, 1, 0)
                        : (int)sendmsg(fd, &message.msg_hdr, 0);
    struct sockaddr_storage local = local_of(fd);
    memset(&source, 0, sizeof source);
    length = sizeof source;
    recvfrom(receiver, &byte, 1, 0, (struct sockaddr*)&source, &length);
    printf(
        "%s %s sender sent=%d local %s datagram-from %s\n",
        name,
        batched ? "sendmmsg" : "sendmsg",
        count,
        show(&local),
        show(&source));
    hold(udp, port_of(&local), "sendmsg port");
    check(port_of(&source) == port_of(&local), "sendmsg datagram source port");
  }
}

static int wide(void) {
  struct held tcp = {.count = 0};
  struct held udp = {.count = 0};
  tcp_round(AF_INET, "tcp4", &tcp);
  udp_round(AF_INET, "udp4", &udp);

  int unbound = socket(AF_INET, SOCK_STREAM, 0);
  int rc = listen(unbound, 1);
  struct sockaddr_storage listening = local_of(unbound);
  printf("tcp4 listen-without-bind listen=%d local %s\n", rc, show(&listening));
  hold(&tcp, port_of(&listening), "listen-without-bind port");

  if (tcp_round(AF_INET6, "tcp6", &tcp) == 0) {
    udp_round(AF_INET6, "udp6", &udp);
  }
  printf("checks failed=%d\n", failures);
  return failures == 0 ? 0 : 1;
}

static int set_range(int low, int high) {
  FILE* file = fopen("/proc/sys/net/ipv4/ip_local_port_range", "w");
  if (file == NULL) {
    printf("ip_local_port_range unwritable errno=%d\n", errno);
    return -1;
  }
  fprintf(file, "%d %d\n", low, high);
  if (fclose(file) != 0) {
    printf("ip_local_port_range %d %d refused errno=%d\n", low, high, errno);
    return -1;
  }
  printf("range %d %d\n", low, high);
  return 0;
}

// connect a new TCP client to `served`; prints and returns the client.
static int tcp_client(const struct sockaddr_storage* served, const char* what) {
  int client = socket(AF_INET, SOCK_STREAM, 0);
  errno = 0;
  int rc = connect(client, (const struct sockaddr*)served, length_of(AF_INET));
  printf(
      "  %s connect=%d errno=%s port %d\n",
      what,
      rc,
      errno_name(rc ? errno : 0),
      local_port(client));
  return client;
}

static void udp_send(const struct sockaddr_storage* to, const char* what) {
  int sender = socket(AF_INET, SOCK_DGRAM, 0);
  errno = 0;
  int sent =
      (int)sendto(sender, "x", 1, 0, (const struct sockaddr*)to, sizeof(struct sockaddr_in));
  printf(
      "  %s sendto=%d errno=%s port %d\n",
      what,
      sent,
      errno_name(sent < 0 ? errno : 0),
      local_port(sender));
}

static int listen_unbound(const char* what) {
  int fd = socket(AF_INET, SOCK_STREAM, 0);
  errno = 0;
  int rc = listen(fd, 1);
  printf(
      "  %s listen=%d errno=%s port %d\n",
      what,
      rc,
      errno_name(rc ? errno : 0),
      local_port(fd));
  return fd;
}

static int forced(void) {
  if (set_range(40000, 40000) != 0) {
    return 2;
  }
  int server = listener(AF_INET);
  struct sockaddr_storage served = local_of(server);
  printf("  listener bound to port 0: port %d\n", port_of(&served));

  set_range(40001, 40001);
  int client = tcp_client(&served, "client");
  struct sockaddr_storage peer;
  socklen_t length = sizeof peer;
  int accepted = accept(server, (struct sockaddr*)&peer, &length);
  printf("  accept reports port %d\n", port_of(&peer));
  tcp_client(&served, "second client");

  set_range(40002, 40002);
  int receiver = socket(AF_INET, SOCK_DGRAM, 0);
  struct sockaddr_storage address = loopback(AF_INET);
  ((struct sockaddr_in*)&address)->sin_port = htons(port_of(&served));
  udp_send(&address, "udp sender");
  udp_send(&address, "second udp sender");
  int connected = socket(AF_INET, SOCK_DGRAM, 0);
  errno = 0;
  int rc = connect(connected, (struct sockaddr*)&address, length_of(AF_INET));
  printf(
      "  udp connect=%d errno=%s port %d\n",
      rc,
      errno_name(rc ? errno : 0),
      local_port(connected));

  set_range(40003, 40003);
  listen_unbound("listen without bind");
  listen_unbound("second listen without bind");

  // A connection the client closes first leaves its port in TIME_WAIT,
  // which a later connect to the same peer skips.
  set_range(40004, 40005);
  int first = tcp_client(&served, "client closing first");
  int first_accepted = accept(server, NULL, NULL);
  close(first);
  close(first_accepted);
  int filler = listen_unbound("listen without bind");
  close(filler);
  int second = tcp_client(&served, "client after TIME_WAIT");
  int second_accepted = accept(server, NULL, NULL);
  close(second);
  close(second_accepted);
  tcp_client(&served, "client with both ports in TIME_WAIT");

  // A socket's own IP_LOCAL_PORT_RANGE narrows the namespace's range.
  set_range(40006, 40008);
  int ranged = socket(AF_INET, SOCK_STREAM, 0);
  uint32_t own_range = 40007u | 40007u << 16;
  int set = setsockopt(ranged, IPPROTO_IP, 51 /* IP_LOCAL_PORT_RANGE */, &own_range, sizeof own_range);
  rc = connect(ranged, (struct sockaddr*)&served, length_of(AF_INET));
  printf(
      "  client with IP_LOCAL_PORT_RANGE 40007-40007 setsockopt=%d connect=%d port %d\n",
      set,
      rc,
      local_port(ranged));

  // Calls Linux fails or finishes before it picks a port take none.
  set_range(40009, 40009);
  struct sockaddr_in6 wrong_family;
  memset(&wrong_family, 0, sizeof wrong_family);
  wrong_family.sin6_family = AF_INET6;
  wrong_family.sin6_addr = in6addr_loopback;
  wrong_family.sin6_port = htons(port_of(&served));
  int ipv4 = socket(AF_INET, SOCK_STREAM, 0);
  errno = 0;
  rc = connect(ipv4, (struct sockaddr*)&wrong_family, sizeof wrong_family);
  printf(
      "  tcp connect to an IPv6 address from an IPv4 socket connect=%d errno=%s port %d\n",
      rc,
      errno_name(rc ? errno : 0),
      local_port(ipv4));
  struct sockaddr unspecified = {.sa_family = AF_UNSPEC};
  int disconnected = socket(AF_INET, SOCK_DGRAM, 0);
  errno = 0;
  rc = connect(disconnected, &unspecified, sizeof unspecified);
  printf(
      "  udp disconnect without a port connect=%d errno=%s port %d\n",
      rc,
      errno_name(rc ? errno : 0),
      local_port(disconnected));
  for (int round = 0; round < 3; round++) {
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    struct iovec payload = {.iov_base = "z", .iov_len = 1};
    struct mmsghdr message;
    memset(&message, 0, sizeof message);
    message.msg_hdr.msg_name = &address;
    message.msg_hdr.msg_namelen = sizeof(struct sockaddr_in);
    message.msg_hdr.msg_iov = &payload;
    message.msg_hdr.msg_iovlen = 1;
    errno = 0;
    int sent = round == 2 ? sendmmsg(fd, &message, 1, 0)
                          : (int)sendmsg(fd, &message.msg_hdr, 0);
    printf(
        "  udp %s sender sent=%d errno=%s port %d\n",
        round == 2 ? "sendmmsg" : round == 1 ? "second sendmsg" : "sendmsg",
        sent,
        errno_name(sent < 0 ? errno : 0),
        local_port(fd));
  }

  close(client);
  close(accepted);
  close(receiver);
  return 0;
}

int main(int argc, char** argv) {
  setvbuf(stdout, NULL, _IOLBF, 0);
  const char* mode = argc > 1 ? argv[1] : "wide";
  if (strcmp(mode, "wide") == 0) {
    return wide();
  }
  if (strcmp(mode, "forced") == 0) {
    return forced();
  }
  fprintf(stderr, "usage: %s [wide|forced]\n", argv[0]);
  return 2;
}
