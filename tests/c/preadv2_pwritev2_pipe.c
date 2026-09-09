/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

enum { PIPE_BUF_BYTES = 4096 };

static ssize_t call_preadv2(int fd, const struct iovec *iov, int count,
                            int64_t offset, int flags) {
  return syscall(SYS_preadv2, fd, iov, count, (uint64_t)offset, 0UL, flags);
}

static ssize_t call_pwritev2(int fd, const struct iovec *iov, int count,
                             int64_t offset, int flags) {
  return syscall(SYS_pwritev2, fd, iov, count, (uint64_t)offset, 0UL, flags);
}

static int read_exact(int fd, char *buffer, size_t length) {
  size_t offset = 0;
  while (offset < length) {
    ssize_t count = read(fd, buffer + offset, length - offset);
    if (count < 0 && errno == EINTR) {
      continue;
    }
    if (count <= 0) {
      fprintf(stderr, "read returned %zd after %zu/%zu bytes: %s\n", count,
              offset, length, strerror(errno));
      return -1;
    }
    offset += (size_t)count;
  }
  return 0;
}

static int fill_pipe(int fd, char value) {
  int capacity = fcntl(fd, F_GETPIPE_SZ);
  if (capacity <= 0) {
    perror("F_GETPIPE_SZ");
    return -1;
  }
  char *fill = malloc((size_t)capacity);
  if (fill == NULL) {
    perror("pipe fill allocation");
    return -1;
  }
  memset(fill, value, (size_t)capacity);
  ssize_t written = write(fd, fill, (size_t)capacity);
  free(fill);
  if (written != capacity) {
    fprintf(stderr, "pipe fill returned %zd/%d with errno %d\n", written,
            capacity, errno);
    return -1;
  }
  return capacity;
}

static int check_nowait_and_error_order(void) {
  int pipe_fds[2];
  if (pipe(pipe_fds) != 0) {
    perror("nowait pipe");
    return -1;
  }

  char byte = 'N';
  struct iovec iov = {.iov_base = &byte, .iov_len = 1};
  struct iovec *bad_iov = (struct iovec *)(uintptr_t)1;

  errno = 0;
  if (call_preadv2(-1, bad_iov, 1, -1, 0) != -1 || errno != EBADF) {
    fprintf(stderr, "preadv2 bad-fd ordering returned errno %d\n", errno);
    return -1;
  }
  errno = 0;
  if (call_pwritev2(-1, bad_iov, 1, -1, 0) != -1 || errno != EBADF) {
    fprintf(stderr, "pwritev2 bad-fd ordering returned errno %d\n", errno);
    return -1;
  }
  errno = 0;
  if (call_preadv2(pipe_fds[1], bad_iov, 1, -1, 0) != -1 || errno != EBADF) {
    fprintf(stderr, "preadv2 access ordering returned errno %d\n", errno);
    return -1;
  }
  errno = 0;
  if (call_pwritev2(pipe_fds[0], bad_iov, 1, -1, 0) != -1 || errno != EBADF) {
    fprintf(stderr, "pwritev2 access ordering returned errno %d\n", errno);
    return -1;
  }
  errno = 0;
  if (call_preadv2(pipe_fds[0], &iov, 1, 0, 0) != -1 || errno != ESPIPE) {
    fprintf(stderr, "preadv2 explicit pipe offset returned errno %d\n", errno);
    return -1;
  }
  errno = 0;
  if (call_pwritev2(pipe_fds[1], &iov, 1, 0, 0) != -1 || errno != ESPIPE) {
    fprintf(stderr, "pwritev2 explicit pipe offset returned errno %d\n", errno);
    return -1;
  }
  errno = 0;
  if (call_preadv2(pipe_fds[0], &iov, 1, -1, RWF_NOWAIT) != -1 ||
      errno != EAGAIN) {
    fprintf(stderr, "preadv2 RWF_NOWAIT returned errno %d\n", errno);
    return -1;
  }

  int capacity = fill_pipe(pipe_fds[1], 'F');
  if (capacity < 0) {
    return -1;
  }
  errno = 0;
  if (call_pwritev2(pipe_fds[1], &iov, 1, -1, RWF_NOWAIT) != -1 ||
      errno != EAGAIN) {
    fprintf(stderr, "pwritev2 RWF_NOWAIT returned errno %d\n", errno);
    return -1;
  }

  char drain[PIPE_BUF_BYTES];
  if (read_exact(pipe_fds[0], drain, sizeof(drain)) != 0) {
    return -1;
  }
  size_t request = (size_t)capacity + PIPE_BUF_BYTES;
  char *payload = malloc(request);
  if (payload == NULL) {
    perror("pwritev2 RWF_NOWAIT partial allocation");
    return -1;
  }
  memset(payload, 'N', request);
  struct iovec partial_iov[2] = {
      {.iov_base = payload, .iov_len = request / 2},
      {.iov_base = payload + request / 2, .iov_len = request - request / 2},
  };
  errno = 0;
  ssize_t partial =
      call_pwritev2(pipe_fds[1], partial_iov, 2, -1, RWF_NOWAIT);
  int partial_errno = errno;
  free(payload);
  if (partial <= 0 || (size_t)partial >= request) {
    fprintf(stderr,
            "partial pwritev2 RWF_NOWAIT returned %zd/%zu with errno %d\n",
            partial, request, partial_errno);
    return -1;
  }
  close(pipe_fds[0]);
  close(pipe_fds[1]);
  puts("preadv2-pwritev2-nowait-and-errors-ok");
  return 0;
}

struct read_release {
  int write_fd;
  struct iovec *iov;
  char *poison;
  int result;
};

static void *release_preadv2(void *opaque) {
  struct read_release *release = opaque;
  struct timespec delay = {.tv_sec = 0, .tv_nsec = 20 * 1000 * 1000};
  while (nanosleep(&delay, &delay) != 0 && errno == EINTR) {
  }
  release->iov[0].iov_base = release->poison;
  release->iov[0].iov_len = 1;
  release->result = write(release->write_fd, "AB", 2) == 2 ? 0 : -1;
  return NULL;
}

static int check_blocking_preadv2_snapshot(void) {
  int pipe_fds[2];
  if (pipe(pipe_fds) != 0) {
    perror("preadv2 snapshot pipe");
    return -1;
  }
  char first = 0;
  char second = 0;
  char poison = 0;
  struct iovec iov[2] = {
      {.iov_base = &first, .iov_len = 1},
      {.iov_base = &second, .iov_len = 1},
  };
  struct read_release release = {
      .write_fd = pipe_fds[1], .iov = iov, .poison = &poison, .result = -1};
  pthread_t thread;
  if (pthread_create(&thread, NULL, release_preadv2, &release) != 0) {
    perror("preadv2 snapshot pthread_create");
    return -1;
  }

  ssize_t count = call_preadv2(pipe_fds[0], iov, 2, -1, 0);
  int join_result = pthread_join(thread, NULL);
  close(pipe_fds[0]);
  close(pipe_fds[1]);
  if (count != 2 || join_result != 0 || release.result != 0 || first != 'A' ||
      second != 'B' || poison != 0 || iov[0].iov_base != &poison) {
    fprintf(stderr,
            "blocking preadv2 snapshot failed: count=%zd join=%d release=%d "
            "bytes=%d,%d,%d live-iov=%p\n",
            count, join_result, release.result, first, second, poison,
            iov[0].iov_base);
    return -1;
  }
  puts("preadv2-snapshot-ok");
  return 0;
}

struct atomic_release {
  int read_fd;
  int capacity;
  struct iovec *iov;
  char *poison;
  int result;
};

static void *release_atomic_pwritev2(void *opaque) {
  struct atomic_release *release = opaque;
  struct timespec delay = {.tv_sec = 0, .tv_nsec = 20 * 1000 * 1000};
  while (nanosleep(&delay, &delay) != 0 && errno == EINTR) {
  }
  release->iov[0].iov_base = release->poison;
  release->iov[0].iov_len = PIPE_BUF_BYTES / 2;

  size_t total = (size_t)release->capacity + PIPE_BUF_BYTES;
  char *received = malloc(total);
  release->result = received != NULL &&
                            read_exact(release->read_fd, received, total) == 0
                        ? 0
                        : -1;
  if (release->result == 0) {
    for (int index = 0; index < release->capacity; ++index) {
      if (received[index] != 'F') {
        release->result = -1;
        break;
      }
    }
  }
  if (release->result == 0) {
    for (size_t index = 0; index < PIPE_BUF_BYTES; ++index) {
      char expected = index < PIPE_BUF_BYTES / 2 ? 'A' : 'B';
      if (received[(size_t)release->capacity + index] != expected) {
        release->result = -1;
        break;
      }
    }
  }
  free(received);
  return NULL;
}

static int check_atomic_pwritev2_snapshot(void) {
  static char first[PIPE_BUF_BYTES / 2];
  static char second[PIPE_BUF_BYTES / 2];
  static char poison[PIPE_BUF_BYTES / 2];
  memset(first, 'A', sizeof(first));
  memset(second, 'B', sizeof(second));
  memset(poison, 'X', sizeof(poison));
  struct iovec iov[2] = {
      {.iov_base = first, .iov_len = sizeof(first)},
      {.iov_base = second, .iov_len = sizeof(second)},
  };

  int pipe_fds[2];
  if (pipe(pipe_fds) != 0) {
    perror("atomic pwritev2 pipe");
    return -1;
  }
  int capacity = fill_pipe(pipe_fds[1], 'F');
  if (capacity < 0) {
    return -1;
  }
  struct atomic_release release = {.read_fd = pipe_fds[0],
                                   .capacity = capacity,
                                   .iov = iov,
                                   .poison = poison,
                                   .result = -1};
  pthread_t thread;
  if (pthread_create(&thread, NULL, release_atomic_pwritev2, &release) != 0) {
    perror("atomic pwritev2 pthread_create");
    return -1;
  }

  ssize_t written = call_pwritev2(pipe_fds[1], iov, 2, -1, 0);
  int join_result = pthread_join(thread, NULL);
  close(pipe_fds[0]);
  close(pipe_fds[1]);
  if (written != PIPE_BUF_BYTES || join_result != 0 || release.result != 0 ||
      iov[0].iov_base != poison) {
    fprintf(stderr,
            "atomic pwritev2 snapshot failed: written=%zd join=%d release=%d "
            "live-iov=%p\n",
            written, join_result, release.result, iov[0].iov_base);
    return -1;
  }
  puts("pwritev2-atomic-snapshot-ok");
  return 0;
}

static int check_large_pwritev2(void) {
  enum { CHUNK_COUNT = 4, CHUNK_SIZE = 32768 };
  static char chunks[CHUNK_COUNT][CHUNK_SIZE];
  struct iovec iov[CHUNK_COUNT];
  for (size_t index = 0; index < CHUNK_COUNT; ++index) {
    memset(chunks[index], 'A' + (int)index, CHUNK_SIZE);
    iov[index].iov_base = chunks[index];
    iov[index].iov_len = CHUNK_SIZE;
  }
  const size_t expected = CHUNK_COUNT * CHUNK_SIZE;

  int pipe_fds[2];
  if (pipe(pipe_fds) != 0) {
    perror("large pwritev2 pipe");
    return -1;
  }
  pid_t child = fork();
  if (child < 0) {
    perror("large pwritev2 fork");
    return -1;
  }
  if (child == 0) {
    close(pipe_fds[1]);
    size_t received = 0;
    char buffer[4096];
    while (received < expected) {
      ssize_t count = read(pipe_fds[0], buffer, sizeof(buffer));
      if (count < 0 && errno == EINTR) {
        continue;
      }
      if (count <= 0) {
        _exit(2);
      }
      for (ssize_t index = 0; index < count; ++index) {
        size_t position = received + (size_t)index;
        if (buffer[index] != (char)('A' + position / CHUNK_SIZE)) {
          _exit(3);
        }
      }
      received += (size_t)count;
    }
    close(pipe_fds[0]);
    _exit(0);
  }

  close(pipe_fds[0]);
  ssize_t written = call_pwritev2(pipe_fds[1], iov, CHUNK_COUNT, -1, 0);
  close(pipe_fds[1]);
  int status = 0;
  if (waitpid(child, &status, 0) != child || written != (ssize_t)expected ||
      !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    fprintf(stderr,
            "large pwritev2 failed: written=%zd/%zu child-status=%#x errno=%d\n",
            written, expected, status, errno);
    return -1;
  }
  puts("pwritev2-large-ok");
  return 0;
}

static volatile sig_atomic_t signal_received;

static void receive_signal(int signal_number) {
  (void)signal_number;
  signal_received = 1;
}

struct signal_context {
  pthread_t target;
  int result;
};

static void *signal_waiter(void *opaque) {
  struct signal_context *context = opaque;
  struct timespec delay = {.tv_sec = 0, .tv_nsec = 20 * 1000 * 1000};
  while (nanosleep(&delay, &delay) != 0 && errno == EINTR) {
  }
  context->result = pthread_kill(context->target, SIGUSR1);
  return NULL;
}

static int check_signal_before_progress(int reading) {
  struct sigaction action = {.sa_handler = receive_signal};
  struct sigaction old_action;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGUSR1, &action, &old_action) != 0) {
    perror("p*v2 signal sigaction");
    return -1;
  }

  int pipe_fds[2];
  if (pipe(pipe_fds) != 0) {
    perror("p*v2 signal pipe");
    return -1;
  }
  int capacity = 0;
  if (!reading && (capacity = fill_pipe(pipe_fds[1], 'F')) < 0) {
    return -1;
  }

  struct signal_context context = {.target = pthread_self(), .result = -1};
  pthread_t thread;
  signal_received = 0;
  if (pthread_create(&thread, NULL, signal_waiter, &context) != 0) {
    perror("p*v2 signal pthread_create");
    return -1;
  }
  char byte = 'S';
  struct iovec iov = {.iov_base = &byte, .iov_len = 1};
  errno = 0;
  ssize_t result = reading ? call_preadv2(pipe_fds[0], &iov, 1, -1, 0)
                           : call_pwritev2(pipe_fds[1], &iov, 1, -1, 0);
  int saved_errno = errno;
  int join_result = pthread_join(thread, NULL);

  if (capacity > 0) {
    char *drain = malloc((size_t)capacity);
    if (drain == NULL || read_exact(pipe_fds[0], drain, (size_t)capacity) != 0) {
      return -1;
    }
    free(drain);
  }
  close(pipe_fds[0]);
  close(pipe_fds[1]);
  sigaction(SIGUSR1, &old_action, NULL);
  if (result != -1 || saved_errno != EINTR || join_result != 0 ||
      context.result != 0 || !signal_received) {
    fprintf(stderr,
            "%s signal failed: result=%zd errno=%d join=%d kill=%d signal=%d\n",
            reading ? "preadv2" : "pwritev2", result, saved_errno,
            join_result, context.result, signal_received);
    return -1;
  }
  puts(reading ? "preadv2-signal-ok" : "pwritev2-signal-ok");
  return 0;
}

struct replacement_writer {
  int fd;
  _Atomic int entered;
  ssize_t result;
  int error;
};

static void *write_replaced_fd(void *opaque) {
  struct replacement_writer *writer = opaque;
  char byte = 'R';
  struct iovec iov = {.iov_base = &byte, .iov_len = 1};
  atomic_store_explicit(&writer->entered, 1, memory_order_release);
  errno = 0;
  writer->result = call_pwritev2(writer->fd, &iov, 1, -1, 0);
  writer->error = errno;
  return NULL;
}

static int check_pwritev2_fd_replacement(void) {
  int original[2];
  int replacement[2];
  if (pipe(original) != 0 || pipe(replacement) != 0) {
    perror("pwritev2 replacement pipe");
    return -1;
  }
  int capacity = fill_pipe(original[1], 'F');
  if (capacity < 0) {
    return -1;
  }

  struct replacement_writer writer = {
      .fd = original[1], .entered = 0, .result = -1, .error = 0};
  pthread_t thread;
  if (pthread_create(&thread, NULL, write_replaced_fd, &writer) != 0) {
    perror("pwritev2 replacement pthread_create");
    return -1;
  }
  while (!atomic_load_explicit(&writer.entered, memory_order_acquire)) {
    sched_yield();
  }
  for (int attempt = 0; attempt < 64; ++attempt) {
    sched_yield();
  }
  if (dup2(replacement[1], original[1]) != original[1]) {
    perror("pwritev2 replacement dup2");
    return -1;
  }
  if (pthread_join(thread, NULL) != 0 || writer.result != -1 ||
      writer.error != EOPNOTSUPP) {
    fprintf(stderr,
            "pwritev2 replacement returned %zd with errno %d, expected EOPNOTSUPP\n",
            writer.result, writer.error);
    return -1;
  }

  int flags = fcntl(replacement[0], F_GETFL);
  if (flags < 0 || fcntl(replacement[0], F_SETFL, flags | O_NONBLOCK) != 0) {
    perror("pwritev2 replacement nonblocking read");
    return -1;
  }
  char byte = 0;
  errno = 0;
  if (read(replacement[0], &byte, 1) != -1 || errno != EAGAIN) {
    fprintf(stderr, "pwritev2 wrote through a replaced descriptor\n");
    return -1;
  }

  char *drain = malloc((size_t)capacity);
  if (drain == NULL || read_exact(original[0], drain, (size_t)capacity) != 0) {
    return -1;
  }
  free(drain);
  close(original[0]);
  close(original[1]);
  close(replacement[0]);
  close(replacement[1]);
  puts("pwritev2-fd-replacement-ok");
  return 0;
}

int main(int argc, char **argv) {
  if (argc > 1 && strcmp(argv[1], "fd-replacement") == 0) {
    return check_pwritev2_fd_replacement() == 0 ? 0 : 1;
  }
  if (argc > 1 && strcmp(argv[1], "record-pipe") == 0) {
    if (check_blocking_preadv2_snapshot() != 0 ||
        check_atomic_pwritev2_snapshot() != 0 ||
        check_large_pwritev2() != 0) {
      return 1;
    }
    puts("preadv2-pwritev2-record-pipe-ok");
    return 0;
  }

  if (check_nowait_and_error_order() != 0 ||
      check_blocking_preadv2_snapshot() != 0 ||
      check_atomic_pwritev2_snapshot() != 0 || check_large_pwritev2() != 0 ||
      check_signal_before_progress(1) != 0 ||
      check_signal_before_progress(0) != 0) {
    return 1;
  }
  puts("preadv2-pwritev2-pipe-ok");
  return 0;
}
