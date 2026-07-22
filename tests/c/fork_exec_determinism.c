/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <spawn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

extern char** environ;

#define VFORK_STACK_SIZE (64 * 1024)

static unsigned char vfork_stack[VFORK_STACK_SIZE]
    __attribute__((aligned(16)));
static volatile sig_atomic_t fork_signal_deliveries;
static volatile sig_atomic_t fork_signal_phase;
static volatile sig_atomic_t fork_signal_observed_phase;

struct VforkContext {
  volatile int entered;
};

static int write_all(int fd, const void* buffer, size_t length) {
  const unsigned char* cursor = buffer;
  while (length > 0) {
    const ssize_t written = write(fd, cursor, length);
    if (written < 0) {
      if (errno == EINTR) {
        continue;
      }
      return -1;
    }
    cursor += written;
    length -= (size_t)written;
  }
  return 0;
}

static int read_all(int fd, char* buffer, size_t capacity) {
  size_t length = 0;
  while (length < capacity) {
    const ssize_t bytes = read(fd, buffer + length, capacity - length);
    if (bytes == 0) {
      break;
    }
    if (bytes < 0) {
      if (errno == EINTR) {
        continue;
      }
      return -1;
    }
    length += (size_t)bytes;
  }
  return (int)length;
}

static int wait_for_exit(pid_t child, int expected_status) {
  int status = 0;
  pid_t waited;
  do {
    waited = waitpid(child, &status, 0);
  } while (waited < 0 && errno == EINTR);
  if (waited != child || !WIFEXITED(status) ||
      WEXITSTATUS(status) != expected_status) {
    return -1;
  }
  return 0;
}

static int inherited_exec_child(const char* fd_argument) {
  char* end = NULL;
  const long parsed_fd = strtol(fd_argument, &end, 10);
  if (end == fd_argument || *end != '\0' || parsed_fd < 0 ||
      parsed_fd > INT_MAX) {
    fputs("invalid inherited fd\n", stderr);
    return 1;
  }
  const int fd = (int)parsed_fd;
  const char* value = getenv("FORK_EXEC_VALUE");
  char cwd[PATH_MAX];
  if (value == NULL || strcmp(value, "expected") != 0 ||
      getcwd(cwd, sizeof(cwd)) == NULL ||
      strcmp(cwd, "/tmp/hermit-fork-exec-determinism") != 0 ||
      fcntl(fd, F_GETFD) < 0 ||
      write_all(fd, "child\n", strlen("child\n")) != 0) {
    fputs("exec did not inherit fd, environment, and cwd\n", stderr);
    return 1;
  }
  puts("exec inherited env cwd fd");
  return 0;
}

static int test_inherited_exec_state(void) {
  const char* directory = "/tmp/hermit-fork-exec-determinism";
  if (mkdir(directory, 0700) != 0 && errno != EEXIST) {
    perror("mkdir");
    return 1;
  }
  if (chdir(directory) != 0) {
    perror("chdir");
    return 1;
  }

  const int fd = open("inherited.txt", O_CREAT | O_TRUNC | O_RDWR, 0600);
  if (fd < 0 || write_all(fd, "parent\n", strlen("parent\n")) != 0) {
    perror("open/write");
    return 1;
  }
  if (setenv("FORK_EXEC_VALUE", "expected", 1) != 0) {
    perror("setenv");
    return 1;
  }

  char fd_argument[32];
  snprintf(fd_argument, sizeof(fd_argument), "%d", fd);
  const pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return 1;
  }
  if (child == 0) {
    char* const arguments[] = {
        (char*)"fork_exec_determinism",
        (char*)"inherited-exec-child",
        fd_argument,
        NULL,
    };
    execv("/proc/self/exe", arguments);
    _exit(127);
  }
  if (wait_for_exit(child, 0) != 0) {
    fputs("inherited-state child failed\n", stderr);
    return 1;
  }

  if (lseek(fd, 0, SEEK_SET) != 0) {
    perror("lseek");
    return 1;
  }
  char contents[32] = {0};
  const int length = read_all(fd, contents, sizeof(contents));
  if (length != (int)strlen("parent\nchild\n") ||
      memcmp(contents, "parent\nchild\n", (size_t)length) != 0) {
    fputs("inherited fd contents are incorrect\n", stderr);
    return 1;
  }
  close(fd);
  unlink("inherited.txt");
  puts("fd contents=parent+child");
  return 0;
}

static int vfork_child(void* argument) {
  struct VforkContext* context = argument;
  context->entered = 1;
  char* const arguments[] = {
      (char*)"fork_exec_determinism",
      (char*)"vfork-exec-child",
      NULL,
  };
  execv("/proc/self/exe", arguments);
  _exit(127);
}

static int test_vfork_exec(void) {
  struct VforkContext context = {.entered = 0};
  const pid_t child = clone(
      vfork_child,
      vfork_stack + sizeof(vfork_stack),
      CLONE_VM | CLONE_VFORK | SIGCHLD,
      &context);
  if (child < 0) {
    perror("clone(CLONE_VFORK)");
    return 1;
  }
  if (context.entered != 1) {
    fputs("CLONE_VFORK parent resumed before child entered\n", stderr);
    return 1;
  }
  if (wait_for_exit(child, 0) != 0) {
    fputs("CLONE_VFORK exec child failed\n", stderr);
    return 1;
  }
  puts("vfork child reached exec=1");
  puts("vfork child status=0");
  return 0;
}

static int test_multi_fork(void) {
  enum { CHILDREN = 4 };
  pid_t children[CHILDREN];
  for (int index = 0; index < CHILDREN; ++index) {
    children[index] = fork();
    if (children[index] < 0) {
      perror("fork");
      return 1;
    }
    if (children[index] == 0) {
      _exit(10 + index);
    }
  }

  for (int index = 0; index < CHILDREN; ++index) {
    if (wait_for_exit(children[index], 10 + index) != 0) {
      fputs("multi-fork child failed\n", stderr);
      return 1;
    }
    printf("wait child=%d status=%d\n", index, 10 + index);
  }
  return 0;
}

static int spawn_child(void) {
  const char* value = getenv("SPAWN_VALUE");
  if (value == NULL || strcmp(value, "spawned") != 0) {
    fputs("posix_spawn environment missing\n", stderr);
    return 1;
  }
  puts("spawn child env=spawned");
  return 0;
}

static int test_posix_spawn(void) {
  int output_pipe[2];
  if (pipe(output_pipe) != 0) {
    perror("pipe");
    return 1;
  }
  if (setenv("SPAWN_VALUE", "spawned", 1) != 0 ||
      setenv("PATH", "/usr/bin:/bin", 1) != 0) {
    perror("setenv");
    return 1;
  }

  posix_spawn_file_actions_t actions;
  int result = posix_spawn_file_actions_init(&actions);
  if (result == 0) {
    result = posix_spawn_file_actions_adddup2(
        &actions, output_pipe[1], STDOUT_FILENO);
  }
  if (result == 0) {
    result = posix_spawn_file_actions_addclose(&actions, output_pipe[0]);
  }
  if (result == 0) {
    result = posix_spawn_file_actions_addclose(&actions, output_pipe[1]);
  }
  if (result != 0) {
    errno = result;
    perror("posix_spawn_file_actions");
    return 1;
  }

  char* const child_arguments[] = {
      (char*)"fork_exec_determinism",
      (char*)"spawn-child",
      NULL,
  };
  pid_t child = -1;
  result = posix_spawn(
      &child, "/proc/self/exe", &actions, NULL, child_arguments, environ);
  posix_spawn_file_actions_destroy(&actions);
  close(output_pipe[1]);
  if (result != 0) {
    errno = result;
    perror("posix_spawn");
    return 1;
  }

  char output[64] = {0};
  const int output_length =
      read_all(output_pipe[0], output, sizeof(output) - 1);
  close(output_pipe[0]);
  if (output_length < 0 || wait_for_exit(child, 0) != 0 ||
      strcmp(output, "spawn child env=spawned\n") != 0) {
    fputs("posix_spawn child output/status mismatch\n", stderr);
    return 1;
  }
  printf("posix_spawn output=%s", output);

  char* const shell_arguments[] = {
      (char*)"sh",
      (char*)"-c",
      (char*)"exit 7",
      NULL,
  };
  child = -1;
  result = posix_spawnp(
      &child, "sh", NULL, NULL, shell_arguments, environ);
  if (result != 0) {
    errno = result;
    perror("posix_spawnp");
    return 1;
  }
  if (wait_for_exit(child, 7) != 0) {
    fputs("posix_spawnp child status mismatch\n", stderr);
    return 1;
  }
  puts("posix_spawnp status=7");
  return 0;
}

static void fork_signal_handler(int signal_number) {
  (void)signal_number;
  fork_signal_observed_phase = fork_signal_phase;
  ++fork_signal_deliveries;
  static const char message[] = "fork signal handler\n";
  (void)write_all(STDOUT_FILENO, message, sizeof(message) - 1);
}

static int test_fork_signal_order(void) {
  sigset_t blocked;
  sigset_t previous;
  sigemptyset(&blocked);
  sigaddset(&blocked, SIGUSR1);
  if (sigprocmask(SIG_BLOCK, &blocked, &previous) != 0) {
    perror("sigprocmask");
    return 1;
  }

  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = fork_signal_handler;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGUSR1, &action, NULL) != 0) {
    perror("sigaction");
    return 1;
  }

  int ready_pipe[2];
  if (pipe(ready_pipe) != 0) {
    perror("pipe");
    return 1;
  }
  const pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return 1;
  }
  if (child == 0) {
    close(ready_pipe[0]);
    if (kill(getppid(), SIGUSR1) != 0 ||
        write_all(ready_pipe[1], "R", 1) != 0) {
      _exit(1);
    }
    _exit(0);
  }

  close(ready_pipe[1]);
  char ready = 0;
  if (read_all(ready_pipe[0], &ready, 1) != 1 || ready != 'R') {
    fputs("fork child did not signal readiness\n", stderr);
    return 1;
  }
  close(ready_pipe[0]);

  sigset_t pending;
  if (sigpending(&pending) != 0 || sigismember(&pending, SIGUSR1) != 1) {
    fputs("SIGUSR1 was not pending after child notification\n", stderr);
    return 1;
  }

  fork_signal_phase = 2;
  sigset_t wait_mask = previous;
  sigdelset(&wait_mask, SIGUSR1);
  while (fork_signal_deliveries == 0) {
    errno = 0;
    if (sigsuspend(&wait_mask) != -1 || errno != EINTR) {
      perror("sigsuspend");
      return 1;
    }
  }
  if (wait_for_exit(child, 0) != 0 ||
      fork_signal_deliveries != 1 ||
      fork_signal_observed_phase != 2) {
    fputs("forked signal delivery order mismatch\n", stderr);
    return 1;
  }
  if (sigprocmask(SIG_SETMASK, &previous, NULL) != 0) {
    perror("sigprocmask restore");
    return 1;
  }
  printf(
      "fork signal phase=%d deliveries=%d child=0\n",
      (int)fork_signal_observed_phase,
      (int)fork_signal_deliveries);
  return 0;
}

int main(int argc, char** argv) {
  if (argc < 2) {
    fprintf(stderr, "usage: %s SCENARIO\n", argv[0]);
    return 2;
  }
  if (strcmp(argv[1], "inherited-exec") == 0) {
    return test_inherited_exec_state();
  }
  if (strcmp(argv[1], "inherited-exec-child") == 0 && argc == 3) {
    return inherited_exec_child(argv[2]);
  }
  if (strcmp(argv[1], "vfork-exec") == 0) {
    return test_vfork_exec();
  }
  if (strcmp(argv[1], "vfork-exec-child") == 0) {
    return 0;
  }
  if (strcmp(argv[1], "multi-fork") == 0) {
    return test_multi_fork();
  }
  if (strcmp(argv[1], "posix-spawn") == 0) {
    return test_posix_spawn();
  }
  if (strcmp(argv[1], "spawn-child") == 0) {
    return spawn_child();
  }
  if (strcmp(argv[1], "fork-signal") == 0) {
    return test_fork_signal_order();
  }
  fprintf(stderr, "unknown scenario: %s\n", argv[1]);
  return 2;
}
