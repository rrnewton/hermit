/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Guest for hermit-cli/tests/fatal_core_capture.rs.
 *
 *   segv          dirty 1 MiB of heap, put MARKER at its start, segfault
 *   children MIB...  fork one child per argument, one at a time, that
 *                 dirties that many MiB of incompressible heap and segfaults;
 *                 exit 1
 *   child-segv-ok fork one child that segfaults; exit 0
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#define MARKER "HERMIT-FATAL-CORE-MARKER"

/* Volatile so that no store to the heap is dropped as dead. */
static unsigned char* volatile heap;

static void dirty_and_segfault(size_t mib, unsigned seed) {
  size_t len = mib << 20;
  unsigned x = seed * 2654435761u + 1;
  heap = malloc(len);
  if (heap == NULL) {
    perror("malloc");
    exit(3);
  }
  for (size_t i = 0; i < len; i++) {
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    heap[i] = (unsigned char)x;
  }
  memcpy((void*)heap, MARKER, sizeof(MARKER));
  *(volatile int*)0 = 1;
}

static void fork_segfaulting_children(int count, char** mib) {
  for (int i = 0; i < count; i++) {
    int status;
    pid_t pid = fork();
    if (pid < 0) {
      perror("fork");
      exit(3);
    }
    if (pid == 0) {
      dirty_and_segfault((size_t)atoi(mib[i]), (unsigned)i + 7);
    }
    if (waitpid(pid, &status, 0) != pid) {
      perror("waitpid");
      exit(3);
    }
    printf("child %d signal %d\n", i, WIFSIGNALED(status) ? WTERMSIG(status) : 0);
  }
}

int main(int argc, char** argv) {
  const char* mode = argc > 1 ? argv[1] : "";
  if (strcmp(mode, "segv") == 0) {
    dirty_and_segfault(1, 1);
  } else if (strcmp(mode, "children") == 0) {
    fork_segfaulting_children(argc - 2, argv + 2);
    return 1;
  } else if (strcmp(mode, "child-segv-ok") == 0) {
    char* one[] = {"1"};
    fork_segfaulting_children(1, one);
    return 0;
  }
  fprintf(stderr, "usage: %s segv | children MIB... | child-segv-ok\n", argv[0]);
  return 2;
}
