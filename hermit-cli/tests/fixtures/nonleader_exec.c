/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved. Licensed under the BSD-style license in LICENSE. */

#define _GNU_SOURCE
#include <errno.h>
#include <inttypes.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define CHECK(expression)                                                       \
  do {                                                                          \
    if (!(expression)) {                                                        \
      fprintf(stderr, "line %d: %s (errno=%d)\n", __LINE__, #expression, errno);   \
      _exit(90);                                                                \
    }                                                                           \
  } while (0)

enum { ROUNDS = 2, SAMPLES = 4, FINAL_STATUS = 73 };
static pthread_mutex_t mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t ready = PTHREAD_COND_INITIALIZER;
static pthread_cond_t parked = PTHREAD_COND_INITIALIZER;
static const char *program;
static int round_number, ready_fd, ack_fd;
static pid_t peer_tid;
static uint64_t previous_time;
static int leader_waiting;

static pid_t my_tid(void) { return (pid_t)syscall(SYS_gettid); }
static pid_t my_pid(void) { return (pid_t)syscall(SYS_getpid); }

static uint64_t parse(const char *text) {
  char *end;
  errno = 0;
  uint64_t value = strtoull(text, &end, 10);
  CHECK(errno == 0 && end != text && *end == '\0');
  return value;
}

static void byte_write(int fd, unsigned char value) {
  CHECK(write(fd, &value, 1) == 1);
}

static unsigned char byte_read(int fd) {
  unsigned char value;
  CHECK(read(fd, &value, 1) == 1);
  return value;
}

/* Real branches between raw syscalls exercise the surviving PMU clock. Every
 * nanosecond sample and work result is printed and compared without rounding. */
static uint64_t samples(const char *phase, int round, uint64_t previous) {
  for (unsigned index = 0; index < SAMPLES; ++index) {
    volatile uint64_t work = 1;
    for (unsigned i = 0; i < 10000 + index * 1000; ++i) {
      if (i & 1)
        work = work * 3 + i;
      else
        work ^= i + 17;
    }
    struct timespec now;
    CHECK(syscall(SYS_clock_gettime, CLOCK_REALTIME, &now) == 0);
    CHECK(now.tv_sec >= 0 && now.tv_nsec >= 0 && now.tv_nsec < 1000000000);
    uint64_t nanos = (uint64_t)now.tv_sec * 1000000000 + (uint64_t)now.tv_nsec;
    CHECK(nanos > previous);
    printf("sample round=%d phase=%s index=%u nanos=%" PRIu64 " work=%" PRIu64
           "\n", round, phase, index, nanos, work);
    previous = nanos;
  }
  return previous;
}

static void *park_peer(void *unused) {
  (void)unused;
  CHECK(pthread_mutex_lock(&mutex) == 0);
  peer_tid = my_tid();
  CHECK(pthread_cond_signal(&ready) == 0);
  /* Neither the leader nor this peer is ever signalled on this condition. */
  for (;;)
    CHECK(pthread_cond_wait(&parked, &mutex) == 0);
  return NULL;
}

static void *exec_worker(void *unused) {
  (void)unused;
  CHECK(pthread_mutex_lock(&mutex) == 0);
  while (!peer_tid)
    CHECK(pthread_cond_wait(&ready, &mutex) == 0);
  /* Obtaining this mutex proves the leader and peer released it in cond_wait.
   * Keep it locked through exec, so even a spurious wake cannot let either run. */
  CHECK(leader_waiting);
  pid_t pid = my_pid(), worker = my_tid();
  CHECK(pid != worker && pid != peer_tid && worker != peer_tid);
  CHECK(syscall(SYS_tgkill, pid, pid, 0) == 0);
  CHECK(syscall(SYS_tgkill, pid, worker, 0) == 0);
  CHECK(syscall(SYS_tgkill, pid, peer_tid, 0) == 0);
  printf("before round=%d pid=%d worker=%d peer=%d\n", round_number, pid,
         worker, peer_tid);
  uint64_t last = samples("before", round_number, previous_time);
  char next[16], leader[16], former[16], peer[16], clock[32], out[16], in[16];
  CHECK(snprintf(next, sizeof(next), "%d", round_number + 1) > 0);
  CHECK(snprintf(leader, sizeof(leader), "%d", pid) > 0);
  CHECK(snprintf(former, sizeof(former), "%d", worker) > 0);
  CHECK(snprintf(peer, sizeof(peer), "%d", peer_tid) > 0);
  CHECK(snprintf(clock, sizeof(clock), "%" PRIu64, last) > 0);
  CHECK(snprintf(out, sizeof(out), "%d", ready_fd) > 0);
  CHECK(snprintf(in, sizeof(in), "%d", ack_fd) > 0);
  char *const args[] = {(char *)program, "image", next, leader, former,
                        peer, clock, out, in, NULL};
  CHECK(fflush(stdout) == 0);
  execv(program, args);
  CHECK(0 && "worker exec must succeed");
  return NULL;
}

static void start_round(void) {
  CHECK(pthread_mutex_lock(&mutex) == 0);
  pthread_t peer, worker;
  CHECK(pthread_create(&peer, NULL, park_peer, NULL) == 0);
  CHECK(pthread_create(&worker, NULL, exec_worker, NULL) == 0);
  leader_waiting = 1;
  for (;;)
    CHECK(pthread_cond_wait(&parked, &mutex) == 0);
}

int main(int argc, char **argv) {
  program = argv[0];
  if (argc == 9 && strcmp(argv[1], "image") == 0) {
    round_number = (int)parse(argv[2]);
    CHECK(round_number > 0 && round_number <= ROUNDS);
    pid_t leader = (pid_t)parse(argv[3]);
    pid_t former = (pid_t)parse(argv[4]);
    pid_t peer = (pid_t)parse(argv[5]);
    previous_time = parse(argv[6]);
    ready_fd = (int)parse(argv[7]);
    ack_fd = (int)parse(argv[8]);
    CHECK(my_pid() == leader && my_tid() == leader);
    CHECK(syscall(SYS_tgkill, leader, leader, 0) == 0);
    /* These are actual IDs in the guest PID namespace. No procfs host IDs or
     * new threads intervene between exec and the negative existence probes. */
    errno = 0;
    CHECK(syscall(SYS_tgkill, leader, former, 0) == -1 && errno == ESRCH);
    errno = 0;
    CHECK(syscall(SYS_tgkill, leader, peer, 0) == -1 && errno == ESRCH);
    printf("after round=%d pid=%d tid=%d former=%d peer=%d gone=ESRCH\n",
           round_number - 1, my_pid(), my_tid(), former, peer);
    previous_time = samples("after", round_number - 1, previous_time);
    CHECK(fflush(stdout) == 0);
    byte_write(ready_fd, (unsigned char)round_number);
    CHECK(byte_read(ack_fd) == (unsigned char)round_number);
    if (round_number == ROUNDS)
      return FINAL_STATUS;
    start_round();
  }
  CHECK(argc == 1);
  int child_ready[2], parent_ack[2];
  CHECK(pipe(child_ready) == 0 && pipe(parent_ack) == 0);
  pid_t child = fork();
  CHECK(child >= 0);
  if (child == 0) {
    CHECK(close(child_ready[0]) == 0 && close(parent_ack[1]) == 0);
    ready_fd = child_ready[1];
    ack_fd = parent_ack[0];
    start_round();
  }
  CHECK(close(child_ready[1]) == 0 && close(parent_ack[0]) == 0);
  for (int round = 1; round <= ROUNDS; ++round) {
    CHECK(byte_read(child_ready[0]) == round);
    int status;
    /* A displaced leader must never surface as an early process exit. The
     * replacement image is causally held alive until this check completes. */
    CHECK(waitpid(child, &status, WNOHANG) == 0);
    printf("wait round=%d child=%d running\n", round - 1, child);
    CHECK(fflush(stdout) == 0);
    byte_write(parent_ack[1], (unsigned char)round);
  }
  int status;
  CHECK(waitpid(child, &status, 0) == child);
  CHECK(WIFEXITED(status) && WEXITSTATUS(status) == FINAL_STATUS);
  errno = 0;
  CHECK(waitpid(child, &status, WNOHANG) == -1 && errno == ECHILD);
  CHECK(close(child_ready[0]) == 0 && close(parent_ack[1]) == 0);
  puts("nonleader-exec-ok rounds=2 final=73 reaped=once");
  return 0;
}
