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
 *   exclusion  Mutual exclusion, the property the pre-#2373 no-op destroyed:
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
 *              scan), so a naive LOCK_NB probe silently destroys it. A fresh
 *              open file description reports whether the original shared lock
 *              survived. HERMIT ONLY: natively the
 *              blocking conversion sleeps and this scenario deadlocks by
 *              construction, which is exactly why Detcore refuses instead.
 *
 *   received   Receive an already-locked open file description through
 *              SCM_RIGHTS, then attempt a contended blocking upgrade. Hermit
 *              did not observe the original lock acquisition, so it must
 *              refuse before a destructive LOCK_NB probe rather than drop a
 *              lock it cannot restore.
 *
 *   fork-unlock  Let a child release an inherited lock, then prove the parent
 *              does not restore that released lock from stale copied state.
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
#include <sys/socket.h>
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


static int send_fd(int socket_fd, int fd) {
  char payload = 'f';
  struct iovec iov = {.iov_base = &payload, .iov_len = sizeof(payload)};
  char control[CMSG_SPACE(sizeof(fd))];
  memset(control, 0, sizeof(control));
  struct msghdr message = {
      .msg_iov = &iov,
      .msg_iovlen = 1,
      .msg_control = control,
      .msg_controllen = sizeof(control),
  };
  struct cmsghdr *header = CMSG_FIRSTHDR(&message);
  header->cmsg_level = SOL_SOCKET;
  header->cmsg_type = SCM_RIGHTS;
  header->cmsg_len = CMSG_LEN(sizeof(fd));
  memcpy(CMSG_DATA(header), &fd, sizeof(fd));
  return sendmsg(socket_fd, &message, 0) == 1 ? 0 : -1;
}

static int receive_fd(int socket_fd) {
  char payload = 0;
  struct iovec iov = {.iov_base = &payload, .iov_len = sizeof(payload)};
  char control[CMSG_SPACE(sizeof(int))];
  memset(control, 0, sizeof(control));
  struct msghdr message = {
      .msg_iov = &iov,
      .msg_iovlen = 1,
      .msg_control = control,
      .msg_controllen = sizeof(control),
  };
  if (recvmsg(socket_fd, &message, 0) != 1) {
    return -1;
  }
  struct cmsghdr *header = CMSG_FIRSTHDR(&message);
  if (header == NULL || header->cmsg_level != SOL_SOCKET ||
      header->cmsg_type != SCM_RIGHTS ||
      header->cmsg_len != CMSG_LEN(sizeof(int))) {
    errno = EBADMSG;
    return -1;
  }
  int fd = -1;
  memcpy(&fd, CMSG_DATA(header), sizeof(fd));
  return fd;
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
  int contender = open_lock(path);
  if (held < 0 || contender < 0) {
    printf("FAIL: could not open %s (errno=%d)\n", path, errno);
    return HARNESS_FAILURE;
  }
  if (flock(held, LOCK_SH | LOCK_NB) != 0) {
    printf("FAIL: LOCK_SH on an unlocked file was refused (errno=%d)\n", errno);
    return 1;
  }
  printf("flock-upgrade-parent-holds-shared\n");
  if (flock(contender, LOCK_SH | LOCK_NB) != 0) {
    printf("FAIL: second LOCK_SH was refused (errno=%d)\n", errno);
    return 1;
  }
  printf("flock-upgrade-contender-holds-shared\n");

  /* The contended blocking conversion. Natively this sleeps forever here,
   * which is the deadlock Detcore refuses rather than reproduces. Both locks
   * are in one process so Detcore still knows the first descriptor's mode and
   * must exercise the restore path after its substituted LOCK_NB probe. */
  errno = 0;
  int upgraded = flock(held, LOCK_EX);
  int upgrade_errno = errno;
  if (upgraded == 0) {
    printf("FAIL: a contended blocking LOCK_EX upgrade reported success\n");
    return 1;
  }
  printf("flock-upgrade-refused errno=%d\n", upgrade_errno);

  flock(contender, LOCK_UN);
  close(contender);
  int probe = open_lock(path);
  if (probe < 0) {
    return HARNESS_FAILURE;
  }
  errno = 0;
  if (flock(probe, LOCK_EX | LOCK_NB) == 0) {
    printf("FAIL: the refused upgrade destroyed this process's shared lock\n");
    return 1;
  }
  if (errno != EWOULDBLOCK) {
    printf("FAIL: shared-lock survival probe got errno=%d\n", errno);
    return 1;
  }
  printf("flock-upgrade-preserved-shared-lock\n");
  printf("flock-upgrade-ok\n");
  close(probe);
  close(held);
  return 0;
}

static int scenario_received(void) {
  const char *path = lock_path("received");
  int holder = open_lock(path);
  if (holder < 0 || flock(holder, LOCK_SH | LOCK_NB) != 0) {
    printf("FAIL: could not establish the shared lock to transfer (errno=%d)\n",
           errno);
    return HARNESS_FAILURE;
  }

  int sockets[2];
  if (socketpair(AF_UNIX, SOCK_DGRAM, 0, sockets) != 0) {
    printf("FAIL: socketpair failed (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }

  fflush(stdout);
  pid_t child = fork();
  if (child < 0) {
    printf("FAIL: fork failed (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  if (child == 0) {
    close(sockets[0]);
    close(holder);
    int received = receive_fd(sockets[1]);
    if (received < 0) {
      _exit(HARNESS_FAILURE);
    }
    errno = 0;
    if (flock(received, 0) != -1 || errno != EINVAL) {
      printf("FAIL: invalid flock operation on received fd did not return EINVAL (errno=%d)\n",
             errno);
      _exit(1);
    }

    int contender = open_lock(path);
    if (contender < 0 || flock(contender, LOCK_SH | LOCK_NB) != 0) {
      _exit(HARNESS_FAILURE);
    }

    errno = 0;
    int upgraded = flock(received, LOCK_EX);
    int upgrade_errno = errno;
    if (upgraded == 0 || upgrade_errno != ENOLCK) {
      printf("FAIL: blocking upgrade on received fd returned %d errno=%d\n",
             upgraded, upgrade_errno);
      _exit(1);
    }
    printf("flock-received-upgrade-refused errno=%d\n", upgrade_errno);

    flock(contender, LOCK_UN);
    close(contender);
    int probe = open_lock(path);
    if (probe < 0) {
      _exit(HARNESS_FAILURE);
    }
    errno = 0;
    if (flock(probe, LOCK_EX | LOCK_NB) == 0) {
      printf("FAIL: probing an unknown received fd destroyed its shared lock\n");
      _exit(1);
    }
    if (errno != EWOULDBLOCK) {
      printf("FAIL: received-fd survival probe got errno=%d\n", errno);
      _exit(1);
    }
    printf("flock-received-upgrade-preserved-shared-lock\n");
    printf("flock-received-upgrade-ok\n");
    _exit(0);
  }

  close(sockets[1]);
  if (send_fd(sockets[0], holder) != 0) {
    printf("FAIL: sendmsg could not transfer the locked fd (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  close(sockets[0]);

  int status = 0;
  if (waitpid(child, &status, 0) < 0 || !WIFEXITED(status)) {
    printf("FAIL: received-fd child did not exit normally (status=%d errno=%d)\n",
           status, errno);
    return HARNESS_FAILURE;
  }
  close(holder);
  return WEXITSTATUS(status);
}

static int scenario_fork_unlock(void) {
  const char *path = lock_path("fork-unlock");
  int inherited = open_lock(path);
  if (inherited < 0 || flock(inherited, LOCK_SH | LOCK_NB) != 0) {
    printf("FAIL: could not establish inherited shared lock (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }

  fflush(stdout);
  pid_t child = fork();
  if (child < 0) {
    printf("FAIL: fork failed (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  if (child == 0) {
    _exit(flock(inherited, LOCK_UN) == 0 ? 0 : HARNESS_FAILURE);
  }
  int status = 0;
  if (waitpid(child, &status, 0) < 0 || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    printf("FAIL: child could not release inherited lock (status=%d errno=%d)\n",
           status, errno);
    return HARNESS_FAILURE;
  }
  printf("flock-fork-child-released-inherited-lock\n");

  int contender = open_lock(path);
  if (contender < 0 || flock(contender, LOCK_SH | LOCK_NB) != 0) {
    printf("FAIL: could not establish post-fork contender (errno=%d)\n", errno);
    return HARNESS_FAILURE;
  }
  errno = 0;
  if (flock(inherited, LOCK_EX) != -1 || errno != ENOLCK) {
    printf("FAIL: post-fork blocking upgrade did not return ENOLCK (errno=%d)\n",
           errno);
    return 1;
  }
  printf("flock-fork-parent-upgrade-refused errno=%d\n", errno);

  flock(contender, LOCK_UN);
  close(contender);
  int probe = open_lock(path);
  if (probe < 0) {
    return HARNESS_FAILURE;
  }
  errno = 0;
  if (flock(probe, LOCK_EX | LOCK_NB) != 0) {
    printf("FAIL: stale parent state restored a lock released by the child (errno=%d)\n",
           errno);
    return 1;
  }
  printf("flock-fork-release-remained-unlocked\n");
  printf("flock-fork-unlock-ok\n");
  close(probe);
  close(inherited);
  return 0;
}

static int scenario_holder(void) {
  const char *path = lock_path("holder");
  int fd = lock_override != NULL ? open(path, O_RDONLY) : open_lock(path);
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
  if (strcmp(scenario, "received") == 0) {
    return scenario_received();
  }
  if (strcmp(scenario, "fork-unlock") == 0) {
    return scenario_fork_unlock();
  }
  if (strcmp(scenario, "holder") == 0) {
    return scenario_holder();
  }
  printf("FAIL: unknown scenario %s\n", scenario);
  return HARNESS_FAILURE;
}
