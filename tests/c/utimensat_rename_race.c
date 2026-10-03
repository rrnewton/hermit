/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// One thread sets an explicit mtime on DIR/target through utimensat while
// another keeps renaming prepared files over that name. Each prepared file also
// has a permanent hard link, DIR/keep_N, and the guest finally reports which of
// them it sees with the explicit mtime. Run without thread sequentialization,
// the report must agree with the files' real mtimes: Hermit's virtual mtime may
// only change on the file the kernel actually updated, never on the file that
// took over the name between the call and Hermit's own lookup of it. A final
// call after the renames end must reach the last file, so the report always
// lists it as seen.
//
// Usage: utimensat_rename_race DIR

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

// 2001-09-09T01:46:40Z, far from any file's creation time.
#define EXPLICIT 1000000000L
// The replacer makes one rename per step and the toucher makes few calls, so
// most files hold the name only briefly and never get the explicit mtime for
// real; a misplaced virtual update to one of them stays visible.
#define CALLS 50
#define FILES 1000

static const char* dir;
static char target[4096];
static atomic_int done;
static atomic_int replacements;

static void path_of(char* out, const char* name, int n) {
  snprintf(out, 4096, "%s/%s_%d", dir, name, n);
}

static void create(const char* path) {
  int fd = open(path, O_WRONLY | O_CREAT | O_EXCL, 0644);
  if (fd < 0 || write(fd, "x", 1) != 1 || close(fd) != 0) {
    perror(path);
    exit(2);
  }
}

static void* replace(void* arg) {
  (void)arg;
  char fresh[4096];
  for (int n = 0; n < FILES && !atomic_load(&done); n++) {
    path_of(fresh, "fresh", n);
    if (rename(fresh, target) != 0) {
      perror(fresh);
      exit(2);
    }
    atomic_store(&replacements, n + 1);
  }
  return NULL;
}

int main(int argc, char** argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s DIR\n", argv[0]);
    return 2;
  }
  dir = argv[1];
  snprintf(target, sizeof(target), "%s/target", dir);
  create(target);
  char fresh[4096], keep[4096];
  for (int n = 0; n < FILES; n++) {
    path_of(fresh, "fresh", n);
    path_of(keep, "keep", n);
    create(fresh);
    if (link(fresh, keep) != 0) {
      perror(keep);
      return 2;
    }
  }

  pthread_t replacer;
  if (pthread_create(&replacer, NULL, replace, NULL) != 0) {
    perror("pthread_create");
    return 2;
  }
  // The target always exists, since rename replaces it atomically, so every
  // call must succeed.
  const struct timespec times[2] = {
      {.tv_sec = 0, .tv_nsec = UTIME_OMIT},
      {.tv_sec = EXPLICIT, .tv_nsec = 0},
  };
  // Start once the replacer is running, so the calls overlap its renames.
  while (atomic_load(&replacements) == 0) {
  }
  for (int call = 0; call < CALLS; call++) {
    if (utimensat(AT_FDCWD, target, times, 0) != 0) {
      fprintf(stderr, "utimensat(%s): %s\n", target, strerror(errno));
      return 1;
    }
  }
  atomic_store(&done, 1);
  pthread_join(replacer, NULL);
  // With nothing racing it, this call must reach the virtual mtime of the file
  // that now holds the name, the last one renamed over it.
  if (utimensat(AT_FDCWD, target, times, 0) != 0) {
    fprintf(stderr, "utimensat(%s): %s\n", target, strerror(errno));
    return 1;
  }

  for (int n = 0; n < atomic_load(&replacements); n++) {
    path_of(keep, "keep", n);
    struct stat st;
    if (stat(keep, &st) != 0) {
      perror(keep);
      return 2;
    }
    printf("keep_%d %d\n", n, st.st_mtim.tv_sec == EXPLICIT);
  }
  fprintf(stderr, "%d utimensat calls, %d replacements\n", CALLS, atomic_load(&replacements));
  return 0;
}
