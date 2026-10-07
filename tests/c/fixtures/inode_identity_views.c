// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

// ONE FILE, ONE INODE NUMBER, WHICHEVER INTERFACE REPORTS IT.
//
// Usage: inode_identity_views scm-getdents DIR
//        inode_identity_views maps-stat [DIR]
//        inode_identity_views maps-stat-full-table
//        inode_identity_views proc-fd-links
//        inode_identity_views stdio-mmap-sentinel < REGULAR-FILE
//        inode_identity_views stdio-getdents > DIR
//        inode_identity_views stdout-alias-maps PATH 1<> PATH
//        inode_identity_views stdout-pipe-links | READER
//
// Detcore keys its deterministic inodes on a raw device and inode together
// (https://github.com/rrnewton/hermit/issues/3307). Every interface that
// reports an inode must then find the SAME key for the same file, or a guest
// sees one file under two inode numbers. Each mode checks one interface
// against `stat`, and asserts only what native Linux also guarantees.
//
// scm-getdents DIR
//   Creates three files in DIR, passes a directory descriptor for DIR to
//   itself over a Unix socket (SCM_RIGHTS), and lists the directory through
//   the RECEIVED descriptor with getdents64 and, where it exists, getdents
//   (each through a descriptor of its own).
//   Detcore does not track a received descriptor, so it has no cached stat
//   for it; the listing must still succeed, and each file's d_ino must equal
//   the st_ino fstatat reports for it.
//
// maps-stat [DIR]
//   For every /proc/self/maps line that names a live file by absolute path,
//   stat(path).st_ino must equal the line's inode column, and stat of
//   /proc/self/exe must agree with the executable's line. The device columns
//   are NOT compared: on btrfs maps reports the superblock's device and stat
//   the subvolume's, on native Linux too. Lines where they differ are counted
//   as `split`, so the owning test can refuse to pass on a btrfs host without
//   having exercised that case. If stdin is a regular file it is also mapped,
//   and its line's inode must equal fstat(0).st_ino.
//   With DIR, two files are created in DIR, mapped, and unlinked, one with
//   its descriptor still open and one with it closed. Their maps lines read
//   " (deleted)", so no path leads to the file any more, and each line's inode
//   must still equal the st_ino fstat reported before the unlink. These are
//   counted as `unlinked`, and those whose device columns differ from st_dev
//   as `unlinked_split`.
//
// maps-stat-full-table
//   maps-stat without DIR, except that after opening /proc/self/maps and
//   before reading it the process opens descriptors until open fails with
//   EMFILE, as proc-fd-links does (see there for how the fill is bounded).
//   Detcore rewrites the maps snapshot on the first read, and on btrfs that
//   rewrite must prove which superblock each split line's path is on; a
//   Detcore embedded in the guest (DBT, SaBRe) must make that proof without
//   creating descriptors of its own in the guest. The executable's line is
//   compared with stat of the path readlink("/proc/self/exe") reports, not
//   with stat of the link: under DBT the link names DynamoRIO's loader, which
//   is the process's executable there, while readlink reports the program.
//
// proc-fd-links
//   A forked child reads the parent's /proc/<pid>/fd/N links for a pipe and a
//   socket. Each must name the inode the child's own fstat of the inherited
//   descriptor reports, and read the same as the child's /proc/self/fd/N.
//   The child first opens descriptors until open fails with EMFILE, so that a
//   Detcore embedded in the guest (DBT, SaBRe) must answer without creating
//   descriptors of its own in the guest. The child lowers its soft
//   RLIMIT_NOFILE first, which bounds the fill on native Linux; Hermit keeps
//   that limit virtual and does not enforce it on open, so under Hermit the
//   fill stops at the HOST limit, and the owning test lowers that before
//   starting Hermit.
//
// stdio-mmap-sentinel < REGULAR-FILE
//   Fills the bytes from 1024 to 128 below the stack pointer with a nonzero
//   pattern, maps the first page of stdin with a raw MAP_PRIVATE mmap system
//   call, and counts the 8-byte words of that range that no longer hold the
//   pattern, all in one asm block so that no compiled code uses the stack in
//   between. Linux never writes below a thread's stack pointer to serve a
//   system call, so the count must be zero. Stdin is a descriptor the guest
//   inherited, which Detcore identifies after the mmap by injecting an fstat;
//   on ptrace that fstat borrows guest stack below the red zone and must put
//   the bytes there back.
//
// stdio-getdents > DIR
//   Stdout is a directory opened for reading, on another device than stdin;
//   the owning test creates three files in it first and makes stdin
//   /dev/null. Lists the directory with getdents64 through descriptor 1 and
//   through a dup of it, taking turns. The two share one file offset, and the
//   buffer holds one record but never two, so each lists part of the
//   directory. (An lseek back to the start cannot give each a whole listing
//   instead: Hermit reports ESPIPE for its inherited stdout and stderr.)
//   Every entry's d_ino must equal the st_ino fstatat reports for its name
//   relative to the descriptor that listed it. getdents keys each entry's
//   inode on the device of the descriptor it lists, so a stdout keyed on
//   stdin's device would list inodes stat does not report. Stdout is the
//   directory, so the result goes to stderr, on a line beginning
//   "stdio-getdents ".
//
// stdout-alias-maps PATH 1<> PATH
//   Stdout is PATH, a regular file of at least one page opened for reading
//   and writing; the owning test makes stdin /dev/null, another object. Dups
//   stdout to a descriptor above 2 (the alias), maps the first page of the
//   alias, opens PATH again read-only and maps the first page of that
//   descriptor, and reads the alias's /proc/self/fdinfo. Each maps line's
//   inode and the fdinfo `ino:` line must equal the st_ino fstat of the alias
//   reports, which must equal what stat and statx of PATH report. fstat of
//   descriptor 1 itself is not compared: Hermit reports a fixed inode for an
//   inherited stdio descriptor, on purpose, where Linux reports the file's.
//   Stdout is the file, so the result goes to stderr, on a line beginning
//   "stdout-alias-maps ": one field per view, "agrees" or "differs".
//
// stdout-pipe-links | READER
//   Stdout is a pipe; the owning test makes stdin /dev/null. Dups stdout to a
//   descriptor above 2 (the alias) and forks. The child reads its own
//   /proc/self/fd link of the alias, and the parent's /proc/<pid>/fd links of
//   descriptor 1 and of the alias, the parent's each through readlink and
//   through readlinkat relative to /proc/<pid>/fd. Each must read pipe:[N],
//   where N is the st_ino the child's fstat of the alias reports. The result
//   goes to stderr, on a line beginning "stdout-pipe-links ": one field per
//   view, "agrees" or "differs".

#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#define CHECK(condition)                                                    \
  do {                                                                      \
    if (!(condition)) {                                                     \
      fprintf(                                                              \
          stderr, "line %d: %s (errno=%d)\n", __LINE__, #condition, errno); \
      exit(1);                                                              \
    }                                                                       \
  } while (0)

static const char* const kEntries[] = {"alpha", "beta", "gamma"};
#define ENTRY_COUNT ((int)(sizeof(kEntries) / sizeof(kEntries[0])))

// ---------------------------------------------------------------------------
// scm-getdents

static int receive_directory_over_scm_rights(int directory) {
  int sockets[2];
  CHECK(socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, sockets) == 0);

  char byte = 'd';
  struct iovec vector = {.iov_base = &byte, .iov_len = 1};
  union {
    struct cmsghdr header;
    char buffer[CMSG_SPACE(sizeof(int))];
  } control;
  memset(&control, 0, sizeof(control));
  struct msghdr message = {0};
  message.msg_iov = &vector;
  message.msg_iovlen = 1;
  message.msg_control = control.buffer;
  message.msg_controllen = sizeof(control.buffer);
  struct cmsghdr* header = CMSG_FIRSTHDR(&message);
  header->cmsg_level = SOL_SOCKET;
  header->cmsg_type = SCM_RIGHTS;
  header->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(header), &directory, sizeof(int));
  CHECK(sendmsg(sockets[0], &message, 0) == 1);
  // The only reference left is the one in flight.
  CHECK(close(directory) == 0);

  memset(&control, 0, sizeof(control));
  memset(&message, 0, sizeof(message));
  byte = 0;
  message.msg_iov = &vector;
  message.msg_iovlen = 1;
  message.msg_control = control.buffer;
  message.msg_controllen = sizeof(control.buffer);
  CHECK(recvmsg(sockets[1], &message, MSG_CMSG_CLOEXEC) == 1);
  CHECK(byte == 'd');
  header = CMSG_FIRSTHDR(&message);
  CHECK(header != NULL);
  CHECK(header->cmsg_level == SOL_SOCKET && header->cmsg_type == SCM_RIGHTS);
  int received = -1;
  memcpy(&received, CMSG_DATA(header), sizeof(int));
  CHECK(received >= 0);
  CHECK(close(sockets[0]) == 0);
  CHECK(close(sockets[1]) == 0);
  return received;
}

// Check one listed entry; returns 1 if it is one of the created files.
static int
check_entry(int directory, const char* name, uint64_t d_ino, const char* call) {
  for (int i = 0; i < ENTRY_COUNT; i++) {
    if (strcmp(name, kEntries[i]) != 0) {
      continue;
    }
    struct stat st;
    CHECK(fstatat(directory, name, &st, AT_SYMLINK_NOFOLLOW) == 0);
    if ((uint64_t)st.st_ino != d_ino) {
      fprintf(
          stderr,
          "%s: d_ino of %s is %llu but fstatat reports %llu\n",
          call,
          name,
          (unsigned long long)d_ino,
          (unsigned long long)st.st_ino);
      exit(1);
    }
    return 1;
  }
  return 0;
}

struct linux_dirent64_header {
  uint64_t d_ino;
  int64_t d_off;
  unsigned short d_reclen;
  unsigned char d_type;
  char d_name[];
};

static void list_with_getdents64(int directory, int* entries, int* matched) {
  char buffer[4096] __attribute__((aligned(8)));
  *entries = 0;
  *matched = 0;
  for (;;) {
    long count = syscall(SYS_getdents64, directory, buffer, sizeof(buffer));
    CHECK(count >= 0);
    if (count == 0) {
      return;
    }
    for (long offset = 0; offset < count;) {
      struct linux_dirent64_header* entry =
          (struct linux_dirent64_header*)(buffer + offset);
      (*entries)++;
      *matched +=
          check_entry(directory, entry->d_name, entry->d_ino, "getdents64");
      offset += entry->d_reclen;
    }
  }
}

#ifdef SYS_getdents
struct linux_dirent_header {
  unsigned long d_ino;
  unsigned long d_off;
  unsigned short d_reclen;
  char d_name[];
};

static void list_with_getdents(int directory, int* entries, int* matched) {
  char buffer[4096] __attribute__((aligned(8)));
  *entries = 0;
  *matched = 0;
  for (;;) {
    long count = syscall(SYS_getdents, directory, buffer, sizeof(buffer));
    CHECK(count >= 0);
    if (count == 0) {
      return;
    }
    for (long offset = 0; offset < count;) {
      struct linux_dirent_header* entry =
          (struct linux_dirent_header*)(buffer + offset);
      (*entries)++;
      *matched +=
          check_entry(directory, entry->d_name, entry->d_ino, "getdents");
      offset += entry->d_reclen;
    }
  }
}
#endif

static int scm_getdents(const char* path) {
  int creator = open(path, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  CHECK(creator >= 0);
  for (int i = 0; i < ENTRY_COUNT; i++) {
    int fd = openat(
        creator, kEntries[i], O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0644);
    CHECK(fd >= 0);
    CHECK(close(fd) == 0);
  }
  CHECK(close(creator) == 0);

  // Open each descriptor to list only after the files exist: btrfs fixes the
  // end of a directory listing when the directory is opened (and again on a
  // rewind to offset 0), so entries created through a descriptor opened
  // earlier are not listed through it, on native Linux too.
  //
  // Each call gets its own received descriptor rather than an lseek back to
  // the start: lseek on a descriptor received over SCM_RIGHTS fails with EBADF
  // under Hermit, a separate defect this mode does not test
  // (https://github.com/rrnewton/hermit/issues/3387).
  int directory = open(path, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  CHECK(directory >= 0);
  int received = receive_directory_over_scm_rights(directory);
  int entries = 0;
  int matched = 0;
  list_with_getdents64(received, &entries, &matched);
  CHECK(matched == ENTRY_COUNT);
  printf("scm getdents64 entries=%d matched=%d\n", entries, matched);
  CHECK(close(received) == 0);
#ifdef SYS_getdents
  directory = open(path, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  CHECK(directory >= 0);
  received = receive_directory_over_scm_rights(directory);
  list_with_getdents(received, &entries, &matched);
  CHECK(matched == ENTRY_COUNT);
  printf("scm getdents entries=%d matched=%d\n", entries, matched);
  CHECK(close(received) == 0);
#endif
  return 0;
}

// ---------------------------------------------------------------------------
// maps-stat

static int ends_with(const char* text, const char* suffix) {
  size_t text_length = strlen(text);
  size_t suffix_length = strlen(suffix);
  return text_length >= suffix_length &&
      strcmp(text + text_length - suffix_length, suffix) == 0;
}

// A file maps-stat maps from DIR and then unlinks, so that only the mapping
// (and, with keep_open, a descriptor) still leads to it.
struct unlinked_mapping {
  const char* name;
  int keep_open;
  uintptr_t start;
  struct stat st;
  int lines;
  int split;
};

static void map_then_unlink(
    const char* directory,
    struct unlinked_mapping* mapping) {
  char path[4096];
  CHECK(
      snprintf(path, sizeof(path), "%s/%s", directory, mapping->name) <
      (int)sizeof(path));
  // O_TRUNC rather than O_EXCL: a verified run starts the guest twice.
  int fd = open(path, O_RDWR | O_CREAT | O_TRUNC | O_CLOEXEC, 0600);
  CHECK(fd >= 0);
  CHECK(ftruncate(fd, 4096) == 0);
  CHECK(fstat(fd, &mapping->st) == 0);
  void* mapped = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, fd, 0);
  CHECK(mapped != MAP_FAILED);
  mapping->start = (uintptr_t)mapped;
  CHECK(unlink(path) == 0);
  if (!mapping->keep_open) {
    CHECK(close(fd) == 0);
  }
}

// Check the maps line [start, end) against the unlinked mapping it holds, if
// any. Returns 1 if it holds one.
static int check_unlinked_line(
    struct unlinked_mapping* mappings,
    int count,
    uintptr_t start,
    uintptr_t end,
    unsigned major,
    unsigned minor,
    unsigned long inode,
    const char* path) {
  for (int i = 0; i < count; i++) {
    struct unlinked_mapping* mapping = &mappings[i];
    if (mapping->start < start || mapping->start >= end) {
      continue;
    }
    if (!ends_with(path, " (deleted)")) {
      fprintf(
          stderr,
          "unlinked %s: maps names %s, not a deleted file\n",
          mapping->name,
          path);
      exit(1);
    }
    if ((unsigned long)mapping->st.st_ino != inode) {
      fprintf(
          stderr,
          "unlinked %s: maps inode %lu (device %x:%x), fstat inode %llu\n",
          mapping->name,
          inode,
          major,
          minor,
          (unsigned long long)mapping->st.st_ino);
      exit(1);
    }
    if (makedev(major, minor) != mapping->st.st_dev) {
      mapping->split++;
    }
    mapping->lines++;
    return 1;
  }
  return 0;
}

static int fill_descriptor_table(void);

static int maps_stat(const char* directory, int full_table) {
  struct unlinked_mapping unlinked[] = {
      {.name = "maps-stat-unlinked-open", .keep_open = 1},
      {.name = "maps-stat-unlinked-closed", .keep_open = 0},
  };
  const int unlinked_count =
      directory == NULL ? 0 : (int)(sizeof(unlinked) / sizeof(unlinked[0]));
  for (int i = 0; i < unlinked_count; i++) {
    map_then_unlink(directory, &unlinked[i]);
  }

  // Map stdin first, when it is a regular file, so its line is in the table.
  struct stat stdin_stat;
  CHECK(fstat(0, &stdin_stat) == 0);
  uintptr_t stdin_start = 0;
  uintptr_t stdin_end = 0;
  if (S_ISREG(stdin_stat.st_mode) && stdin_stat.st_size > 0) {
    void* mapped =
        mmap(NULL, (size_t)stdin_stat.st_size, PROT_READ, MAP_PRIVATE, 0, 0);
    CHECK(mapped != MAP_FAILED);
    stdin_start = (uintptr_t)mapped;
    stdin_end = stdin_start + (uintptr_t)stdin_stat.st_size;
  }

  char executable[4096];
  ssize_t executable_length =
      readlink("/proc/self/exe", executable, sizeof(executable) - 1);
  CHECK(executable_length > 0);
  executable[executable_length] = '\0';
  const char* executable_link = full_table ? executable : "/proc/self/exe";
  struct stat executable_stat;
  CHECK(stat(executable_link, &executable_stat) == 0);

  FILE* maps = fopen("/proc/self/maps", "re");
  CHECK(maps != NULL);
  if (full_table) {
    fill_descriptor_table();
  }
  char* line = NULL;
  size_t capacity = 0;
  int checked = 0;
  int split = 0;
  int executable_lines = 0;
  int stdin_lines = 0;
  while (getline(&line, &capacity, maps) > 0) {
    line[strcspn(line, "\n")] = '\0';
    uintptr_t start = 0;
    uintptr_t end = 0;
    unsigned major = 0;
    unsigned minor = 0;
    unsigned long inode = 0;
    int path_offset = 0;
    CHECK(
        sscanf(
            line,
            "%lx-%lx %*4s %*x %x:%x %lu %n",
            &start,
            &end,
            &major,
            &minor,
            &inode,
            &path_offset) == 5);
    const char* path = path_offset > 0 ? line + path_offset : "";
    if (check_unlinked_line(
            unlinked, unlinked_count, start, end, major, minor, inode, path)) {
      continue;
    }
    if (stdin_end != 0 && start >= stdin_start && start < stdin_end) {
      if ((unsigned long)stdin_stat.st_ino != inode) {
        fprintf(
            stderr,
            "mapped stdin: maps inode %lu, fstat(0) inode %llu\n",
            inode,
            (unsigned long long)stdin_stat.st_ino);
        exit(1);
      }
      stdin_lines++;
      continue;
    }
    if (inode == 0 || path[0] != '/' || ends_with(path, " (deleted)")) {
      continue;
    }
    struct stat st;
    if (stat(path, &st) != 0) {
      fprintf(
          stderr,
          "maps names %s, which stat cannot find (errno=%d)\n",
          path,
          errno);
      exit(1);
    }
    if ((unsigned long)st.st_ino != inode) {
      fprintf(
          stderr,
          "%s: maps inode %lu (device %x:%x), stat inode %llu\n",
          path,
          inode,
          major,
          minor,
          (unsigned long long)st.st_ino);
      exit(1);
    }
    if (makedev(major, minor) != st.st_dev) {
      split++;
    }
    if (strcmp(path, executable) == 0) {
      if ((unsigned long)executable_stat.st_ino != inode) {
        fprintf(
            stderr,
            "%s: maps inode %lu, stat inode %llu\n",
            executable_link,
            inode,
            (unsigned long long)executable_stat.st_ino);
        exit(1);
      }
      executable_lines++;
    }
    checked++;
  }
  free(line);
  CHECK(fclose(maps) == 0);
  CHECK(executable_lines > 0);
  int unlinked_agree = 0;
  int unlinked_split = 0;
  for (int i = 0; i < unlinked_count; i++) {
    if (unlinked[i].lines != 1) {
      fprintf(
          stderr,
          "unlinked %s: %d maps lines, expected 1\n",
          unlinked[i].name,
          unlinked[i].lines);
      exit(1);
    }
    unlinked_agree++;
    unlinked_split += unlinked[i].split;
  }
  printf(
      "maps-stat checked=%d split=%d unlinked=%d unlinked_split=%d exe=%s "
      "stdin=%s\n",
      checked,
      split,
      unlinked_agree,
      unlinked_split,
      executable_lines > 0 ? "agrees" : "absent",
      stdin_end == 0 ? "unmapped" : (stdin_lines > 0 ? "agrees" : "absent"));
  return 0;
}

// ---------------------------------------------------------------------------
// proc-fd-links

// Open descriptors until open fails with EMFILE, after lowering the soft
// RLIMIT_NOFILE to one past the highest open descriptor. That limit bounds the
// fill on native Linux; under Hermit, which does not enforce it
// (https://github.com/rrnewton/hermit/issues/3388), the host limit the
// process inherited does. Returns how many were opened.
static int fill_descriptor_table(void) {
  int highest = -1;
  DIR* fds = opendir("/proc/self/fd");
  CHECK(fds != NULL);
  int listing = dirfd(fds);
  struct dirent* entry;
  while ((entry = readdir(fds)) != NULL) {
    if (entry->d_name[0] == '.') {
      continue;
    }
    int fd = atoi(entry->d_name);
    if (fd != listing && fd > highest) {
      highest = fd;
    }
  }
  CHECK(closedir(fds) == 0);

  struct rlimit limit;
  CHECK(getrlimit(RLIMIT_NOFILE, &limit) == 0);
  limit.rlim_cur = (rlim_t)highest + 1;
  CHECK(setrlimit(RLIMIT_NOFILE, &limit) == 0);
  int filled = 0;
  for (;;) {
    int fd = open("/dev/null", O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
      CHECK(errno == EMFILE);
      break;
    }
    filled++;
  }
  return filled;
}

static unsigned long long link_inode(const char* target, const char* kind) {
  char prefix[16];
  CHECK(snprintf(prefix, sizeof(prefix), "%s:[", kind) < (int)sizeof(prefix));
  size_t prefix_length = strlen(prefix);
  size_t length = strlen(target);
  if (strncmp(target, prefix, prefix_length) != 0 ||
      length < prefix_length + 2 || target[length - 1] != ']') {
    fprintf(stderr, "link %s does not name a %s\n", target, kind);
    exit(1);
  }
  char* end = NULL;
  errno = 0;
  unsigned long long inode = strtoull(target + prefix_length, &end, 10);
  CHECK(errno == 0 && end == target + length - 1);
  return inode;
}

static void check_other_process_link(
    const char* parent,
    int parent_fd_directory,
    int fd,
    const char* kind) {
  char path[64];
  char target[128];
  CHECK(
      snprintf(path, sizeof(path), "/proc/%s/fd/%d", parent, fd) <
      (int)sizeof(path));
  ssize_t length = readlink(path, target, sizeof(target) - 1);
  CHECK(length > 0);
  target[length] = '\0';

  struct stat st;
  CHECK(fstat(fd, &st) == 0);
  unsigned long long inode = link_inode(target, kind);
  if (inode != (unsigned long long)st.st_ino) {
    fprintf(
        stderr,
        "%s names inode %llu but fstat(%d) reports %llu\n",
        path,
        inode,
        fd,
        (unsigned long long)st.st_ino);
    exit(1);
  }

  char self_path[64];
  char self_target[128];
  CHECK(
      snprintf(self_path, sizeof(self_path), "/proc/self/fd/%d", fd) <
      (int)sizeof(self_path));
  length = readlink(self_path, self_target, sizeof(self_target) - 1);
  CHECK(length > 0);
  self_target[length] = '\0';
  if (strcmp(self_target, target) != 0) {
    fprintf(
        stderr,
        "%s reads %s but %s reads %s\n",
        path,
        target,
        self_path,
        self_target);
    exit(1);
  }

  char name[16];
  char at_target[128];
  CHECK(snprintf(name, sizeof(name), "%d", fd) < (int)sizeof(name));
  length =
      readlinkat(parent_fd_directory, name, at_target, sizeof(at_target) - 1);
  CHECK(length > 0);
  at_target[length] = '\0';
  if (strcmp(at_target, target) != 0) {
    fprintf(
        stderr,
        "readlinkat(%s) reads %s but readlink reads %s\n",
        name,
        at_target,
        target);
    exit(1);
  }
}

static int proc_fd_links(void) {
  int pipe_fds[2];
  CHECK(pipe(pipe_fds) == 0);
  int sockets[2];
  CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sockets) == 0);
  CHECK(fflush(stdout) == 0);
  // Name the parent's /proc directory by what /proc/self reads IN the parent,
  // not by getppid(). They agree on native Linux, and under the ptrace backend,
  // but the DBT backend does not translate a numeric /proc/<pid> path, so a
  // virtual pid there names whichever HOST process has that number
  // (https://github.com/rrnewton/hermit/issues/1817).
  char parent[32];
  ssize_t parent_length = readlink("/proc/self", parent, sizeof(parent) - 1);
  CHECK(parent_length > 0);
  parent[parent_length] = '\0';

  pid_t child = fork();
  CHECK(child >= 0);
  if (child == 0) {
    char directory_path[64];
    CHECK(
        snprintf(
            directory_path, sizeof(directory_path), "/proc/%s/fd", parent) <
        (int)sizeof(directory_path));
    int parent_fd_directory =
        open(directory_path, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    CHECK(parent_fd_directory >= 0);
    fill_descriptor_table();
    check_other_process_link(parent, parent_fd_directory, pipe_fds[0], "pipe");
    check_other_process_link(parent, parent_fd_directory, sockets[0], "socket");
    printf("proc-fd-links pipe=agrees socket=agrees\n");
    CHECK(fflush(stdout) == 0);
    _exit(0);
  }

  int status = 0;
  CHECK(waitpid(child, &status, 0) == child);
  CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0);
  return 0;
}

// ---------------------------------------------------------------------------
// stdio-mmap-sentinel

#define STACK_SENTINEL 0xa5c3a5c35a3c5a3cULL

// Fills [rsp-1024, rsp-128) with STACK_SENTINEL, maps the first page of fd 0
// with a raw mmap system call, and stores in *changed_words how many 8-byte
// words of that range no longer hold the pattern. One asm block does all of it,
// so no compiled code can use the stack between the fill and the count; the
// 128 bytes under the stack pointer are the red zone, which compiled code may
// use without moving the stack pointer, and are left alone. Returns what the
// system call returned.
static long map_stdin_and_count_changed_stack_words(long* changed_words) {
  register long fd __asm__("r8") = 0;
  long result;
  long changed;
  __asm__ volatile(
      "movabs %[pattern], %%rdx\n\t"
      "lea -1024(%%rsp), %%rcx\n\t"
      "lea -128(%%rsp), %%r11\n"
      "1:\n\t"
      "mov %%rdx, (%%rcx)\n\t"
      "add $8, %%rcx\n\t"
      "cmp %%r11, %%rcx\n\t"
      "jb 1b\n\t"
      "mov %[nr], %%eax\n\t"
      "xor %%edi, %%edi\n\t"
      "mov $4096, %%esi\n\t"
      "mov %[prot], %%edx\n\t"
      "mov %[flags], %%r10d\n\t"
      "xor %%r9d, %%r9d\n\t"
      "syscall\n\t"
      "movabs %[pattern], %%rdx\n\t"
      "lea -1024(%%rsp), %%rcx\n\t"
      "lea -128(%%rsp), %%r11\n\t"
      "xor %%esi, %%esi\n"
      "2:\n\t"
      "cmp %%rdx, (%%rcx)\n\t"
      "je 3f\n\t"
      "inc %%rsi\n"
      "3:\n\t"
      "add $8, %%rcx\n\t"
      "cmp %%r11, %%rcx\n\t"
      "jb 2b\n\t"
      : "=&a"(result), "=&S"(changed)
      : "r"(fd),
        [pattern] "i"(STACK_SENTINEL),
        [nr] "i"(SYS_mmap),
        [prot] "i"(PROT_READ),
        [flags] "i"(MAP_PRIVATE)
      : "rcx", "rdx", "rdi", "r9", "r10", "r11", "memory", "cc");
  *changed_words = changed;
  return result;
}

static int stdio_mmap_sentinel(void) {
  struct stat stdin_stat;
  CHECK(fstat(0, &stdin_stat) == 0);
  CHECK(S_ISREG(stdin_stat.st_mode) && stdin_stat.st_size >= 4096);
  long changed = -1;
  long mapped = map_stdin_and_count_changed_stack_words(&changed);
  if (mapped < 0 && mapped > -4096) {
    fprintf(stderr, "raw mmap of stdin failed: errno %ld\n", -mapped);
    exit(1);
  }
  CHECK(munmap((void*)mapped, 4096) == 0);
  printf("stdio-mmap-sentinel changed_words=%ld\n", changed);
  return 0;
}

// ---------------------------------------------------------------------------
// stdio-getdents

// Check one entry listed through `directory`, whatever its name: its d_ino
// must be the st_ino fstatat reports for that name relative to the same
// descriptor. Returns 1 if it is one of the files the owning test created.
static int check_listed_entry(
    int directory,
    const char* name,
    uint64_t d_ino,
    const char* through) {
  struct stat st;
  CHECK(fstatat(directory, name, &st, AT_SYMLINK_NOFOLLOW) == 0);
  if ((uint64_t)st.st_ino != d_ino) {
    fprintf(
        stderr,
        "stdio-getdents: through %s, d_ino of %s is %llu but fstatat reports "
        "%llu\n",
        through,
        name,
        (unsigned long long)d_ino,
        (unsigned long long)st.st_ino);
    exit(1);
  }
  for (int i = 0; i < ENTRY_COUNT; i++) {
    if (strcmp(name, kEntries[i]) == 0) {
      return 1;
    }
  }
  return 0;
}

static int stdio_getdents(void) {
  struct stat st;
  CHECK(fstat(STDOUT_FILENO, &st) == 0);
  CHECK(S_ISDIR(st.st_mode));
  int alias = dup(STDOUT_FILENO);
  CHECK(alias > STDERR_FILENO);
  const int descriptors[2] = {STDOUT_FILENO, alias};
  const char* const names[2] = {"stdout", "a dup of stdout"};
  int entries[2] = {0, 0};
  int matched = 0;
  // Room for one record of a name up to 20 bytes long (19 header bytes, the
  // name and its NUL, rounded up to 8), never for two: the smallest, "." and
  // "..", take 24 bytes each.
  char buffer[40] __attribute__((aligned(8)));
  for (int turn = 0;; turn ^= 1) {
    long count =
        syscall(SYS_getdents64, descriptors[turn], buffer, sizeof(buffer));
    CHECK(count >= 0);
    if (count == 0) {
      break;
    }
    for (long offset = 0; offset < count;) {
      struct linux_dirent64_header* entry =
          (struct linux_dirent64_header*)(buffer + offset);
      entries[turn]++;
      matched += check_listed_entry(
          descriptors[turn], entry->d_name, entry->d_ino, names[turn]);
      offset += entry->d_reclen;
    }
  }
  CHECK(entries[0] > 0 && entries[1] > 0);
  CHECK(matched == ENTRY_COUNT);
  CHECK(close(alias) == 0);
  fprintf(
      stderr,
      "stdio-getdents stdout entries=%d dup entries=%d matched=%d\n",
      entries[0],
      entries[1],
      matched);
  return 0;
}

// ---------------------------------------------------------------------------
// stdout-alias-maps and stdout-pipe-links

// A dup of stdout at the lowest free descriptor from 10 up, clear of the three
// stdio descriptors.
static int dup_stdout_above_stdio(void) {
  int alias = fcntl(STDOUT_FILENO, F_DUPFD, 10);
  CHECK(alias >= 10);
  return alias;
}

// "agrees" when `reported` is `expected`; otherwise prints both to stderr and
// returns "differs".
static const char* inode_verdict(
    const char* view,
    unsigned long long reported,
    unsigned long long expected) {
  if (reported == expected) {
    return "agrees";
  }
  fprintf(
      stderr,
      "%s reports inode %llu, but fstat of the alias and stat of the path "
      "report %llu\n",
      view,
      reported,
      expected);
  return "differs";
}

// The inode column of the one /proc/self/maps line whose range holds
// `address`.
static unsigned long long maps_inode_at(uintptr_t address) {
  FILE* maps = fopen("/proc/self/maps", "re");
  CHECK(maps != NULL);
  char* line = NULL;
  size_t capacity = 0;
  unsigned long long found = 0;
  int lines = 0;
  while (getline(&line, &capacity, maps) > 0) {
    uintptr_t start = 0;
    uintptr_t end = 0;
    unsigned long long inode = 0;
    CHECK(
        sscanf(
            line,
            "%lx-%lx %*4s %*x %*x:%*x %llu",
            &start,
            &end,
            &inode) == 3);
    if (address >= start && address < end) {
      found = inode;
      lines++;
    }
  }
  free(line);
  CHECK(fclose(maps) == 0);
  CHECK(lines == 1);
  return found;
}

// Maps the first page of `fd` read-only and returns the inode its maps line
// reports.
static unsigned long long mapping_inode(int fd) {
  void* mapped = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, fd, 0);
  CHECK(mapped != MAP_FAILED);
  unsigned long long inode = maps_inode_at((uintptr_t)mapped);
  CHECK(munmap(mapped, 4096) == 0);
  return inode;
}

// The inode the `ino:` line of /proc/self/fdinfo/<fd> reports.
static unsigned long long fdinfo_inode(int fd) {
  char path[64];
  CHECK(
      snprintf(path, sizeof(path), "/proc/self/fdinfo/%d", fd) <
      (int)sizeof(path));
  FILE* fdinfo = fopen(path, "re");
  CHECK(fdinfo != NULL);
  char* line = NULL;
  size_t capacity = 0;
  unsigned long long inode = 0;
  int lines = 0;
  while (getline(&line, &capacity, fdinfo) > 0) {
    if (sscanf(line, "ino: %llu", &inode) == 1) {
      lines++;
    }
  }
  free(line);
  CHECK(fclose(fdinfo) == 0);
  CHECK(lines == 1);
  return inode;
}

static int stdout_alias_maps(const char* path) {
  struct stat stdout_stat;
  CHECK(fstat(STDOUT_FILENO, &stdout_stat) == 0);
  CHECK(S_ISREG(stdout_stat.st_mode) && stdout_stat.st_size >= 4096);
  int alias = dup_stdout_above_stdio();
  struct stat alias_stat;
  CHECK(fstat(alias, &alias_stat) == 0);
  struct stat path_stat;
  CHECK(stat(path, &path_stat) == 0);
  struct statx path_statx;
  CHECK(statx(AT_FDCWD, path, 0, STATX_INO, &path_statx) == 0);
  CHECK((path_statx.stx_mask & STATX_INO) != 0);
  // One file: Linux reports one inode for it through every interface.
  CHECK(alias_stat.st_ino == path_stat.st_ino);
  CHECK(path_statx.stx_ino == (uint64_t)path_stat.st_ino);
  const unsigned long long expected = (unsigned long long)path_stat.st_ino;

  const char* alias_view =
      inode_verdict("the maps line of the alias", mapping_inode(alias), expected);
  int reopened = open(path, O_RDONLY | O_CLOEXEC);
  CHECK(reopened >= 0);
  struct stat reopened_stat;
  CHECK(fstat(reopened, &reopened_stat) == 0);
  CHECK(reopened_stat.st_ino == path_stat.st_ino);
  const char* reopened_view = inode_verdict(
      "the maps line of the path opened again read-only",
      mapping_inode(reopened),
      expected);
  const char* fdinfo_view = inode_verdict(
      "the alias's fdinfo ino: line", fdinfo_inode(alias), expected);
  CHECK(close(reopened) == 0);
  CHECK(close(alias) == 0);
  fprintf(
      stderr,
      "stdout-alias-maps alias=%s reopened=%s fdinfo=%s\n",
      alias_view,
      reopened_view,
      fdinfo_view);
  return strcmp(alias_view, "agrees") == 0 &&
          strcmp(reopened_view, "agrees") == 0 &&
          strcmp(fdinfo_view, "agrees") == 0
      ? 0
      : 1;
}

// "agrees" when `target` is `expected`; otherwise prints both to stderr and
// returns "differs".
static const char*
link_verdict(const char* view, const char* target, const char* expected) {
  if (strcmp(target, expected) == 0) {
    return "agrees";
  }
  fprintf(
      stderr,
      "%s reads %s, but fstat of the alias reports %s\n",
      view,
      target,
      expected);
  return "differs";
}

// The parent's /proc/<pid>/fd/<fd> link, read through readlink and through
// readlinkat relative to the parent's open /proc/<pid>/fd: "agrees" when both
// read `expected`.
static const char* parent_link_verdict(
    const char* parent,
    int parent_fd_directory,
    int fd,
    const char* expected) {
  char path[64];
  char target[128];
  CHECK(
      snprintf(path, sizeof(path), "/proc/%s/fd/%d", parent, fd) <
      (int)sizeof(path));
  ssize_t length = readlink(path, target, sizeof(target) - 1);
  CHECK(length > 0);
  target[length] = '\0';
  const char* through_path = link_verdict(path, target, expected);

  char name[16];
  char view[96];
  CHECK(snprintf(name, sizeof(name), "%d", fd) < (int)sizeof(name));
  CHECK(
      snprintf(view, sizeof(view), "readlinkat(/proc/%s/fd, %s)", parent, name) <
      (int)sizeof(view));
  length = readlinkat(parent_fd_directory, name, target, sizeof(target) - 1);
  CHECK(length > 0);
  target[length] = '\0';
  const char* through_directory = link_verdict(view, target, expected);
  return strcmp(through_path, "agrees") == 0 &&
          strcmp(through_directory, "agrees") == 0
      ? "agrees"
      : "differs";
}

static int stdout_pipe_links(void) {
  struct stat stdout_stat;
  CHECK(fstat(STDOUT_FILENO, &stdout_stat) == 0);
  CHECK(S_ISFIFO(stdout_stat.st_mode));
  int alias = dup_stdout_above_stdio();
  // Name the parent's /proc directory by what /proc/self reads IN the parent;
  // see proc_fd_links.
  char parent[32];
  ssize_t parent_length = readlink("/proc/self", parent, sizeof(parent) - 1);
  CHECK(parent_length > 0);
  parent[parent_length] = '\0';

  pid_t child = fork();
  CHECK(child >= 0);
  if (child == 0) {
    struct stat alias_stat;
    CHECK(fstat(alias, &alias_stat) == 0);
    char expected[64];
    CHECK(
        snprintf(
            expected,
            sizeof(expected),
            "pipe:[%llu]",
            (unsigned long long)alias_stat.st_ino) < (int)sizeof(expected));

    char path[64];
    char target[128];
    CHECK(
        snprintf(path, sizeof(path), "/proc/self/fd/%d", alias) <
        (int)sizeof(path));
    ssize_t length = readlink(path, target, sizeof(target) - 1);
    CHECK(length > 0);
    target[length] = '\0';
    const char* own_alias = link_verdict(path, target, expected);

    char directory_path[64];
    CHECK(
        snprintf(
            directory_path, sizeof(directory_path), "/proc/%s/fd", parent) <
        (int)sizeof(directory_path));
    int parent_fd_directory =
        open(directory_path, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    CHECK(parent_fd_directory >= 0);
    const char* parent_alias =
        parent_link_verdict(parent, parent_fd_directory, alias, expected);
    const char* parent_stdout = parent_link_verdict(
        parent, parent_fd_directory, STDOUT_FILENO, expected);
    CHECK(close(parent_fd_directory) == 0);
    fprintf(
        stderr,
        "stdout-pipe-links own-alias=%s parent-alias=%s parent-stdout=%s\n",
        own_alias,
        parent_alias,
        parent_stdout);
    _exit(
        strcmp(own_alias, "agrees") == 0 &&
                strcmp(parent_alias, "agrees") == 0 &&
                strcmp(parent_stdout, "agrees") == 0
            ? 0
            : 1);
  }

  int status = 0;
  CHECK(waitpid(child, &status, 0) == child);
  CHECK(WIFEXITED(status));
  CHECK(close(alias) == 0);
  return WEXITSTATUS(status);
}

int main(int argc, char** argv) {
  if (argc == 3 && strcmp(argv[1], "scm-getdents") == 0) {
    return scm_getdents(argv[2]);
  }
  if ((argc == 2 || argc == 3) && strcmp(argv[1], "maps-stat") == 0) {
    return maps_stat(argc == 3 ? argv[2] : NULL, 0);
  }
  if (argc == 2 && strcmp(argv[1], "maps-stat-full-table") == 0) {
    return maps_stat(NULL, 1);
  }
  if (argc == 2 && strcmp(argv[1], "proc-fd-links") == 0) {
    return proc_fd_links();
  }
  if (argc == 2 && strcmp(argv[1], "stdio-mmap-sentinel") == 0) {
    return stdio_mmap_sentinel();
  }
  if (argc == 2 && strcmp(argv[1], "stdio-getdents") == 0) {
    return stdio_getdents();
  }
  if (argc == 3 && strcmp(argv[1], "stdout-alias-maps") == 0) {
    return stdout_alias_maps(argv[2]);
  }
  if (argc == 2 && strcmp(argv[1], "stdout-pipe-links") == 0) {
    return stdout_pipe_links();
  }
  fprintf(
      stderr,
      "usage: %s scm-getdents DIR | maps-stat [DIR] | maps-stat-full-table | "
      "proc-fd-links | stdio-mmap-sentinel | stdio-getdents | "
      "stdout-alias-maps PATH | stdout-pipe-links\n",
      argv[0]);
  return 2;
}
