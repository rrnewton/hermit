/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * proc_fd_directory: the caller's own descriptor directory, /proc/self/fd,
 * listed and used as Linux allows (package K slice F1, toward
 * https://github.com/rrnewton/hermit/issues/4604). Under Hermit, Detcore
 * lists it from its own descriptor table on every backend.
 *
 * It prints: the directory's fstat and statx; a listing with a large buffer
 * (each name, d_type and d_off), the position at the end, and that "."'s
 * d_ino is fstat's; the same listing one record per call, and the EINVAL of
 * buffers too small for one; lseek of every whence; a listing that sees a
 * descriptor opened and one closed while it is read; the position a dup
 * shares; an O_PATH open (EBADF for getdents64, read and lseek; F_GETFL;
 * lookups below it); openat, readlinkat, fstatat and faccessat of names
 * below the directory's descriptor, and its "."; readlink of the
 * descriptor's own link; fchdir to it; the /proc/<pid>/fd, /dev/fd,
 * /proc/thread-self/fd and /proc/<pid>/task/<tid>/fd spellings from the
 * main thread, and the thread spellings and /proc/self/fd from another
 * thread; descriptors received over SCM_RIGHTS, the directory's own among
 * them; a fork's child's own listing; a listing after exec, which a
 * close-on-exec descriptor has left; and CPython's close-all loop
 * (_posixsubprocess's _close_open_fds_safe), which reads the directory with
 * raw getdents64 into a buffer of one record and closes each listed
 * descriptor above 2 but its own, in a child that opened descriptors up to
 * 70.
 *
 * It also checks: that /proc is not seeked to 0x40000000 before any
 * descriptor directory is open; a fork's child's listing of the directory
 * it inherited (the parent's table, whole); the end after a descriptor table
 * grew and shrank, in a fork's child, and after a dup2 whose old descriptor
 * is not open; a received eventfd (which Detcore lists without modelling)
 * closed by the close-all loop; that neither an ordinary /tmp directory
 * seeked to 0x40000000 nor /proc passes for a descriptor directory when
 * received; a process's descriptor link read by a thread with a table of its
 * own; a FIFO opened through the directory; the main thread's
 * /proc/thread-self/fd read by another thread; the directory's own link,
 * followed and not, and a path through it; status flags, the O_PATH
 * refusals, and an anonymous mmap naming an O_PATH descriptor; a 4093-byte
 * path of ./ repeats; fchmodat2 below it; a name below a directory reached
 * through the directory, by its descriptor and through its link spelled
 * with // and /./; 40 and 41 descriptor links in one path (MAXSYMLINKS);
 * and, after exec, received eventfds made
 * close-on-exec by F_SETFD and not by FIONCLEX. A child's failed checks fail
 * its parent, and the checks before exec count after it.
 *
 * usage: proc_fd_directory [--no-exec] [--no-private-tables]
 *                          [--no-fifo-threads]
 *   --no-exec: skip the exec check.
 *   --no-private-tables: skip the thread with a table of its own (KVM gives
 *   no thread one).
 *   --no-fifo-threads: skip the FIFO whose writer is another thread (KVM's
 *   executor holds the turn in a blocking FIFO open).
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sched.h>
#include <sys/eventfd.h>
#include <sys/file.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/xattr.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>
#include <linux/futex.h>

static int failures;

#define CHECK(cond, ...)                                  \
  do {                                                    \
    if (!(cond)) {                                        \
      failures++;                                         \
      fprintf(stderr, "FAIL %s:%d: ", __FILE__, __LINE__); \
      fprintf(stderr, __VA_ARGS__);                       \
      fputc('\n', stderr);                                \
    }                                                     \
  } while (0)

struct record64 {
  uint64_t ino;
  int64_t off;
  unsigned short reclen;
  unsigned char type;
  char name[];
};

/* A record of CPython's size: struct linux_dirent64 with a 256-byte name. */
struct cpython_dirent64 {
  uint64_t d_ino;
  int64_t d_off;
  unsigned short d_reclen;
  unsigned char d_type;
  char d_name[256];
};

static const char *type_name(unsigned char type) {
  switch (type) {
    case DT_DIR:
      return "d";
    case DT_LNK:
      return "l";
    default:
      return "?";
  }
}

/* Read `fd` from its position with buffers of `capacity` bytes until the
 * end, appending " name:type@d_off" to `out` (with "@" omitted when
 * `offsets` is 0). Returns the number of getdents64 calls, or -errno of
 * the first that failed. If `dot_ino` is not NULL, it gets "."'s d_ino. */
static int list(int fd, size_t capacity, int offsets, char *out, size_t size,
                uint64_t *dot_ino) {
  char buffer[4096];
  int calls = 0;
  out[0] = '\0';
  for (;;) {
    long n = syscall(SYS_getdents64, fd, buffer, capacity);
    calls++;
    if (n < 0) {
      return -errno;
    }
    if (n == 0) {
      return calls;
    }
    for (long at = 0; at < n;) {
      struct record64 *record = (struct record64 *)(buffer + at);
      size_t used = strlen(out);
      if (offsets) {
        snprintf(out + used, size - used, " %s:%s@%lld", record->name,
                 type_name(record->type), (long long)record->off);
      } else {
        snprintf(out + used, size - used, " %s", record->name);
      }
      if (dot_ino != NULL && strcmp(record->name, ".") == 0) {
        *dot_ino = record->ino;
      }
      at += record->reclen;
    }
  }
}

static const char *errno_name(int error) {
  switch (error) {
    case 0:
      return "0";
    case EBADF:
      return "EBADF";
    case EINVAL:
      return "EINVAL";
    case ENXIO:
      return "ENXIO";
    case EACCES:
      return "EACCES";
    case ENOENT:
      return "ENOENT";
    case EISDIR:
      return "EISDIR";
    case EEXIST:
      return "EEXIST";
    case EOPNOTSUPP:
      return "EOPNOTSUPP";
    case ELOOP:
      return "ELOOP";
    default:
      return "other";
  }
}

static void print_seek(const char *label, int fd, off_t offset, int whence) {
  errno = 0;
  off_t result = lseek(fd, offset, whence);
  printf("lseek %s=%lld errno=%s\n", label, (long long)result,
         errno_name(result < 0 ? errno : 0));
}

static int open_directory(const char *path, int flags) {
  int fd = open(path, flags);
  CHECK(fd >= 0, "open %s: %s", path, strerror(errno));
  return fd;
}

/* Send `fd` to ourselves over `pair` and return the received descriptor,
 * received with recvmsg `flags`. */
static int pass_to_self_with(int pair[2], int fd, int flags) {
  char byte = 'x';
  struct iovec iov = {.iov_base = &byte, .iov_len = 1};
  union {
    char buffer[CMSG_SPACE(sizeof(int))];
    struct cmsghdr align;
  } control;
  memset(&control, 0, sizeof(control));
  struct msghdr message = {
      .msg_iov = &iov,
      .msg_iovlen = 1,
      .msg_control = control.buffer,
      .msg_controllen = sizeof(control.buffer),
  };
  struct cmsghdr *header = CMSG_FIRSTHDR(&message);
  header->cmsg_level = SOL_SOCKET;
  header->cmsg_type = SCM_RIGHTS;
  header->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(header), &fd, sizeof(int));
  CHECK(sendmsg(pair[0], &message, 0) == 1, "sendmsg: %s", strerror(errno));
  memset(&control, 0, sizeof(control));
  message.msg_controllen = sizeof(control.buffer);
  CHECK(recvmsg(pair[1], &message, flags) == 1, "recvmsg: %s", strerror(errno));
  header = CMSG_FIRSTHDR(&message);
  if (header == NULL || header->cmsg_type != SCM_RIGHTS) {
    CHECK(0, "no descriptor received");
    return -1;
  }
  int received;
  memcpy(&received, CMSG_DATA(header), sizeof(int));
  return received;
}

static int pass_to_self(int pair[2], int fd) {
  return pass_to_self_with(pair, fd, 0);
}

/* Wait for a child, which exits non-zero if any of its checks failed, and
 * count its failure as this process's. Returns its exit status, or -1. */
static int reap(pid_t child, const char *what) {
  int status = 0;
  CHECK(waitpid(child, &status, 0) == child, "waitpid %s: %s", what, strerror(errno));
  int exited = WIFEXITED(status) ? WEXITSTATUS(status) : -1;
  CHECK(exited == 0, "%s exited %d", what, exited);
  return exited;
}

static void *thread_spellings(void *argument) {
  (void)argument;
  char path[64], names[512];
  int fd = open_directory("/proc/thread-self/fd", O_RDONLY | O_DIRECTORY);
  list(fd, sizeof(names), 0, names, sizeof(names), NULL);
  printf("thread: thread-self/fd:%s\n", names);
  close(fd);
  snprintf(path, sizeof(path), "/proc/self/task/%ld/fd", (long)syscall(SYS_gettid));
  fd = open_directory(path, O_RDONLY | O_DIRECTORY);
  list(fd, sizeof(names), 0, names, sizeof(names), NULL);
  printf("thread: self/task/<tid>/fd:%s\n", names);
  close(fd);
  /* The process's directory, which Linux reads from the leader's table,
   * which this thread shares. */
  fd = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
  list(fd, sizeof(names), 0, names, sizeof(names), NULL);
  printf("thread: self/fd:%s\n", names);
  close(fd);
  return NULL;
}

/* After exec: the inherited directory lists the new image's table. */
static int after_exec(int directory, int cloexec, int received_cloexec, int received_kept,
                      int inherited_failures) {
  failures += inherited_failures;
  char names[512];
  printf("after exec: rewind=%lld\n", (long long)lseek(directory, 0, SEEK_SET));
  list(directory, 4096, 0, names, sizeof(names), NULL);
  char own[16], gone[16];
  snprintf(own, sizeof(own), " %d", directory);
  snprintf(gone, sizeof(gone), " %d", cloexec);
  printf("after exec: listing:%s\n", names);
  printf("after exec: directory listed=%d close-on-exec listed=%d\n", strstr(names, own) != NULL,
         strstr(names, gone) != NULL);
  char received_gone[16], received_here[16];
  snprintf(received_gone, sizeof(received_gone), " %d", received_cloexec);
  snprintf(received_here, sizeof(received_here), " %d", received_kept);
  printf("after exec: received eventfd made close-on-exec by F_SETFD listed=%d, received "
         "MSG_CMSG_CLOEXEC then FIONCLEX listed=%d\n",
         strstr(names, received_gone) != NULL, strstr(names, received_here) != NULL);
  close(received_kept);
  close(directory);
  printf("failures=%d\n", failures);
  return failures == 0 ? 0 : 1;
}

/* CPython 3.9's _close_open_fds_safe, in a child: returns how many it
 * closed. */
static int cpython_close_all(void) {
  int fd_dir_fd = open("/proc/self/fd", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  if (fd_dir_fd < 0) {
    return -1;
  }
  char buffer[sizeof(struct cpython_dirent64)];
  int closed = 0;
  long bytes;
  while ((bytes = syscall(SYS_getdents64, fd_dir_fd, buffer, sizeof(buffer))) > 0) {
    for (long offset = 0; offset < bytes;) {
      struct cpython_dirent64 *entry = (struct cpython_dirent64 *)(buffer + offset);
      offset += entry->d_reclen;
      if (entry->d_name[0] < '0' || entry->d_name[0] > '9') {
        continue;
      }
      int fd = atoi(entry->d_name);
      if (fd > 2 && fd != fd_dir_fd) {
        close(fd);
        closed++;
      }
    }
  }
  close(fd_dir_fd);
  return closed;
}

/* A thread with a descriptor table of its own (clone without CLONE_FILES):
 * its descriptor `number` is its thread's directory, where the leader's is
 * /dev/null. Linux reads /proc/self/fd/N in the leader's table and
 * /proc/thread-self/fd/N in the thread's. */
struct private_table {
  int number;
};

static int in_private_table(void *argument) {
  struct private_table *table = argument;
  char link[128], path[64], expected[64];
  int own = open("/proc/thread-self/fd", O_RDONLY | O_DIRECTORY);
  CHECK(own >= 0, "open /proc/thread-self/fd in the private table: %s", strerror(errno));
  CHECK(dup2(own, table->number) == table->number, "dup2: %s", strerror(errno));
  close(own);
  snprintf(path, sizeof(path), "/proc/self/fd/%d", table->number);
  ssize_t length = readlink(path, link, sizeof(link) - 1);
  link[length < 0 ? 0 : length] = '\0';
  CHECK(strcmp(link, "/dev/null") == 0, "private table: %s reads %s, not the leader's /dev/null",
        path, link);
  errno = 0;
  int opened = open(path, O_RDONLY | O_DIRECTORY);
  CHECK(opened < 0 && errno == ENOTDIR, "private table: open %s O_DIRECTORY=%d errno=%d", path,
        opened, errno);
  snprintf(path, sizeof(path), "/proc/thread-self/fd/%d", table->number);
  length = readlink(path, link, sizeof(link) - 1);
  link[length < 0 ? 0 : length] = '\0';
  snprintf(expected, sizeof(expected), "/proc/%d/task/%ld/fd", (int)getpid(),
           (long)syscall(SYS_gettid));
  CHECK(strcmp(link, expected) == 0, "private table: %s reads %s, not %s", path, link, expected);
  close(table->number);
  return 0;
}

static void check_private_table(int null) {
  struct private_table table = {null};
  size_t size = 1 << 18;
  char *stack =
      mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_STACK, -1, 0);
  CHECK(stack != MAP_FAILED, "mmap: %s", strerror(errno));
  if (stack == MAP_FAILED) {
    return;
  }
  static volatile pid_t word;
  int flags = CLONE_VM | CLONE_FS | CLONE_SIGHAND | CLONE_THREAD | CLONE_SYSVSEM |
              CLONE_PARENT_SETTID | CLONE_CHILD_CLEARTID;
  int tid = clone(in_private_table, stack + size, flags, &table, &word, NULL, &word);
  CHECK(tid > 0, "clone without CLONE_FILES: %s", strerror(errno));
  for (pid_t seen; tid > 0 && (seen = word) != 0;) {
    syscall(SYS_futex, &word, FUTEX_WAIT, seen, NULL, NULL, 0);
  }
  munmap(stack, size);
}

/* A FIFO opened through the directory waits for a writer, another thread. */
static const char *fifo_path;
static void *fifo_writer(void *argument) {
  (void)argument;
  int end = open(fifo_path, O_WRONLY);
  CHECK(end >= 0, "open the FIFO for writing: %s", strerror(errno));
  CHECK(write(end, "fifo", 4) == 4, "write the FIFO: %s", strerror(errno));
  close(end);
  return NULL;
}

/* A thread reading the main thread's /proc/thread-self/fd, whose table it
 * shares. */
static int main_thread_directory;
static void *shared_table_reader(void *argument) {
  char *names = argument;
  lseek(main_thread_directory, 0, SEEK_SET);
  list(main_thread_directory, 4096, 0, names, 512, NULL);
  return NULL;
}

static void print_errno(const char *label, long result) {
  printf("%s=%ld errno=%s\n", label, result, errno_name(result < 0 ? errno : 0));
}

int main(int argc, char **argv) {
  if (argc == 7 && strcmp(argv[1], "--after-exec") == 0) {
    return after_exec(atoi(argv[2]), atoi(argv[3]), atoi(argv[4]), atoi(argv[5]), atoi(argv[6]));
  }
  int no_exec = 0, private_tables = 1, fifo_threads = 1;
  for (int arg = 1; arg < argc; arg++) {
    if (strcmp(argv[arg], "--no-exec") == 0) {
      no_exec = 1;
    } else if (strcmp(argv[arg], "--no-private-tables") == 0) {
      private_tables = 0;
    } else if (strcmp(argv[arg], "--no-fifo-threads") == 0) {
      fifo_threads = 0;
    }
  }
  char names[1024];
  uint64_t dot_ino = 0;

  /* Before any descriptor directory is open, a /proc descriptor is not
   * seeked into the tags' range either (Hermit refuses it; Linux accepts
   * it). */
  {
    int early = open_directory("/proc", O_RDONLY | O_DIRECTORY);
    errno = 0;
    off_t moved = lseek(early, 0x40000000, SEEK_SET);
    printf("forgery before any descriptor directory: /proc lseek 0x40000000=%lld errno=%s\n",
           (long long)moved, errno_name(moved < 0 ? errno : 0));
    close(early);
  }

  /* The directory, its metadata and a whole listing. */
  int directory = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  struct stat by_fd, by_path;
  CHECK(fstat(directory, &by_fd) == 0, "fstat: %s", strerror(errno));
  printf("fstat: dir=%d mode=%o nlink=%lu blksize=%ld size=%lld uid=%u\n",
         S_ISDIR(by_fd.st_mode), by_fd.st_mode & 07777, (unsigned long)by_fd.st_nlink,
         (long)by_fd.st_blksize, (long long)by_fd.st_size, by_fd.st_uid);
  struct statx extended;
  CHECK(statx(directory, "", AT_EMPTY_PATH, STATX_BASIC_STATS, &extended) == 0, "statx: %s",
        strerror(errno));
  printf("statx: dir=%d mode=%o nlink=%u same-ino=%d\n", S_ISDIR(extended.stx_mode),
         extended.stx_mode & 07777, extended.stx_nlink, extended.stx_ino == by_fd.st_ino);
  CHECK(stat("/proc/self/fd", &by_path) == 0, "stat: %s", strerror(errno));
  printf("stat path: same-ino=%d\n", by_path.st_ino == by_fd.st_ino);
  int calls = list(directory, 4096, 1, names, sizeof(names), &dot_ino);
  printf("listing:%s\n", names);
  printf("listing calls=%d dot-ino-is-fstat-ino=%d end=%lld\n", calls, dot_ino == by_fd.st_ino,
         (long long)lseek(directory, 0, SEEK_CUR));

  /* One record per call, and buffers too small for one. */
  printf("rewind=%lld\n", (long long)lseek(directory, 0, SEEK_SET));
  calls = list(directory, 24, 1, names, sizeof(names), NULL);
  printf("paged:%s\npaged calls=%d\n", names, calls);
  lseek(directory, 0, SEEK_SET);
  char small[64];
  errno = 0;
  long n = syscall(SYS_getdents64, directory, small, 10);
  printf("getdents64 10 bytes=%ld errno=%s position=%lld\n", n, errno_name(n < 0 ? errno : 0),
         (long long)lseek(directory, 0, SEEK_CUR));
  errno = 0;
  n = syscall(SYS_getdents64, directory, small, 0);
  printf("getdents64 0 bytes=%ld errno=%s\n", n, errno_name(n < 0 ? errno : 0));

  /* lseek. */
  print_seek("end+0", directory, 0, SEEK_END);
  print_seek("end+5", directory, 5, SEEK_END);
  print_seek("data", directory, 0, SEEK_DATA);
  print_seek("hole", directory, 0, SEEK_HOLE);
  print_seek("set-1", directory, -1, SEEK_SET);
  print_seek("set-2^31", directory, 1LL << 31, SEEK_SET);
  print_seek("whence-7", directory, 0, 7);
  print_seek("set-max", directory, (1LL << 31) - 1, SEEK_SET);
  calls = list(directory, 4096, 1, names, sizeof(names), NULL);
  printf("past the end:%s calls=%d position=%lld\n", names, calls,
         (long long)lseek(directory, 0, SEEK_CUR));
  print_seek("set-4", directory, 4, SEEK_SET);
  list(directory, 4096, 1, names, sizeof(names), NULL);
  printf("from position 4:%s\n", names);

  /* The table as it is at each call: one descriptor opened past the
   * position, one closed before it is reached. */
  lseek(directory, 0, SEEK_SET);
  char first[256];
  n = syscall(SYS_getdents64, directory, first, 72);
  printf("live: first call=%ld\n", n);
  int opened = open("/dev/null", O_RDONLY);
  CHECK(dup2(opened, 20) == 20, "dup2: %s", strerror(errno));
  close(opened);
  list(directory, 4096, 1, names, sizeof(names), NULL);
  printf("live: rest:%s\n", names);
  close(20);

  /* A dup shares the position. */
  int duplicate = dup(directory);
  lseek(directory, 0, SEEK_SET);
  n = syscall(SYS_getdents64, directory, first, 48);
  list(duplicate, 24, 1, names, sizeof(names), NULL);
  printf("dup: after 2 entries through the original:%s\n", names);
  close(duplicate);

  /* Names below the directory's descriptor. */
  int null = open("/dev/null", O_RDONLY);
  char name[16], link[128];
  snprintf(name, sizeof(name), "%d", null);
  ssize_t length = readlinkat(directory, name, link, sizeof(link) - 1);
  link[length < 0 ? 0 : length] = '\0';
  printf("below: readlinkat N=%s\n", link);
  struct stat below;
  int rc = fstatat(directory, name, &below, 0);
  printf("below: fstatat N rc=%d chr=%d\n", rc, rc == 0 && S_ISCHR(below.st_mode));
  /* Only the type: the link's mode is the backend's (KVM's link is 0777
   * where Linux's is 0500), which this slice does not decide. */
  rc = fstatat(directory, name, &below, AT_SYMLINK_NOFOLLOW);
  printf("below: fstatat N nofollow rc=%d lnk=%d\n", rc, rc == 0 && S_ISLNK(below.st_mode));
  printf("below: faccessat N R_OK=%d\n", faccessat(directory, name, R_OK, 0));
  int reopened = openat(directory, name, O_RDONLY);
  char byte;
  printf("below: openat N read=%zd\n", reopened >= 0 ? read(reopened, &byte, 1) : -1);
  if (reopened >= 0) {
    close(reopened);
  }
  errno = 0;
  length = readlinkat(directory, "999", link, sizeof(link));
  printf("below: readlinkat 999=%zd errno=%s\n", length, errno_name(length < 0 ? errno : 0));
  int dot = openat(directory, ".", O_RDONLY | O_DIRECTORY);
  list(dot, 4096, 0, names, sizeof(names), NULL);
  printf("below: openat . listing:%s\n", names);
  close(dot);

  /* The descriptor's own link names the directory. */
  char own_link[64], expected[64];
  snprintf(own_link, sizeof(own_link), "/proc/self/fd/%d", directory);
  snprintf(expected, sizeof(expected), "/proc/%d/fd", (int)getpid());
  length = readlink(own_link, link, sizeof(link) - 1);
  link[length < 0 ? 0 : length] = '\0';
  printf("own link names /proc/<pid>/fd=%d\n", strcmp(link, expected) == 0);

  /* O_PATH. */
  int path_only = open_directory("/proc/self/fd", O_PATH | O_DIRECTORY);
  errno = 0;
  n = syscall(SYS_getdents64, path_only, small, sizeof(small));
  printf("O_PATH: getdents64=%ld errno=%s\n", n, errno_name(n < 0 ? errno : 0));
  errno = 0;
  n = read(path_only, small, 1);
  printf("O_PATH: read=%ld errno=%s\n", n, errno_name(n < 0 ? errno : 0));
  print_seek("O_PATH", path_only, 0, SEEK_SET);
  struct stat path_stat;
  CHECK(fstat(path_only, &path_stat) == 0, "fstat O_PATH: %s", strerror(errno));
  printf("O_PATH: fstat dir=%d mode=%o F_GETFL=%o\n", S_ISDIR(path_stat.st_mode),
         path_stat.st_mode & 07777, fcntl(path_only, F_GETFL));
  length = readlinkat(path_only, name, link, sizeof(link) - 1);
  link[length < 0 ? 0 : length] = '\0';
  printf("O_PATH: readlinkat N=%s\n", link);
  close(path_only);

  /* fchdir: refused under Hermit until the directory has an object of its
   * own; Linux changes the working directory. */
  errno = 0;
  rc = fchdir(directory);
  printf("fchdir=%d errno=%s\n", rc, errno_name(rc < 0 ? errno : 0));
  if (rc == 0) {
    CHECK(chdir("/") == 0, "chdir: %s", strerror(errno));
  }

  /* The thread spellings, from the main thread and another. */
  char path[96];
  int thread_fd = open_directory("/proc/thread-self/fd", O_RDONLY | O_DIRECTORY);
  list(thread_fd, sizeof(names), 0, names, sizeof(names), NULL);
  printf("main: thread-self/fd:%s\n", names);
  close(thread_fd);
  snprintf(path, sizeof(path), "/proc/%d/task/%ld/fd", (int)getpid(), (long)syscall(SYS_gettid));
  thread_fd = open_directory(path, O_RDONLY | O_DIRECTORY);
  list(thread_fd, sizeof(names), 0, names, sizeof(names), NULL);
  printf("main: <pid>/task/<tid>/fd:%s\n", names);
  close(thread_fd);
  snprintf(path, sizeof(path), "/proc/%d/fd", (int)getpid());
  thread_fd = open_directory(path, O_RDONLY | O_DIRECTORY);
  list(thread_fd, sizeof(names), 0, names, sizeof(names), NULL);
  printf("main: <pid>/fd:%s\n", names);
  close(thread_fd);
  /* /dev/fd, Linux's symbolic link to /proc/self/fd. */
  thread_fd = open_directory("/dev/fd", O_RDONLY | O_DIRECTORY);
  list(thread_fd, sizeof(names), 0, names, sizeof(names), NULL);
  printf("main: /dev/fd:%s\n", names);
  close(thread_fd);
  fflush(stdout);
  pthread_t thread;
  CHECK(pthread_create(&thread, NULL, thread_spellings, NULL) == 0, "pthread_create");
  pthread_join(thread, NULL);

  /* SCM_RIGHTS: a received descriptor is listed, and a received descriptor
   * directory lists the table and looks names up below it. */
  int pair[2];
  CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0, "socketpair: %s", strerror(errno));
  int received_null = pass_to_self(pair, null);
  int received_directory = pass_to_self(pair, directory);
  printf("received: null=%d directory=%d\n", received_null, received_directory);
  lseek(directory, 0, SEEK_SET);
  list(directory, 4096, 0, names, sizeof(names), NULL);
  printf("received: listing:%s\n", names);
  /* Linux shares the position with the original (at its end now); Detcore
   * models a received descriptor as a description of its own
   * (https://github.com/rrnewton/hermit/issues/4670), so both rewind. */
  lseek(received_directory, 0, SEEK_SET);
  list(received_directory, 4096, 0, names, sizeof(names), NULL);
  printf("received: through the received directory:%s\n", names);
  length = readlinkat(received_directory, name, link, sizeof(link) - 1);
  link[length < 0 ? 0 : length] = '\0';
  printf("received: readlinkat N below it=%s\n", link);
  close(received_directory);
  close(received_null);
  close(pair[0]);
  close(pair[1]);

  /* A fork's child lists its own table. */
  fflush(stdout);
  pid_t child = fork();
  if (child == 0) {
    close(null);
    int own = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
    list(own, 4096, 0, names, sizeof(names), NULL);
    printf("child: own listing:%s\n", names);
    close(own);
    /* The directory it inherited is the parent's, which still holds the
     * descriptor this child closed, whose number the descriptor opened to
     * read the parent's table takes in this one. */
    lseek(directory, 0, SEEK_SET);
    list(directory, 4096, 0, names, sizeof(names), NULL);
    char parent_null[16];
    snprintf(parent_null, sizeof(parent_null), " %d", null);
    printf("child: the inherited directory lists the parent's descriptor %d=%d\n", null,
           strstr(names, parent_null) != NULL);
    fflush(stdout);
    _exit(failures == 0 ? 0 : 1);
  }
  printf("child exited=%d\n", reap(child, "the listing child"));

  /* CPython's close-all loop, in a child holding descriptors up to 70. */
  fflush(stdout);
  child = fork();
  if (child == 0) {
    int targets[] = {10, 11, 30, 64, 70};
    for (unsigned i = 0; i < sizeof(targets) / sizeof(targets[0]); i++) {
      dup2(null, targets[i]);
    }
    int listing = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
    list(listing, 4096, 1, names, sizeof(names), NULL);
    printf("close-all: before:%s\n", names);
    close(listing);
    int closed = cpython_close_all();
    listing = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
    list(listing, 4096, 0, names, sizeof(names), NULL);
    printf("close-all: closed=%d after:%s\n", closed, names);
    fflush(stdout);
    _exit(failures == 0 ? 0 : 1);
  }
  printf("close-all child exited=%d\n", reap(child, "the close-all child"));

  /* The end follows the table's allocation: dup2(0, 100) grows it to 128
   * slots, which a close keeps; a fork's copy is sized from what is open. */
  fflush(stdout);
  child = fork();
  if (child == 0) {
    dup2(0, 100);
    close(100);
    int grown = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
    list(grown, 4096, 1, names, sizeof(names), NULL);
    printf("capacity: after dup2(0, 100) and close(100):%s\n", names);
    close(grown);
    fflush(stdout);
    pid_t grandchild = fork();
    if (grandchild == 0) {
      grown = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
      list(grown, 4096, 1, names, sizeof(names), NULL);
      printf("capacity: in a fork's child:%s\n", names);
      fflush(stdout);
      _exit(failures == 0 ? 0 : 1);
    }
    reap(grandchild, "the capacity grandchild");
    _exit(failures == 0 ? 0 : 1);
  }
  reap(child, "the capacity child");

  /* A dup2 whose old descriptor is not open grows the table for its target
   * first (ksys_dup3), then fails with EBADF. */
  fflush(stdout);
  child = fork();
  if (child == 0) {
    errno = 0;
    int rc = dup2(-1, 100);
    int error = rc < 0 ? errno : 0;
    int grown = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
    list(grown, 4096, 1, names, sizeof(names), NULL);
    printf("capacity: after a failed dup2(-1, 100)=%d errno=%s:%s\n", rc, errno_name(error), names);
    close(grown);
    fflush(stdout);
    _exit(failures == 0 ? 0 : 1);
  }
  reap(child, "the failed-dup2 child");

  /* The close-all loop closes a received eventfd, which Detcore lists
   * though it does not model it. */
  fflush(stdout);
  child = fork();
  if (child == 0) {
    int pair[2];
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0, "socketpair: %s", strerror(errno));
    int event = eventfd(0, 0);
    int received_event = pass_to_self(pair, event);
    close(event);
    int listing = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
    list(listing, 4096, 0, names, sizeof(names), NULL);
    char received_name[16];
    snprintf(received_name, sizeof(received_name), " %d", received_event);
    printf("close-all eventfd: received=%d listed=%d\n", received_event,
           strstr(names, received_name) != NULL);
    close(listing);
    int closed = cpython_close_all();
    errno = 0;
    int again = close(received_event);
    printf("close-all eventfd: closed=%d close of the received eventfd after=%d errno=%s\n",
           closed, again, errno_name(again < 0 ? errno : 0));
    fflush(stdout);
    _exit(failures == 0 ? 0 : 1);
  }
  reap(child, "the close-all eventfd child");

  /* A position in the tags' range does not make a directory a descriptor
   * directory: an ordinary /tmp directory seeked there, and /proc (which
   * Hermit refuses to seek there), each passed over SCM_RIGHTS. */
  {
    int pair[2];
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0, "socketpair: %s", strerror(errno));
    char scratch[] = "/tmp/proc-fd-directory-XXXXXX";
    CHECK(mkdtemp(scratch) != NULL, "mkdtemp: %s", strerror(errno));
    int plain = open_directory(scratch, O_RDONLY | O_DIRECTORY);
    errno = 0;
    off_t moved = lseek(plain, 0x40000000, SEEK_SET);
    printf("forgery: /tmp directory lseek 0x40000000=%lld errno=%s\n", (long long)moved,
           errno_name(moved < 0 ? errno : 0));
    int received_plain = pass_to_self(pair, plain);
    errno = 0;
    char link[128];
    ssize_t length = readlinkat(received_plain, "1", link, sizeof(link));
    printf("forgery: below the received /tmp directory, readlinkat 1=%zd errno=%s\n", length,
           errno_name(length < 0 ? errno : 0));
    close(received_plain);
    close(plain);
    rmdir(scratch);
    int proc = open_directory("/proc", O_RDONLY | O_DIRECTORY);
    errno = 0;
    moved = lseek(proc, 0x40000000, SEEK_SET);
    printf("forgery: /proc lseek 0x40000000=%lld errno=%s\n", (long long)moved,
           errno_name(moved < 0 ? errno : 0));
    int received_proc = pass_to_self(pair, proc);
    length = readlinkat(received_proc, "self", link, sizeof(link));
    printf("forgery: below the received /proc, self is a link=%d\n", length > 0);
    close(received_proc);
    close(proc);
    close(pair[0]);
    close(pair[1]);
  }

  /* A thread with a descriptor table of its own (Linux reads a process's
   * links in the leader's table); KVM gives no thread a table of its own. */
  if (private_tables) {
    check_private_table(null);
  } else {
    fprintf(stderr, "SKIPPED: a thread with a descriptor table of its own (--no-private-tables)\n");
  }

  /* A FIFO opened through the directory, its writer another thread (KVM's
   * executor holds the turn in a blocking FIFO open, with or without the
   * directory, so it skips this; checked, not printed). */
  if (!fifo_threads) {
    fprintf(stderr, "SKIPPED: a FIFO whose writer is another thread (--no-fifo-threads)\n");
  } else {
    char fifo[] = "/tmp/proc-fd-directory-fifo-XXXXXX";
    int made = mkstemp(fifo);
    close(made);
    unlink(fifo);
    CHECK(mkfifo(fifo, 0600) == 0, "mkfifo: %s", strerror(errno));
    fifo_path = fifo;
    int named = open(fifo, O_PATH);
    char fifo_name[16];
    snprintf(fifo_name, sizeof(fifo_name), "%d", named);
    pthread_t writer;
    CHECK(pthread_create(&writer, NULL, fifo_writer, NULL) == 0, "pthread_create");
    int reader = openat(directory, fifo_name, O_RDONLY);
    char text[8] = {0};
    ssize_t got = reader >= 0 ? read(reader, text, sizeof(text) - 1) : -1;
    CHECK(got == 4 && strcmp(text, "fifo") == 0,
          "a FIFO opened through the directory read %zd bytes: %s", got, text);
    pthread_join(writer, NULL);
    if (reader >= 0) {
      close(reader);
    }
    close(named);
    unlink(fifo);
  }

  /* Another thread reads the main thread's /proc/thread-self/fd, whose table
   * it shares. */
  {
    char shared[512];
    main_thread_directory = open_directory("/proc/thread-self/fd", O_RDONLY | O_DIRECTORY);
    pthread_t reader;
    CHECK(pthread_create(&reader, NULL, shared_table_reader, shared) == 0, "pthread_create");
    pthread_join(reader, NULL);
    printf("shared table: the main thread's directory read by another thread:%s\n", shared);
    close(main_thread_directory);
  }

  /* The directory's own link, followed and not. */
  {
    char own[64];
    snprintf(own, sizeof(own), "/proc/self/fd/%d", directory);
    struct stat followed, link_itself, by_descriptor;
    fstat(directory, &by_descriptor);
    int rc = stat(own, &followed);
    printf("own link followed: rc=%d dir=%d mode=%o same-ino=%d\n", rc, S_ISDIR(followed.st_mode),
           followed.st_mode & 07777, followed.st_ino == by_descriptor.st_ino);
    rc = lstat(own, &link_itself);
    printf("own link not followed: rc=%d lnk=%d\n", rc, S_ISLNK(link_itself.st_mode));
    char own_name[16];
    snprintf(own_name, sizeof(own_name), "%d", directory);
    rc = fstatat(directory, own_name, &followed, 0);
    printf("own link below the directory followed: rc=%d dir=%d same-ino=%d\n", rc,
           S_ISDIR(followed.st_mode), followed.st_ino == by_descriptor.st_ino);
    /* Through the link to the directory, and on below it. */
    char through[96], link[128];
    snprintf(through, sizeof(through), "/proc/self/fd/%d/%d", directory, null);
    ssize_t length = readlink(through, link, sizeof(link) - 1);
    link[length < 0 ? 0 : length] = '\0';
    printf("through the directory's link: readlink /proc/self/fd/D/N=%s\n", link);
  }

  /* Status flags and the O_PATH refusals. */
  {
    int nofollow = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY | O_NOFOLLOW);
    printf("flags: O_NOFOLLOW open F_GETFL=%o\n", fcntl(nofollow, F_GETFL));
    print_errno("flags: F_SETFL O_NONBLOCK", fcntl(nofollow, F_SETFL, O_NONBLOCK));
    printf("flags: after F_SETFL F_GETFL=%o\n", fcntl(nofollow, F_GETFL));
    close(nofollow);
    errno = 0;
    int direct = open("/proc/self/fd", O_RDONLY | O_DIRECTORY | O_DIRECT);
    print_errno("flags: O_DIRECT open", direct);
    if (direct >= 0) {
      close(direct);
    }
    int path_only = open_directory("/proc/self/fd", O_PATH | O_DIRECTORY);
    errno = 0;
    print_errno("O_PATH: F_SETFL", fcntl(path_only, F_SETFL, 0));
    errno = 0;
    print_errno("O_PATH: fchmod", fchmod(path_only, 0500));
    errno = 0;
    print_errno("O_PATH: fsync", fsync(path_only));
    errno = 0;
    print_errno("O_PATH: flock", flock(path_only, LOCK_SH));
    int pending = 0;
    errno = 0;
    print_errno("O_PATH: ioctl FIONREAD", ioctl(path_only, FIONREAD, &pending));
    errno = 0;
    char value[8];
    print_errno("O_PATH: fgetxattr", fgetxattr(path_only, "user.x", value, sizeof(value)));
    errno = 0;
    void *mapped = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, path_only, 0);
    print_errno("O_PATH: mmap", mapped == MAP_FAILED ? -1 : 0);
    /* An anonymous mapping ignores its descriptor argument. */
    errno = 0;
    void *anonymous = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, path_only, 0);
    print_errno("O_PATH: anonymous mmap naming it", anonymous == MAP_FAILED ? -1 : 0);
    if (anonymous != MAP_FAILED) {
      munmap(anonymous, 4096);
    }
    close(path_only);
  }

  /* A long path of ./ repeats below the directory, valid on Linux. */
  {
    static char dots[4096];
    size_t at = 0;
    while (at + 2 < 4093) {
      dots[at++] = '.';
      dots[at++] = '/';
    }
    dots[at++] = '.';
    dots[at] = '\0';
    int again = openat(directory, dots, O_RDONLY | O_DIRECTORY);
    printf("long path: %zu bytes opened=%d\n", strlen(dots), again >= 0);
    if (again >= 0) {
      close(again);
    }
  }

  /* fchmodat2 of a name below the directory changes the file N is. */
  {
    char file[] = "/tmp/proc-fd-directory-mode-XXXXXX";
    int target = mkstemp(file);
    fchmod(target, 0644);
    char target_name[16];
    snprintf(target_name, sizeof(target_name), "%d", target);
    errno = 0;
    long rc = syscall(452 /* fchmodat2 */, directory, target_name, 0600, 0);
    struct stat after;
    fstat(target, &after);
    printf("fchmodat2: rc=%ld errno=%s mode=%o\n", rc, errno_name(rc < 0 ? errno : 0),
           after.st_mode & 0777);
    close(target);
    unlink(file);
  }
  close(null);

  /* A name below a directory reached through the directory's link, by its
   * descriptor and through the link spelled with // and /./ before the
   * descriptor's number. (A name that is not UTF-8 is not used: Hermit's
   * scheduler request for an open of one fails before the open, base and
   * head alike; the bytes after the link are kept as they are, which a unit
   * test checks.) */
  {
    char scratch[] = "/tmp/proc-fd-directory-names-XXXXXX";
    CHECK(mkdtemp(scratch) != NULL, "mkdtemp: %s", strerror(errno));
    int inner = open_directory(scratch, O_RDONLY | O_DIRECTORY);
    int made = openat(inner, "entry", O_CREAT | O_WRONLY | O_CLOEXEC, 0600);
    CHECK(made >= 0, "create entry: %s", strerror(errno));
    close(made);
    char path[192];
    struct stat found;
    snprintf(path, sizeof(path), "/proc/self/fd/%d/%d/entry", directory, inner);
    printf("names: /proc/self/fd/D/T/entry stat=%d\n", stat(path, &found));
    snprintf(path, sizeof(path), "%d/entry", inner);
    printf("names: fstatat D T/entry=%d\n", fstatat(directory, path, &found, 0));
    snprintf(path, sizeof(path), "/proc//self/fd/%d/%d/entry", directory, inner);
    printf("names: /proc//self/fd/D/T/entry stat=%d\n", stat(path, &found));
    snprintf(path, sizeof(path), "/proc/./self/fd/%d/%d/entry", directory, inner);
    printf("names: /proc/./self/fd/D/T/entry stat=%d\n", stat(path, &found));
    snprintf(path, sizeof(path), "//proc/self/fd/%d/%d/entry", directory, inner);
    printf("names: //proc/self/fd/D/T/entry stat=%d\n", stat(path, &found));
    snprintf(path, sizeof(path), "/proc/self/fd/./%d//%d/entry", directory, inner);
    printf("names: /proc/self/fd/./D//T/entry stat=%d\n", stat(path, &found));
    unlinkat(inner, "entry", 0);
    close(inner);
    rmdir(scratch);
  }

  /* The links a lookup follows through the directory's descriptor links
   * count against MAXSYMLINKS, 40: /proc/<pid>/fd and then 40 of them reach
   * the directory, 41 fail with ELOOP, for an open and for a stat. */
  {
    char deep[512];
    int at = snprintf(deep, sizeof(deep), "/proc/%d/fd", (int)getpid());
    struct stat found;
    for (int link = 1; link <= 41; link++) {
      at += snprintf(deep + at, sizeof(deep) - (size_t)at, "/%d", directory);
      if (link == 40 || link == 41) {
        errno = 0;
        int reached = open(deep, O_RDONLY | O_DIRECTORY);
        int open_error = reached < 0 ? errno : 0;
        if (reached >= 0) {
          close(reached);
        }
        errno = 0;
        int stated = stat(deep, &found);
        printf("links: %d descriptor links open=%d errno=%s stat=%d errno=%s\n", link,
               reached >= 0, errno_name(open_error), stated, errno_name(stated < 0 ? errno : 0));
      }
    }
  }

  /* exec: the directory, not close-on-exec, survives; a close-on-exec
   * descriptor does not. */
  if (no_exec) {
    close(directory);
    printf("after exec: not checked (--no-exec)\n");
    printf("failures=%d\n", failures);
    return failures == 0 ? 0 : 1;
  }
  close(directory);
  int inherited = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
  int cloexec = open("/dev/null", O_RDONLY | O_CLOEXEC);
  /* Received eventfds, listed though Detcore does not model them, keep the
   * close-on-exec flag F_SETFD and FIONCLEX give them. */
  CHECK(socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair) == 0, "socketpair: %s",
        strerror(errno));
  int event = eventfd(0, EFD_CLOEXEC);
  int received_cloexec = pass_to_self(pair, event);
  CHECK(fcntl(received_cloexec, F_SETFD, FD_CLOEXEC) == 0, "F_SETFD: %s", strerror(errno));
  int received_kept = pass_to_self_with(pair, event, MSG_CMSG_CLOEXEC);
  CHECK(ioctl(received_kept, FIONCLEX) == 0, "FIONCLEX: %s", strerror(errno));
  close(event);
  close(pair[0]);
  close(pair[1]);
  char inherited_text[16], cloexec_text[16], received_cloexec_text[16], received_kept_text[16],
      failures_text[16];
  snprintf(inherited_text, sizeof(inherited_text), "%d", inherited);
  snprintf(cloexec_text, sizeof(cloexec_text), "%d", cloexec);
  snprintf(received_cloexec_text, sizeof(received_cloexec_text), "%d", received_cloexec);
  snprintf(received_kept_text, sizeof(received_kept_text), "%d", received_kept);
  /* The checks this image made carry over to the next one. */
  snprintf(failures_text, sizeof(failures_text), "%d", failures);
  fflush(stdout);
  execl(argv[0], argv[0], "--after-exec", inherited_text, cloexec_text, received_cloexec_text,
        received_kept_text, failures_text, (char *)NULL);
  printf("exec failed: %s\n", strerror(errno));
  return 1;
}
