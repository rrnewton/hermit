/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Another process's /proc/<pid>/fd directory and its fd/<n> links, as Linux
 * shows them (fs/proc/fd.c at v6.17: proc_readfd_common lists the
 * descriptors the task's table holds, proc_lookupfd_common finds one,
 * proc_fd_link names its file as d_path renders it, after
 * proc_fd_access_allowed in fs/proc/base.c), as lsof's descriptor lines read
 * them.
 *
 * The program's last argument is a directory it may write in. It makes a
 * directory "fds" there, works below it, and removes it before it exits.
 * The parent forks a child that holds, at fixed numbers:
 *   40, 41  its two pipes to the parent;
 *   50      <dir>/file, read-write;
 *   51      <dir>/gone, unlinked since;
 *   52      <dir>/old, renamed to <dir>/new since;
 *   53, 54  a pipe the parent made, which the parent holds too;
 *   55, 56  a socket pair the parent made, which the parent holds too;
 *   57      an eventfd;
 *   58      a memfd named "probe";
 *   59      /dev/null;
 *   60      a close-on-exec duplicate of 50;
 *   61      <dir> itself, a directory;
 *   70      a duplicate of 59, past the table's first 64 slots.
 * The parent lists the child's fd directory (by its process and its thread,
 * by a descriptor of the directory, and again after the child closes 60),
 * reads each link (a pipe's and a socket's compared with the parent's own
 * link of the same object, whose inode number the transcript does not
 * print), and the answers Linux gives for a descriptor the child does not
 * hold, names that are no descriptor number, a slash after a link, a buffer
 * too small and the directory's own metadata; then the same for a child
 * that has exited and is not reaped (a zombie: its table is gone, and its
 * directories are root's, see zombie_child), the
 * directory of a reaped child, and a non-dumpable child read by a reader
 * without effective capabilities (EACCES, proc_fd_access_allowed and
 * proc_fdinfo_access_allowed). Only descriptors from 40 up are listed, so
 * the runner's own descriptors do not enter the transcript. Standard output
 * is the oracle; the program exits 0 when every step ran.
 *
 * With "--fdinfo" before the directory, the parent instead reads the
 * child's fdinfo/50 and prints its first line. With "--entry NAME" before
 * it, the parent instead opens the child's /proc/<pid>/NAME and reads it.
 */

#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/capability.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

/* The child's ends of its two pipes to the parent, at fixed numbers. */
#define READY_FD 40
#define HOLD_FD 41

static int failures;
static char work[4096];
/* The pipe and the socket pair the parent makes and the child inherits. */
static int shared_pipe[2] = {-1, -1};
static int shared_sockets[2] = {-1, -1};

static void fail(const char* what) {
  printf("FAIL %s errno=%d\n", what, errno);
  failures++;
}

static const char* errno_name(int error) {
  switch (error) {
    case 0:
      return "ok";
    case EACCES:
      return "EACCES";
    case ENOENT:
      return "ENOENT";
    case EINVAL:
      return "EINVAL";
    case ENOTDIR:
      return "ENOTDIR";
    case ESRCH:
      return "ESRCH";
    default:
      return "other";
  }
}

static void work_path(char* path, size_t size, const char* name) {
  snprintf(path, size, "%s/%s", work, name);
}

/* Move `fd` to `number`, closing the original. */
static int place(int fd, int number) {
  if (fd < 0 || dup2(fd, number) != number) {
    return -1;
  }
  return fd == number ? 0 : close(fd);
}

/* The child's table, at the numbers listed above. */
static int set_up_descriptors(void) {
  char path[4200];
  work_path(path, sizeof(path), "file");
  if (place(open(path, O_RDWR | O_CREAT, 0644), 50) != 0) {
    return -1;
  }
  work_path(path, sizeof(path), "gone");
  if (place(open(path, O_RDWR | O_CREAT, 0644), 51) != 0 || unlink(path) != 0) {
    return -1;
  }
  char renamed[4200];
  work_path(path, sizeof(path), "old");
  work_path(renamed, sizeof(renamed), "new");
  if (place(open(path, O_RDWR | O_CREAT, 0644), 52) != 0 || rename(path, renamed) != 0) {
    return -1;
  }
  if (place(shared_pipe[0], 53) != 0 || place(shared_pipe[1], 54) != 0 ||
      place(shared_sockets[0], 55) != 0 || place(shared_sockets[1], 56) != 0) {
    return -1;
  }
  if (place(eventfd(0, 0), 57) != 0 || place(memfd_create("probe", 0), 58) != 0 ||
      place(open("/dev/null", O_RDONLY), 59) != 0) {
    return -1;
  }
  if (dup3(50, 60, O_CLOEXEC) != 60 || place(open(work, O_RDONLY | O_DIRECTORY), 61) != 0 ||
      dup2(59, 70) != 70) {
    return -1;
  }
  return 0;
}

/* A child's side: set up its table unless `bare`, report that it runs, then
 * obey the parent's commands until it closes the pipe: 'c' closes 60, each
 * answered with a byte. */
static int child_loop(int ready, int hold, int bare) {
  if (place(ready, READY_FD) != 0 || place(hold, HOLD_FD) != 0) {
    return 3;
  }
  if (!bare && set_up_descriptors() != 0) {
    return 4;
  }
  if (write(READY_FD, "r", 1) != 1) {
    return 5;
  }
  char command;
  while (read(HOLD_FD, &command, 1) == 1) {
    if (command == 'c' && close(60) != 0) {
      return 6;
    }
    if (write(READY_FD, "r", 1) != 1) {
      return 5;
    }
  }
  return 0;
}

struct spawned {
  pid_t pid;
  int ready;
  int release;
};

/* Fork a child that runs `body` with its pipes. The parent waits for the
 * child's first report unless `quiet`. */
static struct spawned spawn(int (*body)(int, int), int quiet) {
  int ready[2], hold[2];
  struct spawned none = {-1, -1, -1};
  if (pipe(ready) != 0 || pipe(hold) != 0) {
    fail("pipe");
    return none;
  }
  pid_t pid = fork();
  if (pid < 0) {
    fail("fork");
    return none;
  }
  if (pid == 0) {
    close(ready[0]);
    close(hold[1]);
    _exit(body == NULL ? 0 : body(ready[1], hold[0]));
  }
  close(ready[1]);
  close(hold[0]);
  struct spawned spawned = {pid, ready[0], hold[1]};
  if (!quiet) {
    char byte;
    if (read(ready[0], &byte, 1) != 1) {
      fail("child ready");
    }
  }
  return spawned;
}

static void command(struct spawned spawned, char command) {
  char byte;
  if (write(spawned.release, &command, 1) != 1 || read(spawned.ready, &byte, 1) != 1) {
    fail("child command");
  }
}

static void finish(struct spawned spawned, const char* label) {
  close(spawned.release);
  close(spawned.ready);
  int status;
  if (waitpid(spawned.pid, &status, 0) != spawned.pid || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    printf("FAIL %s status=%d\n", label, status);
    failures++;
  }
}

static int full_body(int ready, int hold) {
  return child_loop(ready, hold, 0);
}

static int bare_body(int ready, int hold) {
  return child_loop(ready, hold, 1);
}

/* The descriptors from 40 up that the directory open as `fd` lists, in its
 * order, and whether it lists 0, 1 and 2, as "<label>: <n> <n> ... stdio=1". */
static void show_listing_of(const char* label, int fd) {
  DIR* directory = fdopendir(fd);
  if (directory == NULL) {
    printf("%s: %s\n", label, errno_name(errno));
    close(fd);
    return;
  }
  char line[1024];
  int length = snprintf(line, sizeof(line), "%s:", label);
  int stdio = 0, dots = 0;
  struct dirent* entry;
  errno = 0;
  while ((entry = readdir(directory)) != NULL) {
    if (strcmp(entry->d_name, ".") == 0 || strcmp(entry->d_name, "..") == 0) {
      dots++;
      continue;
    }
    int number = atoi(entry->d_name);
    if (number <= 2) {
      stdio++;
    }
    if (number >= 40 && length < (int)sizeof(line) - 16) {
      length += snprintf(line + length, sizeof(line) - length, " %s", entry->d_name);
    }
  }
  int error = errno;
  closedir(directory);
  printf("%s stdio=%d dots=%d%s%s\n", line, stdio == 3, dots, error ? " readdir: " : "",
         error ? errno_name(error) : "");
}

/* The listing of the directory at `path` ([`show_listing_of`]). */
static void show_listing(const char* label, const char* path) {
  int fd = open(path, O_RDONLY | O_DIRECTORY);
  if (fd < 0) {
    printf("%s: %s\n", label, errno_name(errno));
    return;
  }
  show_listing_of(label, fd);
}

/* The target the parent's own descriptor `fd` links to. */
static int own_link(int fd, char* target, size_t size) {
  char path[64];
  snprintf(path, sizeof(path), "/proc/self/fd/%d", fd);
  ssize_t got = readlink(path, target, size - 1);
  if (got < 0) {
    return -1;
  }
  target[got] = 0;
  return 0;
}

/* A link's target as printed: the work directory as <dir>; a pipe's or a
 * socket's object as its kind, with whether it is the object the parent's
 * own descriptor `same` names (compared whole, inode number included). */
static void print_target(const char* label, const char* target, int same) {
  size_t length = strlen(work);
  if (strncmp(target, work, length) == 0 && (target[length] == '/' || target[length] == 0)) {
    printf("%s=<dir>%s\n", label, target + length);
    return;
  }
  const char* kinds[] = {"pipe:[", "socket:["};
  for (int i = 0; i < 2; i++) {
    if (strncmp(target, kinds[i], strlen(kinds[i])) == 0) {
      char own[4096];
      int equal = same >= 0 && own_link(same, own, sizeof(own)) == 0 && strcmp(own, target) == 0;
      printf("%s=%.*s<n>] same as the parent's=%d\n", label, (int)strlen(kinds[i]), kinds[i],
             equal);
      return;
    }
  }
  printf("%s=%s\n", label, target);
}

/* readlink of `path`, printed as `label`. */
static void show_link(const char* label, const char* path, int same) {
  char target[4096];
  ssize_t got = readlink(path, target, sizeof(target) - 1);
  if (got < 0) {
    printf("%s: %s\n", label, errno_name(errno));
    return;
  }
  target[got] = 0;
  print_target(label, target, same);
}

/* The errno of a readlink of /proc/<pid>/<rest>, or 0. */
static int link_errno(pid_t pid, const char* rest) {
  char path[128], target[4096];
  snprintf(path, sizeof(path), "/proc/%d/%s", pid, rest);
  return readlink(path, target, sizeof(target)) < 0 ? errno : 0;
}

/* The errno of an open of /proc/<pid>/<rest> for reading, or 0. */
static int open_errno(pid_t pid, const char* rest) {
  char path[128];
  snprintf(path, sizeof(path), "/proc/%d/%s", pid, rest);
  int fd = open(path, O_RDONLY);
  if (fd < 0) {
    return errno;
  }
  close(fd);
  return 0;
}

/* A child holding the table listed above. */
static void holding_child(void) {
  if (pipe(shared_pipe) != 0 || socketpair(AF_UNIX, SOCK_STREAM, 0, shared_sockets) != 0) {
    fail("shared objects");
    return;
  }
  struct spawned kid = spawn(full_body, 0);
  if (kid.pid < 0) {
    return;
  }
  char path[128], label[64];
  snprintf(path, sizeof(path), "/proc/%d/fd", kid.pid);
  show_listing("listing", path);
  snprintf(path, sizeof(path), "/proc/%d/task/%d/fd", kid.pid, kid.pid);
  show_listing("task listing", path);
  /* Each link, by the process's directory. */
  const int numbers[] = {50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 70};
  const int same[] = {-1, -1, -1, shared_pipe[0], shared_pipe[1], shared_sockets[0],
                      shared_sockets[1], -1, -1, -1, -1, -1, -1};
  for (size_t i = 0; i < sizeof(numbers) / sizeof(numbers[0]); i++) {
    snprintf(label, sizeof(label), "fd/%d", numbers[i]);
    snprintf(path, sizeof(path), "/proc/%d/fd/%d", kid.pid, numbers[i]);
    show_link(label, path, same[i]);
  }
  snprintf(path, sizeof(path), "/proc/%d/task/%d/fd/50", kid.pid, kid.pid);
  show_link("task fd/50", path, -1);
  /* By a name relative to a descriptor of the directory. */
  snprintf(path, sizeof(path), "/proc/%d/fd", kid.pid);
  int directory = open(path, O_RDONLY | O_DIRECTORY);
  if (directory < 0) {
    printf("open the fd directory: %s\n", errno_name(errno));
  } else {
    char target[4096];
    ssize_t got = readlinkat(directory, "52", target, sizeof(target) - 1);
    if (got < 0) {
      printf("readlinkat(fd, 52): %s\n", errno_name(errno));
    } else {
      target[got] = 0;
      print_target("readlinkat(fd, 52)", target, -1);
    }
    struct stat metadata;
    if (fstat(directory, &metadata) != 0) {
      printf("fstat the fd directory: %s\n", errno_name(errno));
    } else {
      printf("fd directory: dir=%d mode=%o nlink=%lu\n", S_ISDIR(metadata.st_mode),
             metadata.st_mode & 07777, (unsigned long)metadata.st_nlink);
    }
    show_listing_of("listing by descriptor", directory);
  }
  struct stat metadata;
  snprintf(path, sizeof(path), "/proc/%d/fd", kid.pid);
  if (stat(path, &metadata) != 0) {
    printf("stat the fd directory: %s\n", errno_name(errno));
  } else {
    printf("stat fd directory: dir=%d mode=%o\n", S_ISDIR(metadata.st_mode),
           metadata.st_mode & 07777);
  }
  /* Cut short by the buffer: the first bytes, no NUL. */
  char whole[4096], cut[8];
  snprintf(path, sizeof(path), "/proc/%d/fd/50", kid.pid);
  ssize_t length = readlink(path, whole, sizeof(whole));
  memset(cut, 'x', sizeof(cut));
  ssize_t short_length = readlink(path, cut, 5);
  printf("fd/50 in 5 bytes: length=%zd prefix=%d untouched=%d\n", short_length,
         length >= 5 && short_length == 5 && memcmp(cut, whole, 5) == 0, cut[5] == 'x');
  /* What the child does not hold, and names that are no descriptor. */
  printf("fd/99: %s\n", errno_name(link_errno(kid.pid, "fd/99")));
  printf("fd/abc: %s\n", errno_name(link_errno(kid.pid, "fd/abc")));
  printf("fd/050: %s\n", errno_name(link_errno(kid.pid, "fd/050")));
  printf("fdinfo/99: %s\n", errno_name(open_errno(kid.pid, "fdinfo/99")));
  printf("fdinfo/abc: %s\n", errno_name(open_errno(kid.pid, "fdinfo/abc")));
  printf("bogus entry: %s\n", errno_name(open_errno(kid.pid, "bogus")));
  /* Followed: a file is no directory, a directory is no link. */
  printf("fd/50/: %s\n", errno_name(link_errno(kid.pid, "fd/50/")));
  printf("fd/53/: %s\n", errno_name(link_errno(kid.pid, "fd/53/")));
  printf("fd/61/: %s\n", errno_name(link_errno(kid.pid, "fd/61/")));
  /* The table as the child changes it. */
  command(kid, 'c');
  snprintf(path, sizeof(path), "/proc/%d/fd", kid.pid);
  show_listing("after close", path);
  printf("after close fd/60: %s\n", errno_name(link_errno(kid.pid, "fd/60")));
  finish(kid, "holding child");
  close(shared_pipe[0]);
  close(shared_pipe[1]);
  close(shared_sockets[0]);
  close(shared_sockets[1]);
}

static int zombie_body(int ready, int hold) {
  (void)hold;
  char path[4200];
  work_path(path, sizeof(path), "file");
  if (place(open(path, O_RDWR), 50) != 0) {
    return 4;
  }
  close(ready);
  return 0;
}

/* A child that has exited, not reaped, and then reaped. */
static void zombie_child(void) {
  struct spawned kid = spawn(zombie_body, 1);
  if (kid.pid < 0) {
    return;
  }
  siginfo_t info;
  memset(&info, 0, sizeof(info));
  if (waitid(P_PID, kid.pid, &info, WEXITED | WNOWAIT) != 0) {
    fail("waitid zombie");
  }
  /* A zombie's directories belong to root (task_dump_owner, fs/proc/base.c,
   * gives root to a task without a memory map), so a reader that is not root
   * is refused at the walk (EACCES, the fd directory is 0500), and a root
   * reader (every guest under Hermit) finds no descriptor: its table is gone
   * (ENOENT, an empty listing). Either way no descriptor is shown, and that
   * is what is printed. */
  char path[128];
  snprintf(path, sizeof(path), "/proc/%d/fd", kid.pid);
  int fd = open(path, O_RDONLY | O_DIRECTORY);
  if (fd < 0 && errno == EACCES) {
    printf("zombie listing: no descriptors\n");
  } else if (fd < 0) {
    printf("zombie listing: %s\n", errno_name(errno));
  } else {
    DIR* directory = fdopendir(fd);
    int listed = 0;
    struct dirent* entry;
    while (directory != NULL && (entry = readdir(directory)) != NULL) {
      listed += strcmp(entry->d_name, ".") != 0 && strcmp(entry->d_name, "..") != 0;
    }
    if (directory != NULL) {
      closedir(directory);
    }
    printf("zombie listing: %s\n", listed == 0 ? "no descriptors" : "descriptors listed");
  }
  int error = link_errno(kid.pid, "fd/50");
  printf("zombie fd/50: %s\n",
         error == ENOENT || error == EACCES ? "no link" : errno_name(error));
  printf("zombie fdinfo/50: %s\n", errno_name(open_errno(kid.pid, "fdinfo/50")));
  pid_t pid = kid.pid;
  finish(kid, "zombie child");
  show_listing("reaped listing", path);
  printf("reaped fd/50: %s\n", errno_name(link_errno(pid, "fd/50")));
}

/* Clear the caller's effective capability set, keeping the permitted. */
static int drop_effective(void) {
  struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
  struct __user_cap_data_struct data[2];
  if (syscall(SYS_capget, &header, data) != 0) {
    return -1;
  }
  data[0].effective = 0;
  data[1].effective = 0;
  return syscall(SYS_capset, &header, data);
}

static int private_body(int ready, int hold) {
  if (prctl(PR_SET_DUMPABLE, 0, 0, 0, 0) != 0) {
    return 3;
  }
  return child_loop(ready, hold, 1);
}

/* A non-dumpable child, read without effective capabilities. Its fd/41 is
 * one of its pipes to the parent. */
static void private_child(void) {
  struct spawned kid = spawn(private_body, 0);
  if (kid.pid < 0) {
    return;
  }
  int report[2];
  if (pipe(report) != 0) {
    fail("pipe");
    return;
  }
  pid_t reader = fork();
  if (reader == 0) {
    close(report[0]);
    if (drop_effective() != 0) {
      _exit(5);
    }
    int answers[2] = {link_errno(kid.pid, "fd/41"), open_errno(kid.pid, "fdinfo/41")};
    _exit(write(report[1], answers, sizeof(answers)) == sizeof(answers) ? 0 : 6);
  }
  close(report[1]);
  int answers[2] = {-1, -1};
  if (read(report[0], answers, sizeof(answers)) != sizeof(answers)) {
    fail("reader report");
  }
  close(report[0]);
  int status;
  waitpid(reader, &status, 0);
  printf("non-dumpable child: fd/41=%s fdinfo/41=%s\n", errno_name(answers[0]),
         errno_name(answers[1]));
  finish(kid, "non-dumpable child");
}

/* The first line of the child's fdinfo/50. */
static void fdinfo_child(void) {
  if (pipe(shared_pipe) != 0 || socketpair(AF_UNIX, SOCK_STREAM, 0, shared_sockets) != 0) {
    fail("shared objects");
    return;
  }
  struct spawned kid = spawn(full_body, 0);
  if (kid.pid < 0) {
    return;
  }
  char path[128], line[256];
  snprintf(path, sizeof(path), "/proc/%d/fdinfo/50", kid.pid);
  FILE* file = fopen(path, "r");
  if (file == NULL) {
    printf("fdinfo/50: %s\n", errno_name(errno));
  } else {
    if (fgets(line, sizeof(line), file) != NULL) {
      printf("fdinfo/50 %s", line);
    }
    fclose(file);
  }
  finish(kid, "fdinfo child");
}

/* An open and a read of the child's /proc/<pid>/<name>. */
static void entry_child(const char* name) {
  struct spawned kid = spawn(bare_body, 0);
  if (kid.pid < 0) {
    return;
  }
  char path[256], buffer[256];
  snprintf(path, sizeof(path), "/proc/%d/%s", kid.pid, name);
  int fd = open(path, O_RDONLY);
  if (fd < 0) {
    printf("entry %s: %s\n", name, errno_name(errno));
  } else {
    ssize_t got = read(fd, buffer, sizeof(buffer));
    printf("entry %s: %s\n", name, got >= 0 ? "read" : errno_name(errno));
    close(fd);
  }
  finish(kid, "entry child");
}

int main(int argc, char** argv) {
  int fdinfo = argc == 3 && strcmp(argv[1], "--fdinfo") == 0;
  int entry = argc == 4 && strcmp(argv[1], "--entry") == 0;
  if (argc != 2 && !fdinfo && !entry) {
    fprintf(stderr, "usage: %s [--fdinfo | --entry NAME] DIRECTORY\n", argv[0]);
    return 2;
  }
  snprintf(work, sizeof(work), "%s/fds", argv[argc - 1]);
  setvbuf(stdout, NULL, _IONBF, 0);
  if (mkdir(work, 0755) != 0) {
    fail("mkdir the work directory");
    return 1;
  }
  if (fdinfo) {
    fdinfo_child();
  } else if (entry) {
    entry_child(argv[2]);
  } else {
    holding_child();
    zombie_child();
    private_child();
  }
  const char* names[] = {"file", "new"};
  for (int i = 0; i < 2; i++) {
    char path[4200];
    work_path(path, sizeof(path), names[i]);
    if (unlink(path) != 0 && errno != ENOENT) {
      fail(names[i]);
    }
  }
  if (rmdir(work) != 0) {
    fail("rmdir the work directory");
  }
  printf("failures=%d\n", failures);
  return failures == 0 ? 0 : 1;
}
