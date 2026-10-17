/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * received_directory_openat: a name relative to a directory descriptor this
 * process received over SCM_RIGHTS is looked up below that directory, as
 * Linux looks it up, by openat, fstatat, statx, faccessat and readlinkat.
 * Detcore keeps no record of a received descriptor where its tool does not
 * discover descriptors as they are used (ptrace, KVM), and those calls used
 * to fail with EBADF though the descriptor was open.
 *
 * It makes a directory under /tmp holding a file `f` ("hello\n") and a
 * symbolic link `l` to `f`, and passes that directory, and /proc, to itself
 * over a socketpair. Then, relative to each received descriptor: the file
 * opens and reads its bytes, fstatat and statx describe the file it opened,
 * faccessat(R_OK) succeeds, readlinkat of the link reads `f` and of the file
 * is EINVAL, and a name that is not there is ENOENT.
 *
 * Below a received /proc/self/ns directory, readlinkat of `mnt` reads what
 * readlink of /proc/self/ns/mnt reads; below a received /proc/self/fd
 * directory, readlinkat of a socket's and a pipe's number reads what the
 * absolute spelling reads. Under Hermit those links are Detcore's
 * canonicalized ones, which a run compared with --verify-strict must show
 * alike in both runs.
 *
 * usage: received_directory_openat [--hermit] [--minimal]
 *                                  [--no-process-directories]
 *   --hermit: print what was checked, which must be the same on every
 *   backend; natively print nothing. Exits 0 only if every check passed.
 *   --minimal: the run's /proc is at Hermit's --proc=minimal level, which
 *   omits /proc/interrupts: below a received /proc it is absent too (openat,
 *   fstatat and faccessat ENOENT), as its absolute spelling is.
 *   --no-process-directories: skip the received /proc/self/ns and
 *   /proc/self/fd directories, named on stderr, for a backend whose own /proc
 *   cannot open a process's directory yet (KVM).
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <unistd.h>

static int failures;
static int hermit;
static int minimal;
static int process_directories = 1;

#define CHECK(condition, ...)                              \
  do {                                                     \
    if (!(condition)) {                                    \
      fprintf(stderr, "FAIL %s:%d: ", __FILE__, __LINE__); \
      fprintf(stderr, __VA_ARGS__);                        \
      fprintf(stderr, "\n");                               \
      failures++;                                          \
    }                                                      \
  } while (0)

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

/* `fd`, passed to this process over `socket` and received as a new
 * descriptor. */
static int pass_to_self(int pair[2], int fd) {
  char token = 'p';
  struct iovec iov = {&token, 1};
  char control[CMSG_SPACE(sizeof(int))];
  memset(control, 0, sizeof(control));
  struct msghdr message = {
      .msg_iov = &iov, .msg_iovlen = 1, .msg_control = control, .msg_controllen = sizeof(control)};
  struct cmsghdr* header = CMSG_FIRSTHDR(&message);
  header->cmsg_level = SOL_SOCKET;
  header->cmsg_type = SCM_RIGHTS;
  header->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(header), &fd, sizeof(int));
  CHECK(sendmsg(pair[0], &message, 0) == 1, "sendmsg: %s", strerror(errno));
  memset(control, 0, sizeof(control));
  message.msg_controllen = sizeof(control);
  int received = -1;
  if (recvmsg(pair[1], &message, 0) == 1) {
    header = CMSG_FIRSTHDR(&message);
    if (header != NULL && header->cmsg_type == SCM_RIGHTS) {
      memcpy(&received, CMSG_DATA(header), sizeof(int));
    }
  }
  CHECK(received >= 0 && received != fd, "no descriptor was received");
  return received;
}

/* Below the received directory `directory`, `name` is a file that reads a
 * line starting with `start`; `link`, if not NULL, is a symbolic link to
 * `name`. */
static void check_below(int directory, const char* what, const char* name, const char* start,
                        const char* link) {
  int fd = openat(directory, name, O_RDONLY);
  CHECK(fd >= 0, "%s: openat(%s): %s", what, name, strerror(errno));
  if (fd >= 0) {
    char buffer[64];
    ssize_t length = read(fd, buffer, sizeof(buffer));
    CHECK(length >= (ssize_t)strlen(start) && memcmp(buffer, start, strlen(start)) == 0,
          "%s: %s read %zd bytes that do not start with %s", what, name, length, start);
    struct stat opened;
    struct stat by_name;
    CHECK(fstat(fd, &opened) == 0, "%s: fstat", what);
    CHECK(fstatat(directory, name, &by_name, 0) == 0, "%s: fstatat(%s): %s", what, name,
          strerror(errno));
    CHECK(opened.st_dev == by_name.st_dev && opened.st_ino == by_name.st_ino,
          "%s: fstatat names another file than openat opened", what);
    struct statx extended;
    CHECK(statx(directory, name, 0, STATX_INO, &extended) == 0 &&
              extended.stx_ino == (unsigned long long)opened.st_ino,
          "%s: statx(%s): %s", what, name, strerror(errno));
    close(fd);
  }
  CHECK(faccessat(directory, name, R_OK, 0) == 0, "%s: faccessat(%s): %s", what, name,
        strerror(errno));
  char target[64];
  errno = 0;
  CHECK(readlinkat(directory, name, target, sizeof(target)) == -1 && errno == EINVAL,
        "%s: readlinkat(%s) is not EINVAL: %s", what, name, strerror(errno));
  if (link != NULL) {
    ssize_t length = readlinkat(directory, link, target, sizeof(target) - 1);
    CHECK(length == (ssize_t)strlen(name) && memcmp(target, name, strlen(name)) == 0,
          "%s: readlinkat(%s) gave %zd bytes", what, link, length);
  }
  errno = 0;
  CHECK(openat(directory, "absent", O_RDONLY) == -1 && errno == ENOENT,
        "%s: openat(absent) is not ENOENT: %s", what, strerror(errno));
}

/* readlinkat of `name` below the received directory `directory` reads
 * what readlink of `absolute` reads. */
static void check_link_below(int directory, const char* name, const char* absolute,
                             const char* what) {
  char below[256];
  char direct[256];
  ssize_t length = readlinkat(directory, name, below, sizeof(below) - 1);
  ssize_t expected = readlink(absolute, direct, sizeof(direct) - 1);
  CHECK(length >= 0 && expected >= 0, "%s: readlinkat(%s) %zd, readlink(%s) %zd: %s", what, name,
        length, absolute, expected, strerror(errno));
  if (length >= 0 && expected >= 0) {
    below[length] = '\0';
    direct[expected] = '\0';
    CHECK(strcmp(below, direct) == 0, "%s: readlinkat(%s) read %s where %s reads %s", what, name,
          below, absolute, direct);
  }
}

int main(int argc, char** argv) {
  for (int arg = 1; arg < argc; arg++) {
    if (strcmp(argv[arg], "--hermit") == 0) {
      hermit = 1;
    } else if (strcmp(argv[arg], "--minimal") == 0) {
      minimal = 1;
    } else if (strcmp(argv[arg], "--no-process-directories") == 0) {
      process_directories = 0;
    } else {
      fprintf(stderr, "usage: %s [--hermit] [--minimal] [--no-process-directories]\n",
              argv[0]);
      return 2;
    }
  }
  char directory_path[] = "/tmp/received-directory-XXXXXX";
  CHECK(mkdtemp(directory_path) != NULL, "mkdtemp: %s", strerror(errno));
  char path[128];
  snprintf(path, sizeof(path), "%s/f", directory_path);
  int file = open(path, O_WRONLY | O_CREAT | O_EXCL, 0644);
  CHECK(file >= 0 && write(file, "hello\n", 6) == 6, "create %s: %s", path, strerror(errno));
  close(file);
  snprintf(path, sizeof(path), "%s/l", directory_path);
  CHECK(symlink("f", path) == 0, "symlink %s: %s", path, strerror(errno));

  int pair[2];
  CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0, "socketpair: %s", strerror(errno));
  int directory = open(directory_path, O_RDONLY | O_DIRECTORY);
  CHECK(directory >= 0, "open %s: %s", directory_path, strerror(errno));
  int received = pass_to_self(pair, directory);
  close(directory);
  if (received >= 0) {
    check_below(received, "a received /tmp directory", "f", "hello", "l");
    close(received);
  }
  say("a received /tmp directory: openat, fstatat, statx, faccessat and readlinkat look "
      "names up below it");

  int proc = open("/proc", O_RDONLY | O_DIRECTORY);
  CHECK(proc >= 0, "open /proc: %s", strerror(errno));
  received = pass_to_self(pair, proc);
  close(proc);
  if (received >= 0) {
    check_below(received, "a received /proc", "version", "Linux version ", NULL);
    if (minimal) {
      struct stat ignored;
      errno = 0;
      CHECK(openat(received, "interrupts", O_RDONLY) == -1 && errno == ENOENT,
            "a received /proc: openat(interrupts) at minimal is not ENOENT: %s", strerror(errno));
      errno = 0;
      CHECK(fstatat(received, "interrupts", &ignored, 0) == -1 && errno == ENOENT,
            "a received /proc: fstatat(interrupts) at minimal is not ENOENT: %s", strerror(errno));
      errno = 0;
      CHECK(faccessat(received, "interrupts", R_OK, 0) == -1 && errno == ENOENT,
            "a received /proc: faccessat(interrupts) at minimal is not ENOENT: %s",
            strerror(errno));
      errno = 0;
      CHECK(open("/proc/interrupts", O_RDONLY) == -1 && errno == ENOENT,
            "/proc/interrupts at minimal is not ENOENT: %s", strerror(errno));
    }
    close(received);
  }
  say("a received /proc: openat, fstatat, statx, faccessat and readlinkat look names up "
      "below it");
  if (minimal) {
    say("a received /proc at minimal: interrupts is absent below it, as at its absolute "
        "spelling");
  }

  if (!process_directories) {
    fprintf(stderr, "SKIPPED: received /proc/self/ns and /proc/self/fd directories "
                    "(--no-process-directories)\n");
  } else {
    int ns = open("/proc/self/ns", O_RDONLY | O_DIRECTORY);
    CHECK(ns >= 0, "open /proc/self/ns: %s", strerror(errno));
    received = pass_to_self(pair, ns);
    close(ns);
    if (received >= 0) {
      check_link_below(received, "mnt", "/proc/self/ns/mnt", "a received /proc/self/ns");
      check_link_below(received, "pid", "/proc/self/ns/pid", "a received /proc/self/ns");
      close(received);
    }
    say("a received /proc/self/ns: readlinkat of mnt and pid reads the absolute links");
    int pipe_ends[2];
    CHECK(pipe(pipe_ends) == 0, "pipe: %s", strerror(errno));
    int fds = open("/proc/self/fd", O_RDONLY | O_DIRECTORY);
    CHECK(fds >= 0, "open /proc/self/fd: %s", strerror(errno));
    received = pass_to_self(pair, fds);
    close(fds);
    if (received >= 0) {
      char name[16];
      char absolute[64];
      int linked[] = {pair[0], pipe_ends[0]};
      for (unsigned i = 0; i < 2; i++) {
        snprintf(name, sizeof(name), "%d", linked[i]);
        snprintf(absolute, sizeof(absolute), "/proc/self/fd/%d", linked[i]);
        check_link_below(received, name, absolute, "a received /proc/self/fd");
      }
      close(received);
    }
    close(pipe_ends[0]);
    close(pipe_ends[1]);
    say("a received /proc/self/fd: readlinkat of a socket's and a pipe's number reads the "
        "absolute links");
  }

  snprintf(path, sizeof(path), "%s/f", directory_path);
  unlink(path);
  snprintf(path, sizeof(path), "%s/l", directory_path);
  unlink(path);
  rmdir(directory_path);
  return failures == 0 ? 0 : 1;
}
