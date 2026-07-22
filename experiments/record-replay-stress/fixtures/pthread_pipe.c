#include <errno.h>
#include <inttypes.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <unistd.h>

enum {
  WORKERS = 4,
  ITERATIONS = 250000,
};

struct worker_args {
  int worker;
  int pipe_fd;
};

struct message {
  int worker;
  uint64_t checksum;
};

static pthread_barrier_t start_barrier;

static uint64_t mix(int worker) {
  uint64_t state = (uint64_t)worker + 1;
  for (int iteration = 1; iteration <= ITERATIONS; iteration++) {
    state =
        (state ^ (uint64_t)iteration) * UINT64_C(1103515245) + UINT64_C(12345);
  }
  return state;
}

static void *run_worker(void *opaque) {
  struct worker_args *args = opaque;
  int barrier_status = pthread_barrier_wait(&start_barrier);
  if (barrier_status != 0 && barrier_status != PTHREAD_BARRIER_SERIAL_THREAD) {
    return (void *)1;
  }

  struct message message = {
      .worker = args->worker,
      .checksum = mix(args->worker),
  };
  const char *data = (const char *)&message;
  size_t remaining = sizeof(message);
  while (remaining > 0) {
    ssize_t written = write(args->pipe_fd, data, remaining);
    if (written < 0 && errno == EINTR) {
      continue;
    }
    if (written <= 0) {
      return (void *)1;
    }
    data += written;
    remaining -= (size_t)written;
  }
  return NULL;
}

static int read_message(int fd, struct message *message) {
  char *data = (char *)message;
  size_t remaining = sizeof(*message);
  while (remaining > 0) {
    ssize_t received = read(fd, data, remaining);
    if (received < 0 && errno == EINTR) {
      continue;
    }
    if (received <= 0) {
      return -1;
    }
    data += received;
    remaining -= (size_t)received;
  }
  return 0;
}

int main(void) {
  int pipe_fds[2];
  pthread_t threads[WORKERS];
  struct worker_args args[WORKERS];
  struct message messages[WORKERS];
  uint64_t checksum = 0;

  if (pipe(pipe_fds) != 0 ||
      pthread_barrier_init(&start_barrier, NULL, WORKERS + 1) != 0) {
    return 2;
  }
  for (int worker = 0; worker < WORKERS; worker++) {
    args[worker] = (struct worker_args){
        .worker = worker,
        .pipe_fd = pipe_fds[1],
    };
    if (pthread_create(&threads[worker], NULL, run_worker, &args[worker]) !=
        0) {
      return 3;
    }
  }

  int barrier_status = pthread_barrier_wait(&start_barrier);
  if (barrier_status != 0 && barrier_status != PTHREAD_BARRIER_SERIAL_THREAD) {
    return 4;
  }
  for (int index = 0; index < WORKERS; index++) {
    if (read_message(pipe_fds[0], &messages[index]) != 0) {
      return 5;
    }
    checksum ^= messages[index].checksum;
  }
  for (int worker = 0; worker < WORKERS; worker++) {
    void *result = NULL;
    if (pthread_join(threads[worker], &result) != 0 || result != NULL) {
      return 6;
    }
  }

  close(pipe_fds[0]);
  close(pipe_fds[1]);
  pthread_barrier_destroy(&start_barrier);

  printf("order=");
  for (int index = 0; index < WORKERS; index++) {
    printf("%s%d", index == 0 ? "" : ",", messages[index].worker);
  }
  printf(" checksum=%" PRIu64 "\n", checksum);
  return 0;
}
