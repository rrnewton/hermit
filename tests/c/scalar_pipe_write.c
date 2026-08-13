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
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <unistd.h>

enum { PIPE_CAPACITY = 4096, PAYLOAD_SIZE = 8190 };

struct writer_args {
  int fd;
  const unsigned char *payload;
  size_t length;
  _Atomic int started;
  ssize_t result;
  int error;
};

static volatile sig_atomic_t sigpipe_count;

static unsigned char payload_byte(size_t index) {
  return (unsigned char)((index * 37u + 11u) & 0xffu);
}

static void read_payload(int fd) {
  unsigned char buffer[1024];
  size_t total = 0;
  int valid = 1;

  for (;;) {
    ssize_t count = read(fd, buffer, sizeof(buffer));
    if (count < 0 && errno == EINTR) {
      continue;
    }
    if (count < 0) {
      valid = 0;
      break;
    }
    if (count == 0) {
      break;
    }
    for (ssize_t index = 0; index < count; index++) {
      if (total >= PAYLOAD_SIZE || buffer[index] != payload_byte(total)) {
        valid = 0;
      }
      total++;
    }
  }

  close(fd);
  _exit(valid && total == PAYLOAD_SIZE ? EXIT_SUCCESS : EXIT_FAILURE);
}

static int read_exact(int fd, unsigned char *buffer, size_t length) {
  size_t total = 0;
  while (total < length) {
    ssize_t count = read(fd, buffer + total, length - total);
    if (count < 0 && errno == EINTR) {
      continue;
    }
    if (count <= 0) {
      return -1;
    }
    total += (size_t)count;
  }
  return 0;
}

static void *writer_thread(void *opaque) {
  struct writer_args *args = opaque;
  atomic_store_explicit(&args->started, 1, memory_order_release);
  errno = 0;
  args->result = write(args->fd, args->payload, args->length);
  args->error = errno;
  return NULL;
}

static void count_signal(int signal_number) {
  if (signal_number == SIGPIPE) {
    sigpipe_count++;
  }
}

static int wait_until_started(struct writer_args *args) {
  while (!atomic_load_explicit(&args->started, memory_order_acquire)) {
    sched_yield();
  }
  return 0;
}

static int wait_for_pipe_bytes(int fd, int expected) {
  for (int attempt = 0; attempt < 10000; attempt++) {
    int available = -1;
    if (ioctl(fd, FIONREAD, &available) != 0) {
      return -1;
    }
    if (available == expected) {
      return 0;
    }
    sched_yield();
  }
  return -1;
}

static int check_close_reuse(const unsigned char *payload) {
  int original[2];
  int replacement[2];
  if (pipe(original) != 0 || pipe(replacement) != 0 ||
      fcntl(original[1], F_SETPIPE_SZ, PIPE_CAPACITY) != PIPE_CAPACITY) {
    return -1;
  }

  struct writer_args args = {
      .fd = original[1],
      .payload = payload,
      .length = PAYLOAD_SIZE,
      .started = 0,
      .result = -1,
      .error = 0,
  };
  pthread_t writer;
  if (pthread_create(&writer, NULL, writer_thread, &args) != 0) {
    return -1;
  }
  wait_until_started(&args);
  // Seeing one full page proves the writer completed its first positive-short
  // attempt and is now in the retry path. A pre-call flag or fixed yield count
  // cannot establish that boundary.
  if (wait_for_pipe_bytes(original[0], PIPE_CAPACITY) != 0) {
    return -1;
  }
  if (dup2(replacement[1], original[1]) != original[1]) {
    return -1;
  }

  unsigned char received[PAYLOAD_SIZE];
  if (read_exact(original[0], received, sizeof(received)) != 0 ||
      pthread_join(writer, NULL) != 0 || args.result != PAYLOAD_SIZE ||
      memcmp(received, payload, sizeof(received)) != 0) {
    return -1;
  }

  int flags = fcntl(replacement[0], F_GETFL);
  unsigned char unexpected = 0;
  if (flags < 0 || fcntl(replacement[0], F_SETFL, flags | O_NONBLOCK) != 0 ||
      read(replacement[0], &unexpected, 1) != -1 || errno != EAGAIN) {
    return -1;
  }
  return 0;
}

static int check_sigpipe(const unsigned char *payload) {
  int fds[2];
  if (pipe(fds) != 0 ||
      fcntl(fds[1], F_SETPIPE_SZ, PIPE_CAPACITY) != PIPE_CAPACITY) {
    return -1;
  }
  struct writer_args args = {
      .fd = fds[1],
      .payload = payload,
      .length = PAYLOAD_SIZE,
      .started = 0,
      .result = -2,
      .error = 0,
  };
  pthread_t writer;
  sigpipe_count = 0;
  if (pthread_create(&writer, NULL, writer_thread, &args) != 0) {
    return -1;
  }
  wait_until_started(&args);
  if (wait_for_pipe_bytes(fds[0], PIPE_CAPACITY) != 0) {
    return -1;
  }
  close(fds[0]);
  // Linux returns progress already made by the logical write, while still
  // delivering SIGPIPE when the reader disappears before its remainder.
  if (pthread_join(writer, NULL) != 0 || args.result != PIPE_CAPACITY ||
      sigpipe_count != 1) {
    return -1;
  }
  return 0;
}

int main(int argc, char **argv) {
  struct sigaction action = {.sa_handler = count_signal};
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGPIPE, &action, NULL) != 0) {
    return EXIT_FAILURE;
  }

  static unsigned char payload[PAYLOAD_SIZE];
  for (size_t index = 0; index < sizeof(payload); index++) {
    payload[index] = payload_byte(index);
  }

  if (argc == 2 && strcmp(argv[1], "stdout") == 0) {
    return write(STDOUT_FILENO, payload, sizeof(payload)) ==
                   (ssize_t)sizeof(payload)
               ? EXIT_SUCCESS
               : EXIT_FAILURE;
  }
  if (argc == 2 && strcmp(argv[1], "close-reuse") == 0) {
    return check_close_reuse(payload) == 0 ? EXIT_SUCCESS : EXIT_FAILURE;
  }
  if (argc == 2 && strcmp(argv[1], "sigpipe") == 0) {
    return check_sigpipe(payload) == 0 ? EXIT_SUCCESS : EXIT_FAILURE;
  }
  int pipe_fds[2];
  if (pipe2(pipe_fds, 0) != 0) {
    perror("pipe2");
    return EXIT_FAILURE;
  }
  if (fcntl(pipe_fds[1], F_SETPIPE_SZ, PIPE_CAPACITY) != PIPE_CAPACITY ||
      fcntl(pipe_fds[1], F_GETPIPE_SZ) != PIPE_CAPACITY) {
    perror("pipe capacity");
    return EXIT_FAILURE;
  }

  pid_t reader = fork();
  if (reader < 0) {
    perror("fork");
    return EXIT_FAILURE;
  }
  if (reader == 0) {
    close(pipe_fds[1]);
    read_payload(pipe_fds[0]);
  }

  close(pipe_fds[0]);
  errno = 0;
  ssize_t written = write(pipe_fds[1], payload, sizeof(payload));
  int write_errno = errno;
  close(pipe_fds[1]);

  int reader_status = 0;
  if (waitpid(reader, &reader_status, 0) != reader) {
    perror("waitpid");
    return EXIT_FAILURE;
  }

  if (written != (ssize_t)sizeof(payload) || !WIFEXITED(reader_status) ||
      WEXITSTATUS(reader_status) != EXIT_SUCCESS) {
    fprintf(stderr,
            "scalar pipe write returned %zd/%zu (errno=%d); reader status=%#x\n",
            written, sizeof(payload), write_errno, reader_status);
    return EXIT_FAILURE;
  }

  puts("scalar-pipe-write-ok");
  return EXIT_SUCCESS;
}
