/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A process's /proc/<pid>/maps, read by the process itself and by its parent.
 *
 * The parent maps a memfd (shared), a regular file (private, from an offset)
 * and three anonymous pages whose middle page it makes inaccessible, then
 * checks its own maps: each object has the lines Linux gives it (the
 * anonymous range split in three), with the offset it was mapped from and the
 * device and inode fstat(2) gives its file. A forked child maps one more
 * page; the parent reads the child's maps while the child waits, the child
 * reads its own, and the two must be the same text, holding the child's page
 * and not the parent's later one. Finally it prints its own maps with the
 * addresses left out (each line's size, permissions, offset and name), less
 * the kernel's own lines and the stack's size, which depend on the kernel
 * and its configuration rather than on the program.
 *
 * Usage: proc_maps_other_process DIRECTORY (an empty directory for the file,
 * which the program removes before it exits).
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#define PAGE 4096
#define MAPS_MAX (256 * 1024)

static int failures;

static void check(int ok, const char* what) {
  printf("%s: %s\n", ok ? "PASS" : "FAIL", what);
  if (!ok) {
    failures++;
  }
}

static size_t read_all(const char* path, char* buffer, size_t size) {
  int fd = open(path, O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    printf("FAIL: open %s: %s\n", path, strerror(errno));
    exit(1);
  }
  size_t total = 0;
  for (;;) {
    ssize_t n = read(fd, buffer + total, size - 1 - total);
    if (n < 0) {
      printf("FAIL: read %s: %s\n", path, strerror(errno));
      exit(1);
    }
    if (n == 0) {
      break;
    }
    total += (size_t)n;
  }
  buffer[total] = '\0';
  close(fd);
  return total;
}

struct line {
  uintptr_t start, end;
  char perms[5];
  unsigned long long offset;
  unsigned major, minor;
  unsigned long inode;
  char name[512];
};

/* The maps line of `maps` that holds `address`, if any. */
static int find_line(const char* maps, uintptr_t address, struct line* out) {
  const char* cursor = maps;
  while (*cursor) {
    const char* end = strchr(cursor, '\n');
    size_t length = end ? (size_t)(end - cursor) : strlen(cursor);
    char text[1024];
    if (length >= sizeof(text)) {
      length = sizeof(text) - 1;
    }
    memcpy(text, cursor, length);
    text[length] = '\0';
    struct line line;
    int consumed = 0;
    line.name[0] = '\0';
    if (sscanf(text, "%" SCNxPTR "-%" SCNxPTR " %4s %llx %x:%x %lu %n",
               &line.start, &line.end, line.perms, &line.offset, &line.major,
               &line.minor, &line.inode, &consumed) >= 7) {
      snprintf(line.name, sizeof(line.name), "%s", text + consumed);
      if (line.start <= address && address < line.end) {
        *out = line;
        return 1;
      }
    }
    cursor = end ? end + 1 : cursor + length;
  }
  return 0;
}

static void check_object(const char* maps, const char* what, void* address,
                         size_t length, const char* perms,
                         unsigned long long offset, int fd,
                         const char* name_prefix) {
  struct line line;
  char label[1024];
  snprintf(label, sizeof(label), "%s has a maps line", what);
  if (!find_line(maps, (uintptr_t)address, &line)) {
    check(0, label);
    return;
  }
  check(1, label);
  snprintf(label, sizeof(label), "%s line spans the mapping", what);
  check(line.start == (uintptr_t)address &&
            line.end == (uintptr_t)address + length,
        label);
  snprintf(label, sizeof(label), "%s permissions %s (got %s)", what, perms,
           line.perms);
  check(strcmp(line.perms, perms) == 0, label);
  snprintf(label, sizeof(label), "%s offset %#llx (got %#llx)", what, offset,
           line.offset);
  check(line.offset == offset, label);
  if (fd >= 0) {
    /* The inode is the file's. The device is its superblock's
     * (i_sb->s_dev), which is fstat's st_dev except where the file system
     * gives a part of itself a device of its own (a btrfs subvolume), so it
     * is checked only to be a file's. Where the two devices differ, the
     * inode's agreement is printed rather than checked: Linux's always holds,
     * and a run that numbers a file by its device and inode together (as
     * Hermit's inode pool does) cannot reproduce it there. */
    struct stat st;
    fstat(fd, &st);
    if (makedev(line.major, line.minor) == st.st_dev) {
      snprintf(label, sizeof(label), "%s inode agrees with fstat", what);
      check(line.inode == st.st_ino, label);
    } else {
      printf("KNOWN: %s inode agrees with fstat, its device not fstat's: %s\n",
             what, line.inode == st.st_ino ? "yes" : "no");
    }
    snprintf(label, sizeof(label), "%s has a device", what);
    check(line.major != 0 || line.minor != 0, label);
  } else {
    snprintf(label, sizeof(label), "%s has no device or inode", what);
    check(line.major == 0 && line.minor == 0 && line.inode == 0, label);
  }
  snprintf(label, sizeof(label), "%s name begins %s (got %s)", what,
           name_prefix[0] ? name_prefix : "(none)",
           line.name[0] ? line.name : "(none)");
  check(name_prefix[0] ? strncmp(line.name, name_prefix, strlen(name_prefix)) ==
                             0
                       : line.name[0] == '\0',
        label);
}

/* Each line of `maps` without its addresses, less the kernel's own lines and
 * the stack's size. */
static void print_shape(const char* maps) {
  const char* cursor = maps;
  while (*cursor) {
    const char* end = strchr(cursor, '\n');
    size_t length = end ? (size_t)(end - cursor) : strlen(cursor);
    char text[1024];
    if (length >= sizeof(text)) {
      length = sizeof(text) - 1;
    }
    memcpy(text, cursor, length);
    text[length] = '\0';
    uintptr_t start, stop;
    char perms[5], device[16];
    unsigned long long offset;
    unsigned long inode;
    int consumed = 0;
    if (sscanf(text, "%" SCNxPTR "-%" SCNxPTR " %4s %llx %15s %lu %n", &start,
               &stop, perms, &offset, device, &inode, &consumed) >= 6) {
      const char* name = text + consumed;
      int kernel = strcmp(name, "[vdso]") == 0 || strcmp(name, "[vvar]") == 0 ||
                   strcmp(name, "[vvar_vclock]") == 0 ||
                   strcmp(name, "[vsyscall]") == 0 ||
                   strcmp(name, "[uprobes]") == 0;
      if (!kernel) {
        if (strcmp(name, "[stack]") == 0) {
          printf("MAP - %s %08llx %s\n", perms, offset, name);
        } else {
          printf("MAP %lx %s %08llx %s\n", (unsigned long)(stop - start), perms,
                 offset, name);
        }
      }
    }
    cursor = end ? end + 1 : cursor + length;
  }
}

int main(int argc, char** argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s DIRECTORY\n", argv[0]);
    return 2;
  }
  static char maps[MAPS_MAX];

  int memfd = memfd_create("maps-probe", MFD_CLOEXEC);
  if (memfd < 0 || ftruncate(memfd, 3 * PAGE) != 0) {
    printf("FAIL: memfd: %s\n", strerror(errno));
    return 1;
  }
  char* shared = mmap(NULL, 2 * PAGE, PROT_READ | PROT_WRITE, MAP_SHARED,
                      memfd, PAGE);
  char path[4096];
  snprintf(path, sizeof(path), "%s/maps-file", argv[1]);
  int file = open(path, O_RDWR | O_CREAT | O_TRUNC | O_CLOEXEC, 0600);
  static char contents[3 * PAGE];
  memset(contents, 'f', sizeof(contents));
  if (file < 0 || write(file, contents, sizeof(contents)) != sizeof(contents)) {
    printf("FAIL: file: %s\n", strerror(errno));
    return 1;
  }
  char* private_file = mmap(NULL, 2 * PAGE, PROT_READ, MAP_PRIVATE, file, PAGE);
  char* anonymous = mmap(NULL, 3 * PAGE, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (shared == MAP_FAILED || private_file == MAP_FAILED ||
      anonymous == MAP_FAILED) {
    printf("FAIL: mmap: %s\n", strerror(errno));
    return 1;
  }
  shared[0] = 's';
  anonymous[0] = 'a';
  anonymous[2 * PAGE] = 'a';
  if (mprotect(anonymous + PAGE, PAGE, PROT_NONE) != 0) {
    printf("FAIL: mprotect: %s\n", strerror(errno));
    return 1;
  }

  read_all("/proc/self/maps", maps, sizeof(maps));
  check_object(maps, "memfd view", shared, 2 * PAGE, "rw-s", PAGE, memfd,
               "/memfd:maps-probe (deleted)");
  check_object(maps, "file view", private_file, 2 * PAGE, "r--p", PAGE, file,
               "/");
  check_object(maps, "anonymous head", anonymous, PAGE, "rw-p", 0, -1, "");
  check_object(maps, "anonymous guard", anonymous + PAGE, PAGE, "---p", 0, -1,
               "");
  check_object(maps, "anonymous tail", anonymous + 2 * PAGE, PAGE, "rw-p", 0,
               -1, "");

  int ready[2], go[2], text[2];
  if (pipe(ready) != 0 || pipe(go) != 0 || pipe(text) != 0) {
    printf("FAIL: pipe: %s\n", strerror(errno));
    return 1;
  }
  pid_t child = fork();
  if (child < 0) {
    printf("FAIL: fork: %s\n", strerror(errno));
    return 1;
  }
  if (child == 0) {
    char* own = mmap(NULL, PAGE, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    char token = own == MAP_FAILED ? 'x' : 'r';
    uintptr_t where = (uintptr_t)own;
    if (write(ready[1], &where, sizeof(where)) != sizeof(where) ||
        write(ready[1], &token, 1) != 1 || read(go[0], &token, 1) != 1) {
      _exit(3);
    }
    static char mine[MAPS_MAX];
    size_t length = read_all("/proc/self/maps", mine, sizeof(mine));
    size_t sent = 0;
    while (sent < length) {
      ssize_t n = write(text[1], mine + sent, length - sent);
      if (n <= 0) {
        _exit(4);
      }
      sent += (size_t)n;
    }
    _exit(0);
  }
  close(text[1]);
  uintptr_t child_page = 0;
  char token = 0;
  if (read(ready[0], &child_page, sizeof(child_page)) != sizeof(child_page) ||
      read(ready[0], &token, 1) != 1 || token != 'r') {
    printf("FAIL: child setup\n");
    return 1;
  }
  char* later = mmap(NULL, PAGE, PROT_READ | PROT_EXEC,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  static char theirs[MAPS_MAX];
  char child_path[64];
  snprintf(child_path, sizeof(child_path), "/proc/%d/maps", (int)child);
  read_all(child_path, theirs, sizeof(theirs));
  if (write(go[1], "g", 1) != 1) {
    printf("FAIL: go\n");
    return 1;
  }
  static char own_read[MAPS_MAX];
  size_t got = 0;
  for (;;) {
    ssize_t n = read(text[0], own_read + got, sizeof(own_read) - 1 - got);
    if (n <= 0) {
      break;
    }
    got += (size_t)n;
  }
  own_read[got] = '\0';
  int status = 0;
  waitpid(child, &status, 0);
  check(WIFEXITED(status) && WEXITSTATUS(status) == 0, "child exits 0");
  check(got > 0 && strcmp(theirs, own_read) == 0,
        "the child's maps read by its parent equal its own read");
  struct line line;
  check(find_line(theirs, child_page, &line) && strcmp(line.perms, "r--p") == 0,
        "the parent sees the child's own page");
  /* The child may have its own page where the parent's later one is: the
   * two address spaces place mappings alike. */
  check(later != MAP_FAILED && (!find_line(theirs, (uintptr_t)later, &line) ||
                                strcmp(line.perms, "r-xp") != 0),
        "the child's maps lack the parent's later page");
  check(find_line(theirs, (uintptr_t)shared, &line) &&
            strcmp(line.perms, "rw-s") == 0 && line.offset == PAGE,
        "the child's maps hold the inherited memfd view");

  read_all("/proc/self/maps", maps, sizeof(maps));
  print_shape(maps);
  /* Leave the directory as it was found, so that a second run sees the same. */
  if (unlink(path) != 0) {
    check(0, "remove the file");
  }
  printf("%s\n", failures ? "FAILED" : "ALL PASS");
  return failures ? 1 : 0;
}
