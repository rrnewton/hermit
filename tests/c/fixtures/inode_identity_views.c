// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

// ONE FILE, ONE INODE NUMBER, WHICHEVER INTERFACE REPORTS IT.
//
// Usage: inode_identity_views scm-getdents DIR
//        inode_identity_views maps-stat [DIR]
//        inode_identity_views proc-fd-links
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

static int maps_stat(const char* directory) {
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
  struct stat executable_stat;
  CHECK(stat("/proc/self/exe", &executable_stat) == 0);

  FILE* maps = fopen("/proc/self/maps", "re");
  CHECK(maps != NULL);
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
            "/proc/self/exe: maps inode %lu, stat inode %llu\n",
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

int main(int argc, char** argv) {
  if (argc == 3 && strcmp(argv[1], "scm-getdents") == 0) {
    return scm_getdents(argv[2]);
  }
  if ((argc == 2 || argc == 3) && strcmp(argv[1], "maps-stat") == 0) {
    return maps_stat(argc == 3 ? argv[2] : NULL);
  }
  if (argc == 2 && strcmp(argv[1], "proc-fd-links") == 0) {
    return proc_fd_links();
  }
  fprintf(
      stderr,
      "usage: %s scm-getdents DIR | maps-stat [DIR] | proc-fd-links\n",
      argv[0]);
  return 2;
}
