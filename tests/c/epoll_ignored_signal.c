// epoll_wait returns EINTR only for a signal that is delivered: Linux
// discards a signal the process ignores when it is generated, so it never
// ends the wait (sig_ignored, kernel/signal.c). Under a tracer the kernel
// queues even an ignored signal, and ep_poll then returns EINTR, which no
// restart hides.
//
// Each wait below is, in turn, an epoll_wait, an epoll_pwait with no mask and
// an epoll_pwait2 with no mask.
//
// "chld": five 100 ms waits on an empty pipe, each with a child that exits at
// once (SIGCHLD, ignored by default), then an epoll_wait of up to a second
// that a child's write ends after three more children exit.
// "storm": a child sends 40 SIGWINCH, ignored by default, 20 ms apart while
// the parent makes three 100 ms waits: each returns 0 long before the storm
// ends.
// "sigign": as "chld", with SIGCHLD set to SIG_IGN, so children are reaped by
// the kernel.
// "alrm": a 30 ms ITIMER_REAL with a SIGALRM handler interrupts each of five
// 100 ms epoll_waits: EINTR.
// "exit": a thread waits with no timeout while main returns, so the process
// exits with the thread inside its wait.
//
// Every mode blocks SIGUSR2 first, and reports whether the thread's mask is
// still exactly that after the waits (and, in "alrm", inside the handler).
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/select.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef SYS_epoll_pwait2
#define SYS_epoll_pwait2 441
#endif

static volatile sig_atomic_t alarms;
static volatile sig_atomic_t handler_mask_changed;

// Whether the thread's mask is as main set it: SIGUSR2 blocked, and neither
// ignored signal.
static int mask_as_before(void) {
  sigset_t now;
  sigprocmask(SIG_BLOCK, NULL, &now);
  return sigismember(&now, SIGUSR2) && !sigismember(&now, SIGCHLD) &&
         !sigismember(&now, SIGWINCH);
}

static void on_alarm(int sig) {
  (void)sig;
  alarms++;
  if (!mask_as_before()) {
    handler_mask_changed = 1;
  }
}

static int reap(void) {
  int children = 0;
  int status;
  while (wait(&status) > 0) {
    if (WIFEXITED(status) && WEXITSTATUS(status) == 0) {
      children++;
    }
  }
  return children;
}

struct tally {
  int zero, ready, eintr, other;
};

static void count(struct tally *t, int r) {
  if (r == 0) {
    t->zero++;
  } else if (r > 0) {
    t->ready++;
  } else if (errno == EINTR) {
    t->eintr++;
  } else {
    t->other++;
  }
}

// The i-th 100 ms wait: epoll_wait, epoll_pwait or epoll_pwait2, no mask.
static int wait_100ms(int epfd, int i) {
  struct epoll_event ev;
  struct timespec timeout = {0, 100 * 1000 * 1000};
  switch (i % 3) {
  case 0:
    return epoll_wait(epfd, &ev, 1, 100);
  case 1:
    return epoll_pwait(epfd, &ev, 1, 100, NULL);
  default:
    return (int)syscall(SYS_epoll_pwait2, epfd, &ev, 1, &timeout, NULL, 0);
  }
}

static void print_tally(const char *what, const struct tally *t) {
  printf("%s: %d timed out, %d ready, %d EINTR, %d other", what, t->zero,
         t->ready, t->eintr, t->other);
}

static int children_mode(int epfd, int write_fd, const char *name) {
  struct tally t = {0};
  struct epoll_event ev;
  for (int i = 0; i < 5; i++) {
    if (fork() == 0) {
      _exit(0);
    }
    count(&t, wait_100ms(epfd, i));
  }
  for (int i = 0; i < 3; i++) {
    if (fork() == 0) {
      _exit(0);
    }
  }
  if (fork() == 0) {
    struct timespec delay = {0, 30 * 1000 * 1000};
    syscall(SYS_nanosleep, &delay, NULL);
    _exit(write(write_fd, "x", 1) == 1 ? 0 : 1);
  }
  int late = epoll_wait(epfd, &ev, 1, 1000);
  int late_errno = late < 0 ? errno : 0;
  print_tally(name, &t);
  printf("; late data: %d%s; mask as before: %s; %d children\n", late,
         late_errno == EINTR ? " (EINTR)" : "", mask_as_before() ? "yes" : "no",
         reap());
  return 0;
}

static int storm_mode(int epfd) {
  int done[2];
  if (pipe(done) != 0) {
    perror("pipe");
    return 2;
  }
  pid_t parent = getpid();
  if (fork() == 0) {
    struct timespec gap = {0, 20 * 1000 * 1000};
    for (int i = 0; i < 40; i++) {
      syscall(SYS_nanosleep, &gap, NULL);
      kill(parent, SIGWINCH);
    }
    _exit(write(done[1], "x", 1) == 1 ? 0 : 1);
  }
  struct tally t = {0};
  for (int i = 0; i < 3; i++) {
    count(&t, wait_100ms(epfd, i));
  }
  fd_set set;
  FD_ZERO(&set);
  FD_SET(done[0], &set);
  struct timeval tv = {0, 0};
  int over = select(done[0] + 1, &set, NULL, NULL, &tv);
  print_tally("storm", &t);
  printf(
      "; mask as before: %s; the storm was still running when they returned: "
      "%s; %d children\n",
      mask_as_before() ? "yes" : "no", over == 0 ? "yes" : "no", reap());
  return 0;
}

static int exit_epfd;

static void *wait_forever(void *arg) {
  (void)arg;
  struct epoll_event ev;
  epoll_wait(exit_epfd, &ev, 1, -1);
  return NULL;
}

static int exit_mode(int epfd) {
  exit_epfd = epfd;
  pthread_t waiter;
  if (pthread_create(&waiter, NULL, wait_forever, NULL) != 0) {
    perror("pthread_create");
    return 2;
  }
  struct timespec delay = {0, 50 * 1000 * 1000};
  syscall(SYS_nanosleep, &delay, NULL);
  printf("exit: a thread was still waiting\n");
  fflush(stdout);
  return 0;
}

static int alarm_mode(int epfd) {
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_handler = on_alarm;
  sigaction(SIGALRM, &sa, NULL);
  struct tally t = {0};
  struct epoll_event ev;
  for (int i = 0; i < 5; i++) {
    struct itimerval once = {{0, 0}, {0, 30 * 1000}};
    setitimer(ITIMER_REAL, &once, NULL);
    count(&t, epoll_wait(epfd, &ev, 1, 100));
  }
  print_tally("alrm", &t);
  printf(", %d alarms; mask as before in the handler: %s, after: %s\n",
         (int)alarms, handler_mask_changed ? "no" : "yes",
         mask_as_before() ? "yes" : "no");
  return 0;
}

int main(int argc, char **argv) {
  const char *mode = argc > 1 ? argv[1] : "chld";
  // A signal the guest blocks itself, which every wait must leave blocked.
  sigset_t usr2;
  sigemptyset(&usr2);
  sigaddset(&usr2, SIGUSR2);
  sigprocmask(SIG_BLOCK, &usr2, NULL);
  int pipe_fds[2];
  if (pipe(pipe_fds) != 0) {
    perror("pipe");
    return 2;
  }
  int epfd = epoll_create1(0);
  struct epoll_event ev = {.events = EPOLLIN};
  if (epfd < 0 || epoll_ctl(epfd, EPOLL_CTL_ADD, pipe_fds[0], &ev) != 0) {
    perror("epoll");
    return 2;
  }
  if (strcmp(mode, "storm") == 0) {
    return storm_mode(epfd);
  }
  if (strcmp(mode, "alrm") == 0) {
    return alarm_mode(epfd);
  }
  if (strcmp(mode, "exit") == 0) {
    return exit_mode(epfd);
  }
  if (strcmp(mode, "sigign") == 0) {
    signal(SIGCHLD, SIG_IGN);
  }
  return children_mode(epfd, pipe_fds[1], mode);
}
