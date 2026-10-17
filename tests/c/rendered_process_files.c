/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * rendered_process_files: a process's cgroup file, which Detcore renders
 * per process without the backend opening a kernel object
 * (https://github.com/rrnewton/hermit/issues/4213, package K slice K1),
 * answers as Linux's /proc/<pid>/cgroup does, for every spelling that
 * reaches it:
 * - /proc/self/cgroup, /proc/<pid>/cgroup and /proc/self/./cgroup are one
 *   file; /proc/thread-self/cgroup, /proc/<pid>/task/<pid>/cgroup and
 *   /proc/self/task/<pid>/cgroup are another; a thread's and a thread's
 *   top-level /proc/<tid>/cgroup are others; each reads the same bytes;
 * - fstat agrees with stat; the file is 0444, owned by the caller's own
 *   identity, of size 0 with one link and 1024-byte blocks;
 * - a descriptor's link names /proc/<pid>/cgroup or
 *   /proc/<pid>/task/<tid>/cgroup, and reopening through /proc/self/fd/N
 *   and /dev/fd/N is the same file;
 * - relative to a /proc, /proc/self, /proc/<pid>, /proc/self/task or
 *   /proc/thread-self directory, and to a /proc/self directory reopened
 *   through its descriptor link, the name is the same file;
 * - a fork's child reads its own file at /proc/self/cgroup, but its parent's
 *   through the /proc/self directory it inherited (the selector resolved at
 *   the parent's open), and at /proc/<parent>/cgroup; the parent reads the
 *   child's at /proc/<child>/cgroup;
 * - a /proc/self directory received over SCM_RIGHTS names its sender's file;
 * - a zombie still has the file, a reaped child does not (ENOENT), nor a pid
 *   or a task no process holds, nor a number with a leading zero;
 * - it seeks as a seq_file (SEEK_END is EINVAL), reads in parts and at a
 *   position, and continues across exec from the position the old image
 *   left;
 * - the calls that write or ask for its owner answer as Linux answers its
 *   owner, with or without CAP_DAC_OVERRIDE and CAP_SYS_ADMIN (as capget
 *   reports them): a write-mode open, access(W_OK), truncate and a user.
 *   attribute need CAP_DAC_OVERRIDE; a write through a write-mode descriptor
 *   is EINVAL, ftruncate succeeds, fallocate is EOPNOTSUPP, sendfile and
 *   splice into it are EINVAL, and none of them changes what it reads; a
 *   mapping is ENODEV; times may be set; chmod is EPERM; O_NOATIME and
 *   F_SET_RW_HINT are its owner's; a lease is EINVAL.
 *
 * usage: rendered_process_files [--hermit] [--no-process-directories]
 *        rendered_process_files --inherited <fd> <ino>
 *   --hermit: under Hermit, which renders the file: its bytes are the root
 *   of the cgroup v2 hierarchy, "0::/\n", its identity Detcore's procfs
 *   device and an inode in Detcore's rendered range, and it prints what it
 *   checked, with the caller's pid, its threads' and its children's shown by
 *   role, which must be the same on every backend. Without it (natively) the
 *   host's bytes are only compared across spellings, and nothing is printed.
 *   --no-process-directories: skip the names relative to a process's or a
 *   thread's /proc directory (also inherited across fork, reopened and
 *   received over SCM_RIGHTS), named on stderr, for a backend whose own /proc
 *   cannot open those directories yet (KVM), and print none of their lines.
 *   --inherited is the program after it execs itself.
 * The program must be started by its absolute path, which it execs again.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/capability.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/sendfile.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <sys/xattr.h>
#include <unistd.h>

#ifndef SYS_faccessat2
#define SYS_faccessat2 439
#endif

#define PROC_SUPER_MAGIC 0x9fa0
#define RENDERED_DEVICE 0xf000ffULL
#define F_SET_RW_HINT_COMMAND 1036

static int failures;
static int hermit;
static int process_directories = 1;
static const char* program;
static pid_t pid;

#define CHECK(condition, ...)                              \
  do {                                                     \
    if (!(condition)) {                                    \
      fprintf(stderr, "FAIL %s:%d: ", __FILE__, __LINE__); \
      fprintf(stderr, __VA_ARGS__);                        \
      fprintf(stderr, "\n");                               \
      failures++;                                          \
    }                                                      \
  } while (0)

/* The call fails with `expected`. */
#define REFUSED(call, expected, what)                                    \
  do {                                                                   \
    errno = 0;                                                           \
    long refused_result = (long)(call);                                  \
    int refused_errno = errno;                                           \
    CHECK(refused_result == -1 && refused_errno == (expected),           \
          "%s: %s gave %ld (%s), not %s", (what), #call, refused_result, \
          strerror(refused_errno), strerror(expected));                  \
  } while (0)

/* The call succeeds with `expected`. */
#define RETURNS(call, expected, what)                                              \
  do {                                                                             \
    errno = 0;                                                                     \
    long returns_result = (long)(call);                                            \
    int returns_errno = errno;                                                     \
    CHECK(returns_result == (long)(expected), "%s: %s gave %ld (%s), not %ld", (what), \
          #call, returns_result, returns_result < 0 ? strerror(returns_errno) : "", \
          (long)(expected));                                                       \
  } while (0)

/* The call gives `expected`, 0 for success or an errno. */
#define ANSWERS(call, expected, what)       \
  do {                                      \
    if ((expected) == 0) {                  \
      RETURNS(call, 0, what);               \
    } else {                                \
      REFUSED(call, (expected), what);      \
    }                                       \
  } while (0)

/* The bytes /proc/self/cgroup reads, which every spelling must read. */
static char reference[4096];
static ssize_t reference_length;

/* Prints a line of what was checked, under Hermit only: stdout is compared
 * across backends. */
static void say(const char* format, ...) __attribute__((format(printf, 1, 2)));
static void say(const char* format, ...) {
  if (!hermit) {
    return;
  }
  va_list arguments;
  va_start(arguments, format);
  vprintf(format, arguments);
  va_end(arguments);
  printf("\n");
}

static ssize_t read_fd(int fd, char* buffer, size_t capacity) {
  size_t total = 0;
  while (total < capacity) {
    ssize_t n = read(fd, buffer + total, capacity - total);
    if (n < 0) {
      return -1;
    }
    if (n == 0) {
      break;
    }
    total += (size_t)n;
  }
  return (ssize_t)total;
}

/* Opens `path` relative to `dirfd`, reads it whole, and checks that it reads
 * the reference bytes. Returns the open descriptor, or -1. */
static int open_and_compare(int dirfd, const char* path, const char* what) {
  int fd = openat(dirfd, path, O_RDONLY);
  CHECK(fd >= 0, "%s: open %s: %s", what, path, strerror(errno));
  if (fd < 0) {
    return -1;
  }
  char buffer[4096];
  ssize_t length = read_fd(fd, buffer, sizeof(buffer));
  CHECK(length == reference_length && memcmp(buffer, reference, (size_t)length) == 0,
        "%s: %s read %zd bytes that differ from /proc/self/cgroup's %zd", what, path, length,
        reference_length);
  return fd;
}

static int same_file(const struct stat* a, const struct stat* b) {
  return a->st_dev == b->st_dev && a->st_ino == b->st_ino;
}

/* The file's metadata is Linux's for a process's cgroup file its owner
 * reads, and fstat of `fd` agrees with stat of `path`. */
static void check_metadata(int fd, const char* path, const char* what) {
  struct stat by_path;
  struct stat by_fd;
  CHECK(stat(path, &by_path) == 0, "%s: stat %s: %s", what, path, strerror(errno));
  CHECK(fstat(fd, &by_fd) == 0, "%s: fstat: %s", what, strerror(errno));
  CHECK(same_file(&by_path, &by_fd), "%s: fstat %lu:%lu, stat %lu:%lu", what,
        (unsigned long)by_fd.st_dev, (unsigned long)by_fd.st_ino, (unsigned long)by_path.st_dev,
        (unsigned long)by_path.st_ino);
  CHECK(by_fd.st_mode == (S_IFREG | 0444), "%s: mode %o", what, by_fd.st_mode);
  CHECK(by_fd.st_uid == geteuid() && by_fd.st_gid == getegid(), "%s: owner %u:%u, caller %u:%u",
        what, by_fd.st_uid, by_fd.st_gid, geteuid(), getegid());
  CHECK(by_fd.st_size == 0 && by_fd.st_nlink == 1 && by_fd.st_blksize == 1024,
        "%s: size %lld, %lu links, %ld-byte blocks", what, (long long)by_fd.st_size,
        (unsigned long)by_fd.st_nlink, (long)by_fd.st_blksize);
  if (hermit) {
    CHECK(by_fd.st_dev == RENDERED_DEVICE && (by_fd.st_ino & 0xf0000000ULL) == 0xf0000000ULL,
          "%s: identity %#lx:%lu is not Detcore's rendered procfs file", what,
          (unsigned long)by_fd.st_dev, (unsigned long)by_fd.st_ino);
  }
  struct statfs fs;
  CHECK(fstatfs(fd, &fs) == 0 && fs.f_type == PROC_SUPER_MAGIC, "%s: not on procfs", what);
}

/* The descriptor's link reads `expected`. */
static void check_link(int fd, const char* expected, const char* what) {
  char link[64];
  char target[256];
  snprintf(link, sizeof(link), "/proc/self/fd/%d", fd);
  ssize_t length = readlink(link, target, sizeof(target) - 1);
  CHECK(length >= 0, "%s: readlink %s: %s", what, link, strerror(errno));
  if (length < 0) {
    return;
  }
  target[length] = '\0';
  CHECK(strcmp(target, expected) == 0, "%s: link %s, not %s", what, target, expected);
}

static void stat_of(const char* path, struct stat* out, const char* what) {
  CHECK(stat(path, out) == 0, "%s: stat %s: %s", what, path, strerror(errno));
}

static void nonexistent(const char* path, const char* what) {
  struct stat ignored;
  REFUSED(open(path, O_RDONLY), ENOENT, what);
  REFUSED(stat(path, &ignored), ENOENT, what);
}

/* The thread's /proc/thread-self/cgroup and the paths that name the same
 * file, read in the thread. */
struct thread_result {
  pid_t tid;
  struct stat thread_self;
  struct stat numbered;
  struct stat top_level;
  char link[256];
};

static void* thread_main(void* argument) {
  struct thread_result* result = argument;
  result->tid = (pid_t)syscall(SYS_gettid);
  int fd = open_and_compare(AT_FDCWD, "/proc/thread-self/cgroup", "thread");
  if (fd >= 0) {
    CHECK(fstat(fd, &result->thread_self) == 0, "thread: fstat");
    char link[64];
    snprintf(link, sizeof(link), "/proc/self/fd/%d", fd);
    ssize_t length = readlink(link, result->link, sizeof(result->link) - 1);
    result->link[length < 0 ? 0 : length] = '\0';
    close(fd);
  }
  char path[64];
  snprintf(path, sizeof(path), "/proc/%d/task/%d/cgroup", pid, result->tid);
  stat_of(path, &result->numbered, "thread");
  snprintf(path, sizeof(path), "/proc/%d/cgroup", result->tid);
  stat_of(path, &result->top_level, "thread");
  return NULL;
}

/* The caller's effective capabilities, as capget reports them. */
static unsigned long long effective_capabilities(void) {
  struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
  struct __user_cap_data_struct data[2];
  memset(data, 0, sizeof(data));
  CHECK(syscall(SYS_capget, &header, data) == 0, "capget: %s", strerror(errno));
  return (unsigned long long)data[0].effective | ((unsigned long long)data[1].effective << 32);
}

static void check_spellings(void) {
  char path[128];
  char expected[128];
  struct stat self;
  struct stat thread_self;
  struct stat other;
  int fd = open_and_compare(AT_FDCWD, "/proc/self/cgroup", "self");
  if (fd < 0) {
    return;
  }
  check_metadata(fd, "/proc/self/cgroup", "self");
  snprintf(expected, sizeof(expected), "/proc/%d/cgroup", pid);
  check_link(fd, expected, "self");
  CHECK(fstat(fd, &self) == 0, "self: fstat");
  int numbered = open_and_compare(AT_FDCWD, expected, "numbered");
  if (numbered >= 0) {
    check_metadata(numbered, expected, "numbered");
    check_link(numbered, expected, "numbered");
    CHECK(fstat(numbered, &other) == 0 && same_file(&self, &other),
          "/proc/<pid>/cgroup is not /proc/self/cgroup");
    close(numbered);
  }
  stat_of("/proc/self/./cgroup", &other, "dot");
  CHECK(same_file(&self, &other), "/proc/self/./cgroup is not /proc/self/cgroup");
  say("process: /proc/self/cgroup = /proc/<pid>/cgroup = /proc/self/./cgroup");

  /* Reopened through its descriptor links. */
  snprintf(path, sizeof(path), "/proc/self/fd/%d", fd);
  int reopened = open_and_compare(AT_FDCWD, path, "reopened");
  if (reopened >= 0) {
    CHECK(fstat(reopened, &other) == 0 && same_file(&self, &other),
          "/proc/self/fd/N is not the file");
    check_link(reopened, expected, "reopened");
    close(reopened);
  }
  snprintf(path, sizeof(path), "/dev/fd/%d", fd);
  reopened = open_and_compare(AT_FDCWD, path, "/dev/fd");
  if (reopened >= 0) {
    CHECK(fstat(reopened, &other) == 0 && same_file(&self, &other), "/dev/fd/N is not the file");
    close(reopened);
  }
  say("reopened: /proc/self/fd/N = /dev/fd/N = the file, link /proc/<pid>/cgroup");

  int thread_fd = open_and_compare(AT_FDCWD, "/proc/thread-self/cgroup", "thread-self");
  if (thread_fd >= 0) {
    snprintf(expected, sizeof(expected), "/proc/%d/task/%d/cgroup", pid, pid);
    check_metadata(thread_fd, "/proc/thread-self/cgroup", "thread-self");
    check_link(thread_fd, expected, "thread-self");
    CHECK(fstat(thread_fd, &thread_self) == 0, "thread-self: fstat");
    CHECK(!same_file(&self, &thread_self), "/proc/thread-self/cgroup is /proc/self/cgroup");
    stat_of(expected, &other, "task");
    CHECK(same_file(&thread_self, &other), "/proc/<pid>/task/<pid>/cgroup is not thread-self's");
    snprintf(path, sizeof(path), "/proc/self/task/%d/cgroup", pid);
    stat_of(path, &other, "self task");
    CHECK(same_file(&thread_self, &other), "/proc/self/task/<pid>/cgroup is not thread-self's");
    close(thread_fd);
  }
  say("thread: /proc/thread-self/cgroup = /proc/<pid>/task/<pid>/cgroup = "
      "/proc/self/task/<pid>/cgroup, not /proc/self/cgroup");

  struct thread_result result;
  memset(&result, 0, sizeof(result));
  pthread_t thread;
  CHECK(pthread_create(&thread, NULL, thread_main, &result) == 0, "pthread_create");
  pthread_join(thread, NULL);
  snprintf(expected, sizeof(expected), "/proc/%d/task/%d/cgroup", pid, result.tid);
  CHECK(strcmp(result.link, expected) == 0, "thread: link %s, not %s", result.link, expected);
  CHECK(same_file(&result.thread_self, &result.numbered),
        "a thread's thread-self file is not /proc/<pid>/task/<tid>/cgroup");
  CHECK(!same_file(&result.thread_self, &thread_self) && !same_file(&result.thread_self, &self),
        "a thread's file is the leader's");
  CHECK(!same_file(&result.top_level, &self) && !same_file(&result.top_level, &result.numbered),
        "a thread's /proc/<tid>/cgroup is another spelling's");
  say("other thread: its thread-self = /proc/<pid>/task/<tid>/cgroup, and /proc/<tid>/cgroup "
      "is a file of its own");

  /* Relative to /proc directories. */
  int proc_directory = open("/proc", O_RDONLY | O_DIRECTORY);
  CHECK(proc_directory >= 0, "open /proc: %s", strerror(errno));
  int fd2 = open_and_compare(proc_directory, "self/cgroup", "/proc dirfd");
  if (fd2 >= 0) {
    CHECK(fstat(fd2, &other) == 0 && same_file(&self, &other), "/proc + self/cgroup");
    close(fd2);
  }
  snprintf(path, sizeof(path), "%d/cgroup", pid);
  fd2 = open_and_compare(proc_directory, path, "/proc dirfd numbered");
  if (fd2 >= 0) {
    CHECK(fstat(fd2, &other) == 0 && same_file(&self, &other), "/proc + <pid>/cgroup");
    close(fd2);
  }
  say("directories: /proc + self/cgroup, /proc + <pid>/cgroup = the process's file");
  int self_directory = -1;
  int pid_directory = -1;
  int task_directory = -1;
  int thread_directory = -1;
  int relinked = -1;
  if (!process_directories) {
    fprintf(stderr,
            "SKIPPED: names relative to /proc/self, /proc/<pid>, /proc/self/task and "
            "/proc/thread-self directories, also inherited and received "
            "(--no-process-directories)\n");
  } else {
    self_directory = open("/proc/self", O_RDONLY | O_DIRECTORY);
    CHECK(self_directory >= 0, "open /proc/self: %s", strerror(errno));
    fd2 = open_and_compare(self_directory, "cgroup", "/proc/self dirfd");
    if (fd2 >= 0) {
      CHECK(fstat(fd2, &other) == 0 && same_file(&self, &other), "/proc/self + cgroup");
      close(fd2);
    }
    snprintf(path, sizeof(path), "/proc/%d", pid);
    pid_directory = open(path, O_RDONLY | O_DIRECTORY);
    CHECK(pid_directory >= 0, "open %s: %s", path, strerror(errno));
    fd2 = open_and_compare(pid_directory, "cgroup", "/proc/<pid> dirfd");
    if (fd2 >= 0) {
      CHECK(fstat(fd2, &other) == 0 && same_file(&self, &other), "/proc/<pid> + cgroup");
      close(fd2);
    }
    task_directory = open("/proc/self/task", O_RDONLY | O_DIRECTORY);
    CHECK(task_directory >= 0, "open /proc/self/task: %s", strerror(errno));
    snprintf(path, sizeof(path), "%d/cgroup", pid);
    fd2 = open_and_compare(task_directory, path, "/proc/self/task dirfd");
    if (fd2 >= 0) {
      CHECK(fstat(fd2, &other) == 0 && same_file(&thread_self, &other),
            "/proc/self/task + <pid>/cgroup");
      close(fd2);
    }
    thread_directory = open("/proc/thread-self", O_RDONLY | O_DIRECTORY);
    CHECK(thread_directory >= 0, "open /proc/thread-self: %s", strerror(errno));
    fd2 = open_and_compare(thread_directory, "cgroup", "/proc/thread-self dirfd");
    if (fd2 >= 0) {
      CHECK(fstat(fd2, &other) == 0 && same_file(&thread_self, &other),
            "/proc/thread-self + cgroup");
      close(fd2);
    }
    snprintf(path, sizeof(path), "/proc/self/fd/%d", self_directory);
    relinked = open(path, O_RDONLY | O_DIRECTORY);
    CHECK(relinked >= 0, "open %s: %s", path, strerror(errno));
    fd2 = open_and_compare(relinked, "cgroup", "reopened /proc/self dirfd");
    if (fd2 >= 0) {
      CHECK(fstat(fd2, &other) == 0 && same_file(&self, &other),
            "a reopened /proc/self + cgroup");
      close(fd2);
    }
    say("process directories: /proc/self + cgroup, /proc/<pid> + cgroup, reopened /proc/self + "
        "cgroup = the process's file; /proc/self/task + <pid>/cgroup, /proc/thread-self + "
        "cgroup = the thread's");
  }

  /* A fork's child. */
  int ready[2];
  int done[2];
  CHECK(pipe(ready) == 0 && pipe(done) == 0, "pipe");
  fflush(stdout);
  pid_t child = fork();
  if (child == 0) {
    pid_t me = getpid();
    struct stat mine;
    int own = open_and_compare(AT_FDCWD, "/proc/self/cgroup", "child");
    if (own >= 0) {
      snprintf(expected, sizeof(expected), "/proc/%d/cgroup", me);
      check_link(own, expected, "child");
      CHECK(fstat(own, &mine) == 0 && !same_file(&mine, &self),
            "a child's /proc/self/cgroup is its parent's");
      close(own);
    }
    snprintf(expected, sizeof(expected), "/proc/%d/cgroup", pid);
    if (process_directories) {
      int inherited = open_and_compare(self_directory, "cgroup", "child via inherited dirfd");
      if (inherited >= 0) {
        CHECK(fstat(inherited, &other) == 0 && same_file(&other, &self),
              "the inherited /proc/self directory + cgroup is not the parent's file");
        check_link(inherited, expected, "child via inherited dirfd");
        close(inherited);
      }
      inherited = open_and_compare(relinked, "cgroup", "child via inherited reopened dirfd");
      if (inherited >= 0) {
        CHECK(fstat(inherited, &other) == 0 && same_file(&other, &self),
              "the inherited reopened /proc/self directory + cgroup is not the parent's file");
        close(inherited);
      }
      inherited = open_and_compare(thread_directory, "cgroup", "child via thread-self dirfd");
      if (inherited >= 0) {
        CHECK(fstat(inherited, &other) == 0 && same_file(&other, &thread_self),
              "the inherited /proc/thread-self directory + cgroup is not the parent's thread's");
        close(inherited);
      }
    }
    stat_of(expected, &other, "child");
    CHECK(same_file(&other, &self), "a child's /proc/<parent>/cgroup is not the parent's");
    if (write(ready[1], "r", 1) != 1) {
      _exit(2);
    }
    char byte;
    if (read(done[0], &byte, 1) != 1) {
      _exit(2);
    }
    _exit(failures == 0 ? 0 : 1);
  }
  CHECK(child > 0, "fork: %s", strerror(errno));
  char byte;
  CHECK(read(ready[0], &byte, 1) == 1, "the child did not get ready");
  snprintf(path, sizeof(path), "/proc/%d/cgroup", child);
  int childs = open_and_compare(AT_FDCWD, path, "parent reads the child's");
  if (childs >= 0) {
    check_link(childs, path, "the child's file");
    CHECK(fstat(childs, &other) == 0 && !same_file(&other, &self),
          "the child's file is the parent's");
    close(childs);
  }
  /* A task of the child is not a thread of this process. */
  snprintf(path, sizeof(path), "/proc/self/task/%d/cgroup", child);
  nonexistent(path, "the child as a thread of the parent");
  CHECK(write(done[1], "d", 1) == 1, "release the child");
  /* The child, exited and not yet reaped, is a zombie and keeps its file. */
  siginfo_t info;
  CHECK(waitid(P_PID, (id_t)child, &info, WEXITED | WNOWAIT) == 0, "waitid: %s", strerror(errno));
  snprintf(path, sizeof(path), "/proc/%d/cgroup", child);
  int zombie = openat(AT_FDCWD, path, O_RDONLY);
  CHECK(zombie >= 0, "a zombie's cgroup file: %s", strerror(errno));
  if (zombie >= 0) {
    close(zombie);
  }
  int status;
  CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0,
        "the child failed its checks (status %#x)", status);
  nonexistent(path, "a reaped child");
  say("fork: the child's /proc/self/cgroup is its own; its /proc/<parent>/cgroup is the "
      "parent's; the parent reads /proc/<child>/cgroup; a zombie's exists; a reaped "
      "child's does not");
  if (process_directories) {
    say("fork directories: the child's inherited /proc/self, reopened /proc/self and "
        "/proc/thread-self directories name the parent's");
  }

  /* A /proc/self directory received over SCM_RIGHTS names its sender's. */
  if (process_directories) {
    int pair[2];
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0, "socketpair");
    fflush(stdout);
    pid_t sender = fork();
    if (sender == 0) {
      close(pair[0]);
      int directory = open("/proc/self", O_RDONLY | O_DIRECTORY);
      char token = 's';
      struct iovec iov = {&token, 1};
      char control[CMSG_SPACE(sizeof(int))];
      memset(control, 0, sizeof(control));
      struct msghdr message = {.msg_iov = &iov,
                               .msg_iovlen = 1,
                               .msg_control = control,
                               .msg_controllen = sizeof(control)};
      struct cmsghdr* header = CMSG_FIRSTHDR(&message);
      header->cmsg_level = SOL_SOCKET;
      header->cmsg_type = SCM_RIGHTS;
      header->cmsg_len = CMSG_LEN(sizeof(int));
      memcpy(CMSG_DATA(header), &directory, sizeof(int));
      if (directory < 0 || sendmsg(pair[1], &message, 0) != 1) {
        _exit(2);
      }
      /* Stay until the receiver is done with the directory. */
      if (read(pair[1], &token, 1) != 1) {
        _exit(2);
      }
      _exit(0);
    }
    close(pair[1]);
    char token;
    struct iovec iov = {&token, 1};
    char control[CMSG_SPACE(sizeof(int))];
    struct msghdr message = {.msg_iov = &iov,
                             .msg_iovlen = 1,
                             .msg_control = control,
                             .msg_controllen = sizeof(control)};
    int received = -1;
    if (recvmsg(pair[0], &message, 0) == 1) {
      struct cmsghdr* header = CMSG_FIRSTHDR(&message);
      if (header != NULL && header->cmsg_type == SCM_RIGHTS) {
        memcpy(&received, CMSG_DATA(header), sizeof(int));
      }
    }
    CHECK(received >= 0, "no /proc/self directory was received");
    if (received >= 0) {
      snprintf(expected, sizeof(expected), "/proc/%d/cgroup", sender);
      int senders = open_and_compare(received, "cgroup", "a received /proc/self directory");
      if (senders >= 0) {
        check_link(senders, expected, "a received /proc/self directory");
        stat_of(expected, &other, "the sender's file");
        struct stat theirs;
        CHECK(fstat(senders, &theirs) == 0 && same_file(&theirs, &other) &&
                  !same_file(&theirs, &self),
              "the received /proc/self directory + cgroup is not the sender's file");
        close(senders);
      }
      close(received);
    }
    CHECK(send(pair[0], "d", 1, MSG_NOSIGNAL) == 1, "release the sender");
    CHECK(waitpid(sender, &status, 0) == sender && WIFEXITED(status) && WEXITSTATUS(status) == 0,
          "the sender failed (status %#x)", status);
    close(pair[0]);
    say("scm_rights: a received /proc/self directory + cgroup = the sender's file");
  }

  /* Tasks and spellings no process holds. */
  nonexistent("/proc/999999/cgroup", "no such pid");
  snprintf(path, sizeof(path), "/proc/%d/task/999999/cgroup", pid);
  nonexistent(path, "no such thread");
  snprintf(path, sizeof(path), "/proc/0%d/cgroup", pid);
  nonexistent(path, "a leading zero");
  say("nonexistent: a reaped pid, an unused pid, an unused tid, a leading zero: ENOENT");

  if (process_directories) {
    close(relinked);
    close(thread_directory);
    close(task_directory);
    close(pid_directory);
    close(self_directory);
  }
  close(proc_directory);
  close(fd);
}

static void check_reads(void) {
  int fd = open("/proc/self/cgroup", O_RDONLY);
  CHECK(fd >= 0, "open: %s", strerror(errno));
  if (fd < 0) {
    return;
  }
  REFUSED(lseek(fd, 0, SEEK_END), EINVAL, "SEEK_END before a read");
  char buffer[4096];
  ssize_t head = read(fd, buffer, 2);
  ssize_t rest = read_fd(fd, buffer + 2, sizeof(buffer) - 2);
  CHECK(head == 2 && head + rest == reference_length &&
            memcmp(buffer, reference, (size_t)reference_length) == 0,
        "reading in parts gave %zd+%zd bytes", head, rest);
  REFUSED(lseek(fd, 0, SEEK_END), EINVAL, "SEEK_END after a read");
  REFUSED(lseek(fd, 0, SEEK_DATA), EINVAL, "SEEK_DATA");
  RETURNS(lseek(fd, 0, SEEK_SET), 0, "SEEK_SET");
  ssize_t again = read_fd(fd, buffer, sizeof(buffer));
  CHECK(again == reference_length, "a read after SEEK_SET gave %zd bytes", again);
  ssize_t positioned = pread(fd, buffer, sizeof(buffer), 1);
  CHECK(positioned == reference_length - 1 &&
            memcmp(buffer, reference + 1, (size_t)positioned) == 0,
        "pread at 1 gave %zd bytes", positioned);
  close(fd);
  say("reads: SEEK_END and SEEK_DATA EINVAL; parts, a rewind and pread read the bytes");
}

/* Every call a write-mode descriptor makes is refused or does nothing, and
 * the file still reads its bytes. */
static void check_write_mode_descriptor(int fd, const char* what) {
  char buffer[4096];
  struct iovec vector = {"x", 1};
  RETURNS(fcntl(fd, F_GETFL) & O_ACCMODE, O_RDWR, what);
  REFUSED(write(fd, "x", 1), EINVAL, what);
  REFUSED(writev(fd, &vector, 1), EINVAL, what);
  REFUSED(pwritev2(fd, &vector, 1, -1, 0), EINVAL, what);
  REFUSED(pwrite(fd, "x", 1, 0), ESPIPE, what);
  RETURNS(ftruncate(fd, 5), 0, what);
  REFUSED(fallocate(fd, 0, 0, 10), EOPNOTSUPP, what);
  int zero = open("/dev/zero", O_RDONLY);
  off_t offset = 0;
  REFUSED(sendfile(fd, zero, &offset, 1), EINVAL, what);
  close(zero);
  int pipe_ends[2];
  CHECK(pipe(pipe_ends) == 0, "pipe");
  CHECK(write(pipe_ends[1], "x", 1) == 1, "pipe write");
  REFUSED(splice(pipe_ends[0], NULL, fd, NULL, 1, 0), EINVAL, what);
  close(pipe_ends[0]);
  close(pipe_ends[1]);
  void* mapping = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  CHECK(mapping == MAP_FAILED && errno == ENODEV, "%s: a shared writable mapping: %s", what,
        mapping == MAP_FAILED ? strerror(errno) : "mapped");
  REFUSED(fcntl(fd, F_ADD_SEALS, F_SEAL_SEAL), EINVAL, what);
  /* Nothing changed what the file reads, through this descriptor or another. */
  ssize_t length = pread(fd, buffer, sizeof(buffer), 0);
  CHECK(length == reference_length && memcmp(buffer, reference, (size_t)length) == 0,
        "%s: after the writes the file reads %zd bytes", what, length);
  struct stat metadata;
  CHECK(fstat(fd, &metadata) == 0 && metadata.st_size == 0, "%s: size after the writes", what);
  int fresh = open_and_compare(AT_FDCWD, "/proc/self/cgroup", "after the writes");
  if (fresh >= 0) {
    close(fresh);
  }
}

static void check_owner_calls(void) {
  const char* path = "/proc/self/cgroup";
  unsigned long long capabilities = effective_capabilities();
  int dac_override = (capabilities >> CAP_DAC_OVERRIDE) & 1;
  int sys_admin = (capabilities >> CAP_SYS_ADMIN) & 1;
  say("capabilities: dac_override=%d sys_admin=%d", dac_override, sys_admin);
  int writes = dac_override ? 0 : EACCES;
  int fd = open(path, O_RDWR);
  CHECK(dac_override ? fd >= 0 : (fd == -1 && errno == EACCES), "open O_RDWR: %d (%s)", fd,
        strerror(errno));
  if (fd >= 0) {
    check_write_mode_descriptor(fd, "O_RDWR");
    close(fd);
  }
  fd = open(path, O_WRONLY);
  CHECK(dac_override ? fd >= 0 : (fd == -1 && errno == EACCES), "open O_WRONLY: %d (%s)", fd,
        strerror(errno));
  if (fd >= 0) {
    char byte;
    REFUSED(read(fd, &byte, 1), EBADF, "read of O_WRONLY");
    REFUSED(write(fd, "x", 1), EINVAL, "write of O_WRONLY");
    void* mapping = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, fd, 0);
    CHECK(mapping == MAP_FAILED && errno == EACCES, "a mapping of O_WRONLY: %s",
          mapping == MAP_FAILED ? strerror(errno) : "mapped");
    close(fd);
  }
  fd = open(path, O_RDONLY | O_TRUNC);
  CHECK(dac_override ? fd >= 0 : (fd == -1 && errno == EACCES), "open O_TRUNC: %d (%s)", fd,
        strerror(errno));
  if (fd >= 0) {
    close(fd);
  }
  ANSWERS(access(path, W_OK), writes, "access(W_OK)");
  ANSWERS(syscall(SYS_faccessat2, AT_FDCWD, path, W_OK, AT_EACCESS), writes,
          "faccessat2(W_OK, AT_EACCESS)");
  RETURNS(access(path, R_OK), 0, "access(R_OK)");
  REFUSED(access(path, X_OK), EACCES, "access(X_OK)");
  ANSWERS(truncate(path, 0), writes, "truncate");
  REFUSED(truncate(path, -1), EINVAL, "truncate(-1)");
  RETURNS(utimensat(AT_FDCWD, path, NULL, 0), 0, "utimensat(now)");
  struct timespec times[2] = {{1, 0}, {2, 0}};
  RETURNS(utimensat(AT_FDCWD, path, times, 0), 0, "utimensat(times)");
  REFUSED(chmod(path, 0444), EPERM, "chmod");
  ANSWERS(setxattr(path, "user.x", "1", 1, 0), dac_override ? EOPNOTSUPP : EACCES,
          "setxattr(user.x)");
  ANSWERS(removexattr(path, "user.x"), dac_override ? EOPNOTSUPP : EACCES, "removexattr(user.x)");
  ANSWERS(setxattr(path, "security.x", "1", 1, 0), sys_admin ? EOPNOTSUPP : EPERM,
          "setxattr(security.x)");
  REFUSED(setxattr(path, "trusted.x", "1", 1, 0), EPERM, "setxattr(trusted.x)");
  char value[16];
  REFUSED(getxattr(path, "user.x", value, sizeof(value)), EOPNOTSUPP, "getxattr(user.x)");
  RETURNS(listxattr(path, value, sizeof(value)), 0, "listxattr");

  fd = open(path, O_RDONLY | O_NOATIME);
  CHECK(fd >= 0, "open O_NOATIME: %s", strerror(errno));
  if (fd >= 0) {
    close(fd);
  }
  fd = open(path, O_RDONLY);
  CHECK(fd >= 0, "open: %s", strerror(errno));
  if (fd >= 0) {
    REFUSED(fchmod(fd, 0444), EPERM, "fchmod");
    RETURNS(fcntl(fd, F_SETFL, O_NOATIME), 0, "F_SETFL O_NOATIME");
    RETURNS(fcntl(fd, F_GETFL) & O_NOATIME, O_NOATIME, "F_GETFL after O_NOATIME");
    REFUSED(fcntl(fd, F_SETFL, O_DIRECT), EINVAL, "F_SETFL O_DIRECT");
    REFUSED(fcntl(fd, F_SETLEASE, F_RDLCK), EINVAL, "F_SETLEASE");
    RETURNS(fcntl(fd, F_GETLEASE), F_UNLCK, "F_GETLEASE");
    unsigned long long hint = 1;
    RETURNS(fcntl(fd, F_SET_RW_HINT_COMMAND, &hint), 0, "F_SET_RW_HINT");
    REFUSED(fcntl(fd, F_ADD_SEALS, F_SEAL_SEAL), EPERM, "F_ADD_SEALS");
    REFUSED(fcntl(fd, F_GET_SEALS), EINVAL, "F_GET_SEALS");
    REFUSED(fsync(fd), EINVAL, "fsync");
    REFUSED(fsetxattr(fd, "user.x", "1", 1, 0), dac_override ? EOPNOTSUPP : EACCES,
            "fsetxattr(user.x)");
    RETURNS(futimens(fd, NULL), 0, "futimens");
    void* mapping = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, fd, 0);
    CHECK(mapping == MAP_FAILED && errno == ENODEV, "a private mapping: %s",
          mapping == MAP_FAILED ? strerror(errno) : "mapped");
    REFUSED(pwrite(fd, "x", 1, 0), ESPIPE, "pwrite of O_RDONLY");
    REFUSED(write(fd, "x", 1), EBADF, "write of O_RDONLY");
    close(fd);
  }
  say("owner: write-mode opens, access(W_OK), truncate and user. attributes follow "
      "CAP_DAC_OVERRIDE; writes EINVAL, ftruncate 0, fallocate EOPNOTSUPP, sendfile and "
      "splice EINVAL, mappings ENODEV, and the file unchanged; times 0; chmod EPERM; "
      "O_NOATIME and F_SET_RW_HINT allowed; leases EINVAL");
}

int main(int argc, char** argv) {
  program = argv[0];
  pid = getpid();
  int arg = 1;
  if (argc >= 4 && strcmp(argv[1], "--inherited") == 0) {
    hermit = 0;
    int fd = atoi(argv[2]);
    unsigned long long ino = strtoull(argv[3], NULL, 10);
    char buffer[4096];
    ssize_t rest = read_fd(fd, buffer, sizeof(buffer));
    struct stat metadata;
    CHECK(fstat(fd, &metadata) == 0 && metadata.st_ino == ino,
          "after exec the descriptor is another file");
    int whole = open("/proc/self/cgroup", O_RDONLY);
    char expected[4096];
    ssize_t length = read_fd(whole, expected, sizeof(expected));
    CHECK(length >= 2 && rest == length - 2 && memcmp(buffer, expected + 2, (size_t)rest) == 0,
          "after exec the rest is %zd bytes of %zd", rest, length);
    if (failures == 0) {
      printf("exec: the descriptor continues from its position, 2+%zd bytes\n", rest);
    }
    return failures == 0 ? 0 : 1;
  }
  for (; arg < argc; arg++) {
    if (strcmp(argv[arg], "--hermit") == 0) {
      hermit = 1;
    } else if (strcmp(argv[arg], "--no-process-directories") == 0) {
      process_directories = 0;
    } else {
      fprintf(stderr, "usage: %s [--hermit] [--no-process-directories]\n", argv[0]);
      return 2;
    }
  }
  int fd = open("/proc/self/cgroup", O_RDONLY);
  CHECK(fd >= 0, "open /proc/self/cgroup: %s", strerror(errno));
  if (fd < 0) {
    return 1;
  }
  reference_length = read_fd(fd, reference, sizeof(reference));
  close(fd);
  CHECK(reference_length > 0, "/proc/self/cgroup is empty");
  if (hermit) {
    CHECK(reference_length == 5 && memcmp(reference, "0::/\n", 5) == 0,
          "/proc/self/cgroup is not the root of the cgroup v2 hierarchy: %.*s",
          (int)reference_length, reference);
    say("contents: 0::/");
  }
  check_spellings();
  check_reads();
  check_owner_calls();
  /* Across exec: the descriptor read in part continues where it was. */
  fd = open("/proc/self/cgroup", O_RDONLY);
  char head[2];
  struct stat metadata;
  CHECK(fd >= 0 && read(fd, head, 2) == 2 && fstat(fd, &metadata) == 0, "before exec");
  if (failures != 0) {
    return 1;
  }
  fflush(stdout);
  char fd_text[16];
  char ino_text[32];
  snprintf(fd_text, sizeof(fd_text), "%d", fd);
  snprintf(ino_text, sizeof(ino_text), "%llu", (unsigned long long)metadata.st_ino);
  if (hermit) {
    execl(program, program, "--inherited", fd_text, ino_text, (char*)NULL);
  } else {
    /* Natively, say nothing after exec either. */
    int devnull = open("/dev/null", O_WRONLY);
    dup2(devnull, STDOUT_FILENO);
    execl(program, program, "--inherited", fd_text, ino_text, (char*)NULL);
  }
  fprintf(stderr, "FAIL: exec %s: %s\n", program, strerror(errno));
  return 1;
}
