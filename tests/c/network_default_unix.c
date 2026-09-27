/* SPDX-License-Identifier: BSD-3-Clause */
#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

static void fail(const char *what) { perror(what); exit(2); }
static struct sockaddr_un address(const char *path) {
  struct sockaddr_un out = { .sun_family = AF_UNIX };
  if (strlen(path) >= sizeof(out.sun_path)) { errno = ENAMETOOLONG; fail("path"); }
  memcpy(out.sun_path, path, strlen(path) + 1);
  return out;
}
static int listener(const char *path) {
  int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
  if (fd < 0) fail("socket");
  struct sockaddr_un addr = address(path);
  if (bind(fd, (struct sockaddr *)&addr, sizeof(addr))) fail("bind");
  if (listen(fd, 1)) fail("listen");
  return fd;
}
static void exchange(int fd, int client) {
  char byte = 0;
  if (client && send(fd, "Q", 1, MSG_NOSIGNAL) != 1) fail("send request");
  if (recv(fd, &byte, 1, MSG_WAITALL) != 1) fail("receive");
  if (byte != (client ? 'A' : 'Q')) { errno = EPROTO; fail("payload"); }
  if (!client && send(fd, "A", 1, MSG_NOSIGNAL) != 1) fail("send response");
}
static int connect_client(const char *path) {
  int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
  if (fd < 0) fail("client socket");
  struct sockaddr_un addr = address(path);
  if (connect(fd, (struct sockaddr *)&addr, sizeof(addr))) {
    int saved = errno;
    if (close(fd)) fail("client failed close");
    printf("connect=refused errno=%d\n", saved);
    return 3;
  }
  exchange(fd, 1);
  if (close(fd)) fail("client close");
  puts("connect=success request=Q response=A");
  return 0;
}
int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  if (argc == 2 && !strcmp(argv[1], "pair")) {
    int fd[2]; char byte = 0;
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, fd)) fail("socketpair");
    if (send(fd[0], "P", 1, MSG_NOSIGNAL) != 1 || recv(fd[1], &byte, 1, 0) != 1 || byte != 'P') fail("pair payload");
    if (close(fd[0]) || close(fd[1])) fail("pair close");
    puts("pair=success payload=P");
    return 0;
  }
  if (argc == 3 && !strcmp(argv[1], "client")) return connect_client(argv[2]);
  if (argc == 3 && !strcmp(argv[1], "internal")) {
    int fd = listener(argv[2]);
    pid_t child = fork();
    if (child < 0) fail("fork");
    if (child == 0) { if (close(fd)) fail("child listener close"); _exit(connect_client(argv[2])); }
    int accepted = accept4(fd, NULL, NULL, SOCK_CLOEXEC);
    if (accepted < 0) fail("internal accept");
    exchange(accepted, 0);
    if (close(accepted) || close(fd)) fail("internal close");
    int status = 0;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status)) { errno = ECHILD; fail("child status"); }
    if (unlink(argv[2])) fail("internal unlink");
    puts("internal=success child_reaped=1");
    return 0;
  }
  if (argc == 4 && !strcmp(argv[1], "controller")) {
    int fd = listener(argv[2]);
    puts("controller=ready");
    struct pollfd p = {.fd=fd, .events=POLLIN};
    int ready = poll(&p, 1, 10000);
    if (ready < 0) fail("controller poll");
    if (ready == 0) { if (close(fd) || unlink(argv[2])) fail("controller timeout cleanup"); return 4; }
    int accepted = accept4(fd, NULL, NULL, SOCK_CLOEXEC);
    if (accepted < 0) fail("controller accept");
    FILE *report = fopen(argv[3], "wx");
    if (!report) fail("controller report");
    if (fputs("external_contact=1\n", report) < 0 || fclose(report)) fail("controller report write");
    exchange(accepted, 0);
    if (close(accepted) || close(fd) || unlink(argv[2])) fail("controller close");
    puts("controller=complete");
    return 0;
  }
  fprintf(stderr, "usage: probe pair | client PATH | internal PATH | controller PATH REPORT\n");
  return 2;
}
