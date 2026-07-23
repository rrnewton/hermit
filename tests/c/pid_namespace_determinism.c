/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

enum { CHILDREN = 4 };

static volatile sig_atomic_t signals_delivered;

static void handle_signal(int signal) {
  if (signal == SIGUSR1) {
    ++signals_delivered;
  }
}

struct child_report {
  pid_t pid;
  pid_t ppid;
  pid_t tid;
};

static void fail(const char *operation) {
  perror(operation);
  exit(EXIT_FAILURE);
}

static void write_report(int fd, const struct child_report *report) {
  const unsigned char *cursor = (const unsigned char *)report;
  size_t remaining = sizeof(*report);
  while (remaining > 0) {
    ssize_t written = write(fd, cursor, remaining);
    if (written < 0) {
      if (errno == EINTR) {
        continue;
      }
      _exit(100);
    }
    cursor += written;
    remaining -= (size_t)written;
  }
}

static void read_report(int fd, struct child_report *report) {
  unsigned char *cursor = (unsigned char *)report;
  size_t remaining = sizeof(*report);
  while (remaining > 0) {
    ssize_t received = read(fd, cursor, remaining);
    if (received < 0) {
      if (errno == EINTR) {
        continue;
      }
      fail("read");
    }
    if (received == 0) {
      fprintf(stderr, "child report ended early\n");
      exit(EXIT_FAILURE);
    }
    cursor += received;
    remaining -= (size_t)received;
  }
}

int main(void) {
  struct sigaction action = {
      .sa_handler = handle_signal,
  };
  pid_t parent_pid = getpid();
  pid_t parent_ppid = getppid();
  pid_t parent_tid = (pid_t)syscall(SYS_gettid);
  siginfo_t queue_info = {
      .si_signo = SIGUSR1,
      .si_code = SI_QUEUE,
      .si_pid = parent_pid,
      .si_uid = getuid(),
  };
  int report_fds[CHILDREN];
  pid_t children[CHILDREN];

  if (sigemptyset(&action.sa_mask) != 0 ||
      sigaction(SIGUSR1, &action, NULL) != 0) {
    fail("sigaction");
  }
  if (kill(parent_pid, SIGUSR1) != 0 || signals_delivered != 1) {
    fprintf(stderr, "kill(getpid(), SIGUSR1) was not delivered\n");
    return EXIT_FAILURE;
  }
  if (syscall(SYS_tkill, parent_tid, SIGUSR1) != 0 ||
      signals_delivered != 2) {
    fprintf(stderr, "tkill(gettid(), SIGUSR1) was not delivered\n");
    return EXIT_FAILURE;
  }
  if (syscall(SYS_tgkill, parent_pid, parent_tid, SIGUSR1) != 0 ||
      signals_delivered != 3) {
    fprintf(stderr, "tgkill(getpid(), gettid(), SIGUSR1) was not delivered\n");
    return EXIT_FAILURE;
  }
  if (syscall(SYS_rt_sigqueueinfo, parent_pid, SIGUSR1, &queue_info) != 0 ||
      signals_delivered != 4) {
    fprintf(stderr, "rt_sigqueueinfo(getpid(), SIGUSR1) was not delivered\n");
    return EXIT_FAILURE;
  }

  if (parent_pid != parent_tid) {
    fprintf(stderr, "root process PID and TID differ\n");
    return EXIT_FAILURE;
  }

  for (int index = 0; index < CHILDREN; ++index) {
    int pipe_fds[2];
    if (pipe(pipe_fds) != 0) {
      fail("pipe");
    }

    pid_t child = fork();
    if (child < 0) {
      fail("fork");
    }
    if (child == 0) {
      struct child_report report = {
          .pid = getpid(),
          .ppid = getppid(),
          .tid = (pid_t)syscall(SYS_gettid),
      };
      close(pipe_fds[0]);
      write_report(pipe_fds[1], &report);
      close(pipe_fds[1]);
      _exit(0);
    }

    close(pipe_fds[1]);
    report_fds[index] = pipe_fds[0];
    children[index] = child;
  }

  printf("parent pid=%d ppid=%d tid=%d\n", parent_pid, parent_ppid,
         parent_tid);
  for (int index = 0; index < CHILDREN; ++index) {
    struct child_report report;
    int status;
    read_report(report_fds[index], &report);
    close(report_fds[index]);
    pid_t waited = waitpid(children[index], &status, 0);

    if (waited != children[index] || !WIFEXITED(status) ||
        WEXITSTATUS(status) != 0 || report.pid != children[index] ||
        report.ppid != parent_pid || report.tid != report.pid) {
      fprintf(stderr, "PID namespace identity mismatch for child %d\n", index);
      return EXIT_FAILURE;
    }

    printf("child index=%d fork_pid=%d pid=%d ppid=%d tid=%d wait_pid=%d\n",
           index, children[index], report.pid, report.ppid, report.tid, waited);
  }

  return EXIT_SUCCESS;
}
