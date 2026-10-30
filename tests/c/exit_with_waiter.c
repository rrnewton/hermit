// A process exits while one of its threads is still blocked in a call, so the
// call never returns: the thread dies inside it. Linux ends the waiter when
// main returns (exit_group). A recorder writes a call's events when the call
// returns, so a recording holds nothing for that last call, and replay has to
// leave the thread blocked until the exit, as the recording did.
//
// The mode names the call the waiter blocks in, with no timeout, on an empty
// pipe unless said otherwise: "poll", "ppoll", "select", "pselect" (pselect6),
// "epoll" (epoll_wait), "epoll_pwait", "recv" (a socket pair), "read" or
// "accept" (a listening Unix socket). The waiter writes to a pipe just before
// its call; main reads that, sleeps 50 ms and returns. Main prints a different
// line if the call returned, so a call that fails at once does not pass.
//
// "busy FILE" starts no thread: main rewrites FILE and fsyncs it in a loop, in
// calls a recording logs, until something outside the guest kills it.
//
// "both FILE" starts the waiter as "poll" does, then writes "ready" to FILE and
// blocks in the same poll itself, so that every thread waits in a call that
// never returns, until something outside the guest kills it.
//
// "segv" and "abort" start the waiter as "poll" does, then main crashes: a
// store to address 0 (SIGSEGV) or abort(3) (SIGABRT). Linux ends the waiter
// inside its poll with the same signal, and the process dies of it.
#define _GNU_SOURCE
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

static const char *mode;
static int fds[2], waiting[2], listener = -1;
static atomic_int returned;

static void *waiter(void *arg) {
  (void)arg;
  int epfd = -1;
  if (strncmp(mode, "epoll", 5) == 0) {
    epfd = epoll_create1(0);
    struct epoll_event ev = {.events = EPOLLIN};
    if (epfd < 0 || epoll_ctl(epfd, EPOLL_CTL_ADD, fds[0], &ev) != 0) {
      return NULL;
    }
  }
  if (write(waiting[1], "x", 1) != 1) {
    return NULL;
  }
  struct pollfd pfd = {fds[0], POLLIN, 0};
  fd_set set;
  FD_ZERO(&set);
  FD_SET(fds[0], &set);
  if (strcmp(mode, "poll") == 0) {
    poll(&pfd, 1, -1);
  } else if (strcmp(mode, "ppoll") == 0) {
    ppoll(&pfd, 1, NULL, NULL);
  } else if (strcmp(mode, "select") == 0) {
    select(fds[0] + 1, &set, NULL, NULL, NULL);
  } else if (strcmp(mode, "pselect") == 0) {
    pselect(fds[0] + 1, &set, NULL, NULL, NULL, NULL);
  } else if (epfd >= 0 && strcmp(mode, "epoll") == 0) {
    struct epoll_event ev;
    epoll_wait(epfd, &ev, 1, -1);
  } else if (epfd >= 0) {
    struct epoll_event ev;
    epoll_pwait(epfd, &ev, 1, -1, NULL);
  } else if (strcmp(mode, "recv") == 0) {
    char c;
    if (recv(fds[0], &c, 1, 0) < 0) {
      perror("recv");
    }
  } else if (strcmp(mode, "read") == 0) {
    char c;
    if (read(fds[0], &c, 1) < 0) {
      perror("read");
    }
  } else {
    accept(listener, NULL, NULL);
  }
  atomic_store(&returned, 1);
  return NULL;
}

int main(int argc, char **argv) {
  mode = argc > 1 ? argv[1] : "poll";
  if (strcmp(mode, "busy") == 0 && argc > 2) {
    int fd = open(argv[2], O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) {
      perror("open");
      return 2;
    }
    for (long i = 0; i < 1000000; i++) {
      if (pwrite(fd, "x", 1, 0) != 1 || fsync(fd) != 0) {
        perror("busy");
        return 2;
      }
    }
    return 0;
  }
  const char *ready = NULL;
  const char *crash = NULL;
  if (strcmp(mode, "both") == 0 && argc > 2) {
    ready = argv[2];
    mode = "poll";
  } else if (strcmp(mode, "segv") == 0 || strcmp(mode, "abort") == 0) {
    crash = mode;
    mode = "poll";
  }
  // "recv" waits on a socket pair, the others on a pipe.
  int made = strcmp(mode, "recv") == 0
                 ? socketpair(AF_UNIX, SOCK_STREAM, 0, fds)
                 : pipe(fds);
  if (made != 0 || pipe(waiting) != 0) {
    perror("pipe");
    return 2;
  }
  if (strcmp(mode, "accept") == 0) {
    // An abstract socket: no file to clean up.
    struct sockaddr_un addr = {.sun_family = AF_UNIX};
    snprintf(addr.sun_path + 1, sizeof addr.sun_path - 1, "exit-with-waiter-%d",
             (int)getpid());
    listener = socket(AF_UNIX, SOCK_STREAM, 0);
    if (listener < 0 ||
        bind(listener, (struct sockaddr *)&addr, sizeof addr) != 0 ||
        listen(listener, 1) != 0) {
      perror("listener");
      return 2;
    }
  }
  pthread_t thread;
  char c;
  if (pthread_create(&thread, NULL, waiter, NULL) != 0 ||
      read(waiting[0], &c, 1) != 1) {
    perror("waiter");
    return 2;
  }
  if (ready != NULL) {
    int fd = open(ready, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0 || write(fd, "ready", 5) != 5 || close(fd) != 0) {
      perror("ready");
      return 2;
    }
    struct pollfd pfd = {fds[0], POLLIN, 0};
    poll(&pfd, 1, -1);
    printf("both: main's poll returned\n");
    return 1;
  }
  if (crash != NULL) {
    printf("%s: crashing with a thread still waiting\n", crash);
    fflush(stdout);
    if (strcmp(crash, "abort") == 0) {
      abort();
    }
    *(volatile int *)0 = 1;
    printf("%s: the store to address 0 returned\n", crash);
    return 1;
  }
  struct timespec delay = {0, 50 * 1000 * 1000};
  syscall(SYS_nanosleep, &delay, NULL);
  if (atomic_load(&returned)) {
    printf("%s: the waiter's call returned\n", mode);
  } else {
    printf("%s: exiting with a thread still waiting\n", mode);
  }
  fflush(stdout);
  return 0;
}
