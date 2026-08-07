/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * AUTONOMOUS-BOT-IMPLEMENTED
 * TODO-HUMAN-REVIEW(PR-973): Review positioned/copy procfs bypass coverage.
 *
 * Exercises procfs snapshot lifecycle and offset-based read mediation:
 *
 *   1. One open /proc/self/maps fd must refresh after mmap, mprotect and
 *      munmap when rewound, preserving Linux's live address-space view.
 *   2. pread64(/proc/self/stat) must return the *sanitized* snapshot, so the
 *      volatile "starttime" field (proc stat field 22) reads back as 0 rather
 *      than a live, run-varying kernel value.
 *   3. sendfile() with a procfs input must be refused with ENOSYS so callers
 *      fall back to the mediated read()/write() path.
 *
 * The probe prints only fixed strings, so a --strict --verify run is bitwise
 * identical across the two executions.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/sendfile.h>
#include <sys/mman.h>
#include <unistd.h>

#define MAPS_CAPACITY (1024 * 1024)

static char maps_text[MAPS_CAPACITY];

static int reread_maps(int fd) {
  if (lseek(fd, 0, SEEK_SET) != 0) {
    perror("lseek /proc/self/maps");
    return -1;
  }

  size_t used = 0;
  while (used + 1 < sizeof(maps_text)) {
    ssize_t n = read(fd, maps_text + used, sizeof(maps_text) - used - 1);
    if (n < 0 && errno == EINTR) {
      continue;
    }
    if (n < 0) {
      perror("read /proc/self/maps");
      return -1;
    }
    if (n == 0) {
      maps_text[used] = '\0';
      return (int)used;
    }
    used += (size_t)n;
  }

  fprintf(stderr, "/proc/self/maps exceeded probe capacity\n");
  return -1;
}

static int mapping_permissions(uintptr_t address, char permissions[5]) {
  const char *line = maps_text;
  while (*line != '\0') {
    unsigned long start = 0;
    unsigned long end = 0;
    char current[5] = {0};
    if (sscanf(line, "%lx-%lx %4s", &start, &end, current) == 3 &&
        address >= (uintptr_t)start && address < (uintptr_t)end) {
      memcpy(permissions, current, sizeof(current));
      return 1;
    }
    const char *newline = strchr(line, '\n');
    if (newline == NULL) {
      break;
    }
    line = newline + 1;
  }
  return 0;
}

static int check_maps_lifecycle(void) {
  int fd = open("/proc/self/maps", O_RDONLY);
  if (fd < 0) {
    perror("open /proc/self/maps");
    return 1;
  }
  if (reread_maps(fd) <= 0) {
    close(fd);
    return 1;
  }

  long page_size = sysconf(_SC_PAGESIZE);
  if (page_size <= 0) {
    perror("sysconf(_SC_PAGESIZE)");
    close(fd);
    return 1;
  }
  void *mapping = mmap(NULL, (size_t)page_size, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (mapping == MAP_FAILED) {
    perror("mmap");
    close(fd);
    return 1;
  }

  char permissions[5] = {0};
  if (reread_maps(fd) <= 0 ||
      !mapping_permissions((uintptr_t)mapping, permissions) ||
      strcmp(permissions, "rw-p") != 0) {
    fprintf(stderr, "maps did not refresh after mmap (perms=%s)\n", permissions);
    munmap(mapping, (size_t)page_size);
    close(fd);
    return 1;
  }
  puts("procfs-maps-live-mmap-ok");

  if (mprotect(mapping, (size_t)page_size, PROT_READ) != 0) {
    perror("mprotect");
    munmap(mapping, (size_t)page_size);
    close(fd);
    return 1;
  }
  memset(permissions, 0, sizeof(permissions));
  if (reread_maps(fd) <= 0 ||
      !mapping_permissions((uintptr_t)mapping, permissions) ||
      strcmp(permissions, "r--p") != 0) {
    fprintf(stderr, "maps did not refresh after mprotect (perms=%s)\n",
            permissions);
    munmap(mapping, (size_t)page_size);
    close(fd);
    return 1;
  }
  puts("procfs-maps-live-mprotect-ok");

  if (munmap(mapping, (size_t)page_size) != 0) {
    perror("munmap");
    close(fd);
    return 1;
  }
  memset(permissions, 0, sizeof(permissions));
  if (reread_maps(fd) <= 0 ||
      mapping_permissions((uintptr_t)mapping, permissions)) {
    fprintf(stderr, "maps retained an unmapped address\n");
    close(fd);
    return 1;
  }
  puts("procfs-maps-live-munmap-ok");
  close(fd);
  return 0;
}

/* Returns the 1-based proc stat field `index` (>= 3), or NULL on parse error.
 * The comm field (2) may contain spaces and parentheses, so scan past the
 * final ") " terminator and split the remainder on whitespace. */
static char *stat_field(char *buf, int index) {
  char *comm_end = strstr(buf, ") ");
  if (comm_end == NULL || index < 3) {
    return NULL;
  }
  char *rest = comm_end + 2;
  char *save = NULL;
  char *tok = strtok_r(rest, " \t\n", &save);
  for (int field = 3; tok != NULL; field++) {
    if (field == index) {
      return tok;
    }
    tok = strtok_r(NULL, " \t\n", &save);
  }
  return NULL;
}

static int check_pread_sanitized(void) {
  int fd = open("/proc/self/stat", O_RDONLY);
  if (fd < 0) {
    perror("open /proc/self/stat");
    return 1;
  }

  char buf[8192];
  ssize_t n = pread(fd, buf, sizeof(buf) - 1, 0);
  if (n <= 0) {
    fprintf(stderr, "pread /proc/self/stat: n=%zd errno=%d\n", n, errno);
    close(fd);
    return 1;
  }
  buf[n] = '\0';
  close(fd);

  /* Field 22 (starttime) is normalized to 0 by the snapshot sanitizer. Live
   * kernel bytes would contain a nonzero, run-varying tick count. */
  char *starttime = stat_field(buf, 22);
  if (starttime == NULL) {
    fprintf(stderr, "could not locate stat field 22 in: %s\n", buf);
    return 1;
  }
  if (strcmp(starttime, "0") != 0) {
    fprintf(stderr, "pread bypassed procfs snapshot: starttime=%s\n", starttime);
    return 1;
  }

  puts("procfs-pread-sanitized-ok");
  return 0;
}

static int check_sendfile_refused(void) {
  int in_fd = open("/proc/self/stat", O_RDONLY);
  int out_fd = open("/tmp/hermit-procfs-sendfile-destination",
                    O_WRONLY | O_CREAT | O_TRUNC, 0600);
  if (in_fd < 0 || out_fd < 0) {
    perror("open sendfile endpoints");
    return 1;
  }

  errno = 0;
  ssize_t copied = sendfile(out_fd, in_fd, NULL, 4096);
  int saved = errno;
  close(in_fd);
  close(out_fd);

  if (copied == -1 && saved == ENOSYS) {
    puts("procfs-sendfile-refused-ok");
    return 0;
  }

  fprintf(stderr, "sendfile: expected ENOSYS, got copied=%zd errno=%d\n", copied,
          saved);
  return 1;
}

int main(void) {
  if (check_maps_lifecycle() != 0) {
    return 1;
  }
  if (check_pread_sanitized() != 0) {
    return 1;
  }
  if (check_sendfile_refused() != 0) {
    return 1;
  }
  return 0;
}
