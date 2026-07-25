#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/epoll.h>
#include <sys/syscall.h>
#include <unistd.h>

static int epoll_fd;
static int ready_pipe[2];

static void fail(const char *message) {
  perror(message);
  exit(1);
}

static void *wait_for_event(void *unused) {
  (void)unused;
  const pid_t tid = syscall(SYS_gettid);
  if (write(ready_pipe[1], &tid, sizeof(tid)) != sizeof(tid))
    fail("write waiter tid");

  struct epoll_event event;
  const int result = epoll_wait(epoll_fd, &event, 1, -1);
  fprintf(stderr, "epoll_wait unexpectedly returned %d (errno=%d)\n", result,
          errno);
  _exit(2);
}

static void wait_until_epoll_blocked(pid_t tid) {
  char path[64];
  if (snprintf(path, sizeof(path), "/proc/self/task/%d/syscall", tid) < 0)
    fail("format waiter syscall path");

  for (int attempt = 0; attempt < 1000; ++attempt) {
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0)
      fail("open waiter syscall");

    char syscall_state[128];
    const ssize_t size = read(fd, syscall_state, sizeof(syscall_state) - 1);
    if (close(fd) != 0)
      fail("close waiter syscall");
    if (size < 0)
      fail("read waiter syscall");
    syscall_state[size] = '\0';

    char *end;
    const long syscall_number = strtol(syscall_state, &end, 10);
    if (end != syscall_state && syscall_number == SYS_epoll_wait)
      return;
    if (sched_yield() != 0)
      fail("sched_yield");
  }

  fputs("waiter did not block in epoll_wait\n", stderr);
  exit(3);
}

int main(void) {
  epoll_fd = epoll_create1(EPOLL_CLOEXEC);
  if (epoll_fd < 0)
    fail("epoll_create1");
  if (pipe2(ready_pipe, O_CLOEXEC) != 0)
    fail("pipe2");

  pthread_t waiter;
  if (pthread_create(&waiter, NULL, wait_for_event, NULL) != 0)
    fail("pthread_create");

  pid_t waiter_tid;
  if (read(ready_pipe[0], &waiter_tid, sizeof(waiter_tid)) !=
      sizeof(waiter_tid))
    fail("read waiter tid");
  wait_until_epoll_blocked(waiter_tid);

  execl("/bin/echo", "echo", "epoll-exec-ok", (char *)NULL);
  fail("execl");
}
