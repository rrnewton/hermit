/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Another process's /proc/<pid>/exe, cwd and root links, as Linux shows them
 * (fs/proc/base.c at v6.17: proc_pid_readlink, which checks
 * proc_fd_access_allowed, then proc_exe_link, proc_cwd_link or
 * proc_root_link, and d_path of what the link names): the file the process's
 * image was read from, its working directory and its root directory, as
 * lsof's txt, cwd and rtd lines read them.
 *
 * The program's one argument is a directory it may write in. It makes a
 * directory "links" there, works below it, and removes it before it exits.
 * The parent reads, for:
 *   - a forked child that changed its working directory: all three links,
 *     by the process's number and below its task directory, by a name
 *     relative to a descriptor of its /proc directory, one cut short by a
 *     small buffer, each followed with a slash, the image and the root with
 *     "/." after them, and the working directory again after the child's
 *     fchdir;
 *   - a forked child that did not change its working directory, which is
 *     the parent's;
 *   - a child that changed its working directory and then exec'd a copy of
 *     this program: its working directory survives the exec, and its exe
 *     link names the copy, then the copy's new name after a rename, then
 *     that name with " (deleted)" after an unlink;
 *   - a second thread of a child, created sharing its filesystem
 *     information (CLONE_FS), after the child's main thread changed the
 *     working directory: the thread's links, by its process's task directory
 *     and by its own number;
 *   - a child that has exited and is not reaped (a zombie): no link
 *     (ENOENT: it has no memory map and no filesystem information);
 *   - a child that made itself non-dumpable (PR_SET_DUMPABLE 0), read by a
 *     reader without effective capabilities: EACCES for every link
 *     (proc_fd_access_allowed asks PTRACE_MODE_READ_FSCREDS).
 * Paths are printed with the work directory as <dir>, this program's own
 * image as <exe> and the parent's working directory as <cwd>, and no pid is
 * printed, so one transcript describes every run. Standard output is the
 * oracle; the program exits 0 when every step ran.
 *
 * With "--renamed-cwd" before the directory, the program instead reads the
 * working directory of a child whose working directory the parent renamed
 * after the child entered it.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/capability.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

/* The child's ends of its two pipes, at fixed numbers. */
#define READY_FD 100
#define HOLD_FD 101

static int failures;
static char work[4096];
static char own_exe[4096];
static char own_cwd[4096];

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

/* A link's target as printed: the work directory as <dir>, this program's
 * image as <exe>, the parent's working directory as <cwd>. */
static void print_target(const char* label, const char* target) {
  size_t length = strlen(work);
  if (strncmp(target, work, length) == 0 && (target[length] == '/' || target[length] == 0)) {
    printf("%s=<dir>%s\n", label, target + length);
  } else if (strcmp(target, own_exe) == 0) {
    printf("%s=<exe>\n", label);
  } else if (strcmp(target, own_cwd) == 0) {
    printf("%s=<cwd>\n", label);
  } else {
    printf("%s=%s\n", label, target);
  }
}

/* readlink of `path`, printed as `label`. */
static void show_link(const char* label, const char* path) {
  char target[4096];
  ssize_t got = readlink(path, target, sizeof(target) - 1);
  if (got < 0) {
    printf("%s: %s\n", label, errno_name(errno));
    return;
  }
  target[got] = 0;
  print_target(label, target);
}

/* readlink of /proc/<pid>/<rest>, printed as "<label> <rest>". */
static void show_task_link(const char* label, pid_t pid, const char* rest) {
  char path[128], full[160];
  snprintf(path, sizeof(path), "/proc/%d/%s", pid, rest);
  snprintf(full, sizeof(full), "%s %s", label, rest);
  show_link(full, path);
}

/* The errno of a readlink of /proc/<pid>/<rest>, or 0. */
static int link_errno(pid_t pid, const char* rest) {
  char path[128], target[4096];
  snprintf(path, sizeof(path), "/proc/%d/%s", pid, rest);
  return readlink(path, target, sizeof(target)) < 0 ? errno : 0;
}

static void work_path(char* path, size_t size, const char* name) {
  snprintf(path, size, "%s/%s", work, name);
}

/* A child's side: optionally change directory, report that it runs, then
 * obey the parent's commands until it closes the pipe: 'f' fchdir to
 * <dir>/fsub, each answered with a byte. */
static int child_loop(int ready, int hold) {
  if (write(ready, "r", 1) != 1) {
    return 4;
  }
  char command;
  while (read(hold, &command, 1) == 1) {
    if (command == 'f') {
      char path[4200];
      work_path(path, sizeof(path), "fsub");
      int fd = open(path, O_RDONLY | O_DIRECTORY);
      if (fd < 0 || fchdir(fd) != 0) {
        return 5;
      }
      close(fd);
    }
    if (write(ready, "r", 1) != 1) {
      return 4;
    }
  }
  return 0;
}

struct thread_start {
  int hold;
  int report;
};

static void* parked_thread(void* argument) {
  struct thread_start* start = argument;
  pid_t tid = (pid_t)syscall(SYS_gettid);
  if (write(start->report, &tid, sizeof(tid)) != sizeof(tid)) {
    return NULL;
  }
  char byte;
  while (read(start->hold, &byte, 1) > 0) {
  }
  return NULL;
}

/* A child with a second thread, which shares its filesystem information:
 * the main thread changes directory after the thread starts, then reports
 * the thread's number. */
static int threaded_child(int ready, int hold) {
  int report[2];
  if (pipe(report) != 0) {
    return 3;
  }
  struct thread_start start = {dup(hold), report[1]};
  pthread_t thread;
  if (start.hold < 0 || pthread_create(&thread, NULL, parked_thread, &start) != 0) {
    return 3;
  }
  pid_t tid = 0;
  if (read(report[0], &tid, sizeof(tid)) != sizeof(tid)) {
    return 3;
  }
  char sub[4200];
  work_path(sub, sizeof(sub), "sub");
  if (chdir(sub) != 0) {
    return 5;
  }
  if (write(ready, &tid, sizeof(tid)) != sizeof(tid)) {
    return 4;
  }
  char byte;
  while (read(hold, &byte, 1) > 0) {
  }
  close(start.hold);
  pthread_join(thread, NULL);
  return 0;
}

struct spawned {
  pid_t pid;
  int ready;
  int release;
};

/* Fork a child that runs `body` with its pipes; with `directory`, the child
 * enters <dir>/<directory> first; with `image`, it then execs `image` as
 * role `role`, keeping its pipes at their numbers. The parent waits for the
 * child's first report unless `quiet`. */
static struct spawned spawn(const char* directory, const char* image, const char* role,
                            int (*body)(int, int), int quiet) {
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
    if (directory != NULL) {
      char path[4200];
      work_path(path, sizeof(path), directory);
      if (chdir(path) != 0) {
        _exit(125);
      }
    }
    if (image != NULL) {
      if (dup2(ready[1], READY_FD) != READY_FD || dup2(hold[0], HOLD_FD) != HOLD_FD) {
        _exit(126);
      }
      char role_var[64], work_var[4200];
      snprintf(role_var, sizeof(role_var), "LINKS_ROLE=%s", role);
      snprintf(work_var, sizeof(work_var), "LINKS_WORK=%s", work);
      char* argv[] = {"copy", NULL};
      char* envp[] = {role_var, work_var, NULL};
      syscall(SYS_execve, image, argv, envp);
      _exit(127);
    }
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

/* Send the child `command` and wait for its answer. */
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

/* A forked child that entered <dir>/sub. */
static void forked_child(void) {
  struct spawned kid = spawn("sub", NULL, NULL, child_loop, 0);
  if (kid.pid < 0) {
    return;
  }
  show_task_link("forked", kid.pid, "exe");
  show_task_link("forked", kid.pid, "cwd");
  show_task_link("forked", kid.pid, "root");
  char rest[64];
  const char* names[] = {"exe", "cwd", "root"};
  for (int i = 0; i < 3; i++) {
    char label[64], path[128];
    snprintf(label, sizeof(label), "forked task/<pid>/%s", names[i]);
    snprintf(path, sizeof(path), "/proc/%d/task/%d/%s", kid.pid, kid.pid, names[i]);
    show_link(label, path);
  }
  /* The parent is no thread of the child. */
  snprintf(rest, sizeof(rest), "task/%d/cwd", getpid());
  printf("forked task/<parent>/cwd: %s\n", errno_name(link_errno(kid.pid, rest)));
  /* By a name relative to the process's directory. */
  char path[64];
  snprintf(path, sizeof(path), "/proc/%d", kid.pid);
  int directory = open(path, O_RDONLY | O_DIRECTORY);
  if (directory < 0) {
    fail("open the child's /proc directory");
  } else {
    char target[4096];
    ssize_t got = readlinkat(directory, "cwd", target, sizeof(target) - 1);
    if (got < 0) {
      printf("forked readlinkat(<pid>, cwd): %s\n", errno_name(errno));
    } else {
      target[got] = 0;
      print_target("forked readlinkat(<pid>, cwd)", target);
    }
    close(directory);
  }
  /* Cut short by the buffer: the first bytes, no NUL. */
  char whole[4096], cut[8];
  snprintf(path, sizeof(path), "/proc/%d/cwd", kid.pid);
  ssize_t length = readlink(path, whole, sizeof(whole));
  memset(cut, 'x', sizeof(cut));
  ssize_t short_length = readlink(path, cut, 5);
  printf("forked cwd in 5 bytes: length=%zd prefix=%d untouched=%d\n", short_length,
         length >= 5 && short_length == 5 && memcmp(cut, whole, 5) == 0, cut[5] == 'x');
  /* Followed: a directory is no link, the image is no directory. */
  printf("forked cwd/: %s\n", errno_name(link_errno(kid.pid, "cwd/")));
  printf("forked root/: %s\n", errno_name(link_errno(kid.pid, "root/")));
  printf("forked exe/: %s\n", errno_name(link_errno(kid.pid, "exe/")));
  printf("forked root/.: %s\n", errno_name(link_errno(kid.pid, "root/.")));
  printf("forked exe/.: %s\n", errno_name(link_errno(kid.pid, "exe/.")));
  command(kid, 'f');
  show_task_link("forked after fchdir", kid.pid, "cwd");
  finish(kid, "forked child");
}

/* A forked child that stayed in the parent's working directory. */
static void inherited_child(void) {
  struct spawned kid = spawn(NULL, NULL, NULL, child_loop, 0);
  if (kid.pid < 0) {
    return;
  }
  show_task_link("inherited", kid.pid, "cwd");
  show_task_link("inherited", kid.pid, "root");
  finish(kid, "inherited child");
}

/* Copy this program's image to `copy`, executable. */
static int copy_image(const char* copy) {
  const char* self = (const char*)getauxval(AT_EXECFN);
  int in = self == NULL ? -1 : open(self, O_RDONLY);
  int out = open(copy, O_WRONLY | O_CREAT | O_EXCL, 0755);
  if (in < 0 || out < 0) {
    return -1;
  }
  static char buffer[1 << 16];
  for (;;) {
    ssize_t got = read(in, buffer, sizeof(buffer));
    if (got < 0) {
      return -1;
    }
    if (got == 0) {
      break;
    }
    if (write(out, buffer, got) != got) {
      return -1;
    }
  }
  close(in);
  return close(out);
}

/* A child that entered <dir>/sub2 and exec'd a copy of this program, whose
 * copy the parent then renames and removes. */
static void exec_child(void) {
  char copy[4200], moved[4200];
  work_path(copy, sizeof(copy), "copy");
  work_path(moved, sizeof(moved), "moved");
  if (copy_image(copy) != 0) {
    fail("copy the image");
    return;
  }
  struct spawned kid = spawn("sub2", copy, "wait", NULL, 0);
  if (kid.pid < 0) {
    return;
  }
  show_task_link("exec'd", kid.pid, "exe");
  show_task_link("exec'd", kid.pid, "cwd");
  show_task_link("exec'd", kid.pid, "root");
  if (rename(copy, moved) != 0) {
    fail("rename the copy");
  }
  show_task_link("exec'd after rename", kid.pid, "exe");
  if (unlink(moved) != 0) {
    fail("unlink the copy");
  }
  show_task_link("exec'd after unlink", kid.pid, "exe");
  finish(kid, "exec'd child");
}

/* A child's second thread, which shares the child's filesystem information. */
static void threaded(void) {
  int ready[2], hold[2];
  if (pipe(ready) != 0 || pipe(hold) != 0) {
    fail("pipe");
    return;
  }
  pid_t pid = fork();
  if (pid < 0) {
    fail("fork");
    return;
  }
  if (pid == 0) {
    close(ready[0]);
    close(hold[1]);
    _exit(threaded_child(ready[1], hold[0]));
  }
  close(ready[1]);
  close(hold[0]);
  pid_t tid = 0;
  if (read(ready[0], &tid, sizeof(tid)) != sizeof(tid)) {
    fail("thread report");
  } else {
    char rest[64];
    snprintf(rest, sizeof(rest), "task/%d/cwd", tid);
    char label[] = "thread task/<tid>/cwd";
    char path[128];
    snprintf(path, sizeof(path), "/proc/%d/%s", pid, rest);
    show_link(label, path);
    snprintf(path, sizeof(path), "/proc/%d/task/%d/exe", pid, tid);
    show_link("thread task/<tid>/exe", path);
    snprintf(path, sizeof(path), "/proc/%d/task/%d/root", pid, tid);
    show_link("thread task/<tid>/root", path);
    snprintf(path, sizeof(path), "/proc/%d/cwd", tid);
    show_link("thread <tid>/cwd", path);
  }
  struct spawned kid = {pid, ready[0], hold[1]};
  finish(kid, "threaded child");
}

/* A child that has exited, not reaped. */
static void zombie_child(void) {
  struct spawned kid = spawn(NULL, NULL, NULL, NULL, 1);
  if (kid.pid < 0) {
    return;
  }
  siginfo_t info;
  memset(&info, 0, sizeof(info));
  if (waitid(P_PID, kid.pid, &info, WEXITED | WNOWAIT) != 0) {
    fail("waitid zombie");
  }
  printf("zombie exe: %s\n", errno_name(link_errno(kid.pid, "exe")));
  printf("zombie cwd: %s\n", errno_name(link_errno(kid.pid, "cwd")));
  printf("zombie root: %s\n", errno_name(link_errno(kid.pid, "root")));
  finish(kid, "zombie child");
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
  return child_loop(ready, hold);
}

/* A non-dumpable child, read without effective capabilities. */
static void private_child(void) {
  struct spawned kid = spawn("sub", NULL, NULL, private_body, 0);
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
    int answers[3] = {link_errno(kid.pid, "exe"), link_errno(kid.pid, "cwd"),
                      link_errno(kid.pid, "root")};
    _exit(write(report[1], answers, sizeof(answers)) == sizeof(answers) ? 0 : 6);
  }
  close(report[1]);
  int answers[3] = {-1, -1, -1};
  if (read(report[0], answers, sizeof(answers)) != sizeof(answers)) {
    fail("reader report");
  }
  close(report[0]);
  int status;
  waitpid(reader, &status, 0);
  printf("non-dumpable child: exe=%s cwd=%s root=%s\n", errno_name(answers[0]),
         errno_name(answers[1]), errno_name(answers[2]));
  finish(kid, "non-dumpable child");
}

/* A child whose working directory the parent renamed. */
static void renamed_cwd(void) {
  char renamed[4200], sub[4200];
  work_path(sub, sizeof(sub), "sub");
  work_path(renamed, sizeof(renamed), "renamed");
  struct spawned kid = spawn("sub", NULL, NULL, child_loop, 0);
  if (kid.pid < 0) {
    return;
  }
  if (rename(sub, renamed) != 0) {
    fail("rename the working directory");
  }
  show_task_link("renamed", kid.pid, "cwd");
  finish(kid, "renamed child");
  if (rename(renamed, sub) != 0) {
    fail("rename the working directory back");
  }
}

static void make_directory(const char* name) {
  char path[4200];
  work_path(path, sizeof(path), name);
  if (mkdir(path, 0755) != 0) {
    fail(name);
  }
}

static void remove_directory(const char* name) {
  char path[4200];
  work_path(path, sizeof(path), name);
  if (rmdir(path) != 0) {
    fail(name);
  }
}

int main(int argc, char** argv) {
  const char* role = getenv("LINKS_ROLE");
  if (role != NULL) {
    /* The exec'd copy: its pipes are at their numbers. */
    return child_loop(READY_FD, HOLD_FD);
  }
  int renamed = argc == 3 && strcmp(argv[1], "--renamed-cwd") == 0;
  if (argc != 2 && !renamed) {
    fprintf(stderr, "usage: %s [--renamed-cwd] DIRECTORY\n", argv[0]);
    return 2;
  }
  snprintf(work, sizeof(work), "%s/links", argv[argc - 1]);
  ssize_t got = readlink("/proc/self/exe", own_exe, sizeof(own_exe) - 1);
  if (got < 0 || getcwd(own_cwd, sizeof(own_cwd)) == NULL) {
    fail("own exe or cwd");
    return 1;
  }
  own_exe[got] = 0;
  setvbuf(stdout, NULL, _IONBF, 0);
  if (mkdir(work, 0755) != 0) {
    fail("mkdir the work directory");
    return 1;
  }
  make_directory("sub");
  make_directory("sub2");
  make_directory("fsub");
  if (renamed) {
    renamed_cwd();
  } else {
    forked_child();
    inherited_child();
    exec_child();
    threaded();
    zombie_child();
    private_child();
  }
  remove_directory("sub");
  remove_directory("sub2");
  remove_directory("fsub");
  if (rmdir(work) != 0) {
    fail("rmdir the work directory");
  }
  printf("failures=%d\n", failures);
  return failures == 0 ? 0 : 1;
}
