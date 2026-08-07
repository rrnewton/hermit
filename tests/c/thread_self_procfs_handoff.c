/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Regression for #964: /proc/thread-self/{stat,status} must report the thread
 * that OPENED the descriptor, not the one that reads it. The kernel resolves
 * thread-self at open time, so an fd handed to another thread still describes
 * the opener.
 *
 * THE OPENER IS DELIBERATELY NOT THE MAIN THREAD, and that is the whole point.
 * This guest previously opened in main(), where tid == tgid == the virtualized
 * pid; those three coincide, so substituting any of them for the opener's tid
 * was unobservable. Measured 2026-08-07 at hermit 294e89bfe: with the main
 * thread as opener, mutating the sanitizer to report the tgid, and separately
 * to report the virtual pid, BOTH left this guest green -- the regression could
 * not fail. With a non-main opener the three identities are distinct and each
 * substitution is caught.
 *
 * Distinct exit codes so a failure names the broken invariant instead of only
 * being non-zero.
 */

#define _GNU_SOURCE

#include <fcntl.h>
#include <pthread.h>
#include <semaphore.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

enum {
  RESULT_OK = 0,
  RESULT_READ_FAILED = 1,
  RESULT_IDENTITY_MISMATCH = 2,
  RESULT_NO_PID_LINE = 3,
  RESULT_OPEN_FAILED = 4,
  /* The opener's tid coincided with the tgid or with the reader's tid, so this
   * run could not have distinguished a substitution. Fail loudly rather than
   * report a pass the run did not earn. */
  RESULT_IDENTITIES_NOT_DISTINCT = 5,
};

struct handoff {
  int fd;
  long opener_tid;
  sem_t opened;   /* opener -> main: fd is ready */
  sem_t may_exit; /* main -> opener: reader is done, you may exit */
};

/*
 * Opens on a NON-MAIN thread, so opener_tid != tgid, and then STAYS ALIVE until
 * the reader is finished. That wait is load-bearing, not politeness: once a
 * thread exits, reading its procfs entry fails, so joining the opener first
 * turns this into an error-path test that never reaches the identity check.
 * Measured while writing this: joining the opener first exited 1
 * (RESULT_READ_FAILED) NATIVELY as well as under hermit, which is what
 * distinguished "my test is wrong" from "hermit is wrong".
 */
static void *open_status(void *opaque) {
  struct handoff *handoff = opaque;
  handoff->opener_tid = syscall(SYS_gettid);
  handoff->fd = open("/proc/thread-self/status", O_RDONLY | O_CLOEXEC);
  sem_post(&handoff->opened);
  sem_wait(&handoff->may_exit);
  return NULL;
}

static void *read_status(void *opaque) {
  struct handoff *handoff = opaque;
  long reader_tid = syscall(SYS_gettid);
  long tgid = (long)getpid();

  /* Guard this test's discriminating power inside the run itself: if the three
   * identities are not distinct, a substitution bug would be invisible here. */
  if (handoff->opener_tid == tgid || handoff->opener_tid == reader_tid) {
    return (void *)(intptr_t)RESULT_IDENTITIES_NOT_DISTINCT;
  }

  char buffer[8192];
  ssize_t count = read(handoff->fd, buffer, sizeof(buffer) - 1);
  if (count < 0) {
    return (void *)(intptr_t)RESULT_READ_FAILED;
  }
  buffer[count] = '\0';

  char *save = NULL;
  for (char *line = strtok_r(buffer, "\n", &save); line != NULL;
       line = strtok_r(NULL, "\n", &save)) {
    if (strncmp(line, "Pid:", 4) == 0) {
      long observed = strtol(line + 4, NULL, 10);
      return (void *)(intptr_t)(observed == handoff->opener_tid
                                    ? RESULT_OK
                                    : RESULT_IDENTITY_MISMATCH);
    }
  }
  return (void *)(intptr_t)RESULT_NO_PID_LINE;
}

int main(void) {
  struct handoff handoff = {.fd = -1, .opener_tid = -1};
  if (sem_init(&handoff.opened, 0, 0) != 0 ||
      sem_init(&handoff.may_exit, 0, 0) != 0) {
    return EXIT_FAILURE;
  }

  pthread_t opener;
  if (pthread_create(&opener, NULL, open_status, &handoff) != 0) {
    return EXIT_FAILURE;
  }
  sem_wait(&handoff.opened);
  if (handoff.fd < 0) {
    sem_post(&handoff.may_exit);
    pthread_join(opener, NULL);
    return RESULT_OPEN_FAILED;
  }

  /* The opener is still alive here; the reader is a third thread. */
  pthread_t reader;
  if (pthread_create(&reader, NULL, read_status, &handoff) != 0) {
    sem_post(&handoff.may_exit);
    pthread_join(opener, NULL);
    return EXIT_FAILURE;
  }
  void *result = NULL;
  int joined = pthread_join(reader, &result);
  sem_post(&handoff.may_exit);
  if (pthread_join(opener, NULL) != 0 || joined != 0 ||
      close(handoff.fd) != 0) {
    return EXIT_FAILURE;
  }
  if ((intptr_t)result != RESULT_OK) {
    return (int)(intptr_t)result;
  }
  puts("thread-self opener identity preserved");
  return EXIT_SUCCESS;
}
