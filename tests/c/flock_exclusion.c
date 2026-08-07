/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * flock(2) probes for hermit-cli/tests/flock_exclusion.rs.
 *
 * Scenario is argv[1]; "exclusion" when absent. Every scenario prints a
 * scenario-specific `...-ok` marker on success and a line beginning `FAIL` on
 * failure, and exits 0 / 1 accordingly, so the Rust driver can distinguish a
 * product failure from a harness failure (exit 2).
 *
 *   exclusion  Mutual exclusion, the property the pre-#1742 no-op destroyed:
 *              a held LOCK_EX must exclude a second open file description in
 *              the same process AND a second process, and must become
 *              available again after LOCK_UN. Matches native Linux, so it is
 *              also meaningful under --verify and under record/replay.
 *
 *   upgrade    A CONTENDED BLOCKING LOCK_SH -> LOCK_EX conversion. Detcore
 *              cannot park a thread on a file lock deterministically, so it
 *              refuses; the point of this scenario is that the refusal must
 *              leave the caller's shared lock intact. Linux converts a lock
 *              non-atomically (the old lock is deleted before the conflict
 *              scan), so a naive LOCK_NB probe silently destroys it. A second
 *              process reports, through a fresh open file description, whether
 *              the parent's shared lock survived. HERMIT ONLY: natively the
 *              blocking conversion sleeps and this scenario deadlocks by
 *              construction, which is exactly why Detcore refuses instead.
 *
 *   holder     Take LOCK_EX|LOCK_NB, print, release. Deliberately minimal, so
 *              a record/replay driver can tell "replay re-took the kernel
 *              lock" from "replay only replayed the return value".
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/wait.h>
#include <unistd.h>

/* Harness failure: something unrelated to flock semantics broke. */
#define HARNESS_FAILURE 2

/* argv[2], when present, overrides the lock file path. Hermit replaces guest
 * /tmp with a per-run isolated directory, so a driver that needs the host and
 * the guest to contend for ONE inode -- the record/replay side-effect probe --
 * must name a path outside /tmp. */
static const char *lock_override = NULL;

static const char *lock_path(const char *scenario) {
  static char buffer[512];
  if (lock_override != NULL) {
    return lock_override;
  }
  snprintf(buffer, sizeof(buffer), "/tmp/hermit-flock-%s.lock", scenario);
  return buffer;
}

static int open_lock(const char *path) {
  return open(path, O_CREAT | O_RDWR, 0600);
}

static int scenario_exclusion(void) {
  const char *path = lock_path("exclusion");
  int first = open_lock(path);
  int second = open_lock(path);
  if (first < 0 || second < 0) {
    printf("FAIL: could not open %s (errno=%d)\n", path, errno);
    return HARNESS_FAILURE;
  }

  if (flock(first, LOCK_EX | LOCK_NB) != 0) {
    printf("FAIL: LOCK_EX on an unlocked file was refused (errno=%d)\n", errno);
    return 1;
  }
  printf("flock-first-holder-acquired\n");

  /* A second open of the same file is an independent flock contender, even
   * inside one process. */
  errno = 0;
  if (flock(second, LOCK_EX | LOCK_NB) == 0) {
    printf("FAIL: a second open file description acquired a held LOCK_EX\n");
    return 1;
  }
  if (errno != EWOULDBLOCK) {
    printf("FAIL: second open file description got errno=%d, wanted EWOULDBLOCK\n",
           errno);
    return 1;
  }
  printf("flock-second-open-excluded\n");

  fflush(stdout);
  pid_t child = fork();
  if (child < 0) {
    printf("FAIL: fork failed (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  if (child == 0) {
    int contender = open_lock(path);
    if (contender < 0) {
      _exit(HARNESS_FAILURE);
    }
    errno = 0;
    int taken = flock(contender, LOCK_EX | LOCK_NB);
    _exit((taken == -1 && errno == EWOULDBLOCK) ? 0 : 1);
  }
  int status = 0;
  if (waitpid(child, &status, 0) < 0) {
    printf("FAIL: waitpid failed (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  if (!WIFEXITED(status)) {
    printf("FAIL: contending process did not exit normally (status=%d)\n", status);
    return HARNESS_FAILURE;
  }
  if (WEXITSTATUS(status) == HARNESS_FAILURE) {
    printf("FAIL: contending process could not open %s\n", path);
    return HARNESS_FAILURE;
  }
  if (WEXITSTATUS(status) != 0) {
    printf("FAIL: a second process acquired a LOCK_EX this process holds\n");
    return 1;
  }
  printf("flock-second-process-excluded\n");

  if (flock(first, LOCK_UN) != 0) {
    printf("FAIL: LOCK_UN was refused (errno=%d)\n", errno);
    return 1;
  }
  if (flock(second, LOCK_EX | LOCK_NB) != 0) {
    printf("FAIL: LOCK_EX after LOCK_UN was refused (errno=%d)\n", errno);
    return 1;
  }
  printf("flock-released-and-reacquired\n");
  printf("flock-exclusion-ok\n");
  close(first);
  close(second);
  return 0;
}

static int scenario_upgrade(void) {
  const char *path = lock_path("upgrade");
  int held = open_lock(path);
  if (held < 0) {
    printf("FAIL: could not open %s (errno=%d)\n", path, errno);
    return HARNESS_FAILURE;
  }
  if (flock(held, LOCK_SH | LOCK_NB) != 0) {
    printf("FAIL: LOCK_SH on an unlocked file was refused (errno=%d)\n", errno);
    return 1;
  }
  printf("flock-upgrade-parent-holds-shared\n");

  int to_child[2];
  int to_parent[2];
  if (pipe(to_child) != 0 || pipe(to_parent) != 0) {
    printf("FAIL: pipe failed (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }

  fflush(stdout);
  pid_t child = fork();
  if (child < 0) {
    printf("FAIL: fork failed (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  if (child == 0) {
    close(to_child[1]);
    close(to_parent[0]);
    /* 'H': the parent's shared lock survived its refused upgrade.
     * 'L': it was destroyed.  '?': the probe could not be taken. */
    char verdict = '?';
    int shared = open_lock(path);
    if (shared >= 0 && flock(shared, LOCK_SH | LOCK_NB) == 0) {
      char ready = 's';
      ssize_t sent = write(to_parent[1], &ready, 1);
      char go = 0;
      ssize_t got = (sent == 1) ? read(to_child[0], &go, 1) : -1;
      if (got == 1) {
        /* Drop this process's own contribution so the probe below measures
         * ONLY whether the parent still holds its shared lock. */
        flock(shared, LOCK_UN);
        close(shared);
        int probe = open_lock(path);
        if (probe >= 0) {
          errno = 0;
          if (flock(probe, LOCK_EX | LOCK_NB) == 0) {
            verdict = 'L';
          } else if (errno == EWOULDBLOCK) {
            verdict = 'H';
          }
          close(probe);
        }
      }
    }
    ssize_t reported = write(to_parent[1], &verdict, 1);
    _exit(reported == 1 ? 0 : HARNESS_FAILURE);
  }

  close(to_child[0]);
  close(to_parent[1]);
  char ready = 0;
  if (read(to_parent[0], &ready, 1) != 1 || ready != 's') {
    printf("FAIL: contending process never reported its shared lock\n");
    return HARNESS_FAILURE;
  }
  printf("flock-upgrade-child-holds-shared\n");

  /* The contended blocking conversion. Natively this sleeps forever here (the
   * child only releases after we signal), which is the deadlock Detcore
   * refuses rather than reproduces. */
  errno = 0;
  int upgraded = flock(held, LOCK_EX);
  int upgrade_errno = errno;
  if (upgraded == 0) {
    printf("FAIL: a contended blocking LOCK_EX upgrade reported success\n");
    return 1;
  }
  printf("flock-upgrade-refused errno=%d\n", upgrade_errno);

  char go = 'g';
  if (write(to_child[1], &go, 1) != 1) {
    printf("FAIL: could not release the contending process (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  char verdict = 0;
  if (read(to_parent[0], &verdict, 1) != 1) {
    printf("FAIL: contending process never reported a verdict\n");
    return HARNESS_FAILURE;
  }
  int status = 0;
  waitpid(child, &status, 0);

  if (verdict == 'H') {
    printf("flock-upgrade-preserved-shared-lock\n");
    printf("flock-upgrade-ok\n");
    return 0;
  }
  if (verdict == 'L') {
    printf("FAIL: the refused upgrade destroyed this process's shared lock\n");
    return 1;
  }
  printf("FAIL: the survival probe was inconclusive (verdict=%d)\n", verdict);
  return HARNESS_FAILURE;
}

static int scenario_holder(void) {
  const char *path = lock_path("holder");
  int fd = open_lock(path);
  if (fd < 0) {
    printf("FAIL: could not open %s (errno=%d)\n", path, errno);
    return HARNESS_FAILURE;
  }
  errno = 0;
  if (flock(fd, LOCK_EX | LOCK_NB) != 0) {
    printf("FAIL: LOCK_EX was refused (errno=%d)\n", errno);
    return 1;
  }
  printf("flock-holder-acquired\n");
  if (flock(fd, LOCK_UN) != 0) {
    printf("FAIL: LOCK_UN was refused (errno=%d)\n", errno);
    return 1;
  }
  printf("flock-holder-ok\n");
  close(fd);
  return 0;
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IOLBF, 0);
  const char *scenario = (argc > 1) ? argv[1] : "exclusion";
  if (argc > 2) {
    lock_override = argv[2];
  }
  if (strcmp(scenario, "exclusion") == 0) {
    return scenario_exclusion();
  }
  if (strcmp(scenario, "upgrade") == 0) {
    return scenario_upgrade();
  }
  if (strcmp(scenario, "holder") == 0) {
    return scenario_holder();
  }
  printf("FAIL: unknown scenario %s\n", scenario);
  return HARNESS_FAILURE;
}
