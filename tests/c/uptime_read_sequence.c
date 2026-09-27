/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Reports the virtual uptime as sysinfo(2), /proc/uptime and /proc/stat show
// it, at five points of one run: at start; after sleeping one second and
// re-executing this program; from a second thread; from a forked child; and
// from the main thread at the end. Each report is one line written with one
// write(2), so no line can be lost in a stdio buffer across exec or fork:
//
//   <phase> sysinfo=<s> uptime=<u> idle=<i> btime=<b> cpu0=<c>
//
// where <s> is sysinfo(2) uptime, <u> and <i> are the two /proc/uptime fields
// as printed, <b> is the /proc/stat btime and <c> is the first counter of the
// /proc/stat cpu0 line. With the single argument "sysinfo" the program instead
// prints "sysinfo=<s>" once, before any other system call it makes itself.
//
// hermit-cli/tests/procfs_determinism.rs runs this under epochs that differ
// only in their sub-second fraction and compares the lines.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/sysinfo.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

// Large enough for /proc/stat on hosts with many CPUs and interrupts. Only one
// thread of a process reads at a time: the main thread waits in pthread_join
// while the second thread reports.
static char contents[1 << 20];

static void write_all(int fd, const char *text, size_t length) {
  while (length > 0) {
    ssize_t written = write(fd, text, length);
    if (written < 0 && errno == EINTR) {
      continue;
    }
    if (written <= 0) {
      _exit(3);
    }
    text += written;
    length -= (size_t)written;
  }
}

__attribute__((noreturn)) static void fail(const char *what, int error) {
  char message[256];
  int length = snprintf(
      message,
      sizeof(message),
      "uptime_read_sequence: %s: %s\n",
      what,
      error == 0 ? "unexpected contents" : strerror(error));
  if (length > 0) {
    write_all(
        2,
        message,
        (size_t)length < sizeof(message) ? (size_t)length
                                         : sizeof(message) - 1);
  }
  _exit(2);
}

static size_t read_file(const char *path) {
  int fd = open(path, O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    fail(path, errno);
  }
  size_t length = 0;
  for (;;) {
    if (length == sizeof(contents) - 1) {
      fail(path, EFBIG);
    }
    ssize_t count = read(fd, contents + length, sizeof(contents) - 1 - length);
    if (count < 0 && errno == EINTR) {
      continue;
    }
    if (count < 0) {
      fail(path, errno);
    }
    if (count == 0) {
      break;
    }
    length += (size_t)count;
  }
  if (close(fd) != 0) {
    fail(path, errno);
  }
  contents[length] = '\0';
  return length;
}

// Parses a decimal field that must be followed by `terminator`.
static unsigned long long parse_decimal(const char *text, char terminator) {
  if (*text < '0' || *text > '9') {
    fail("decimal field", 0);
  }
  char *end = NULL;
  errno = 0;
  unsigned long long value = strtoull(text, &end, 10);
  if (errno != 0 || end == text || *end != terminator) {
    fail("decimal field", errno);
  }
  return value;
}

// Returns the text after `prefix` on the line that starts with it.
static const char *line_value(const char *prefix) {
  size_t prefix_length = strlen(prefix);
  for (const char *line = contents; *line != '\0';) {
    if (strncmp(line, prefix, prefix_length) == 0) {
      return line + prefix_length;
    }
    const char *newline = strchr(line, '\n');
    if (newline == NULL) {
      break;
    }
    line = newline + 1;
  }
  fail(prefix, 0);
  return NULL;
}

static void report(const char *phase) {
  struct sysinfo info;
  if (sysinfo(&info) != 0) {
    fail("sysinfo", errno);
  }

  // /proc/uptime must be exactly two space-separated fields and a newline.
  char uptime[64];
  char idle[64];
  size_t length = read_file("/proc/uptime");
  char *space = strchr(contents, ' ');
  char *newline = strchr(contents, '\n');
  if (space == NULL || newline == NULL || space > newline ||
      newline != contents + length - 1 || space == contents ||
      newline == space + 1 ||
      (size_t)(space - contents) >= sizeof(uptime) ||
      (size_t)(newline - space - 1) >= sizeof(idle) ||
      strchr(space + 1, ' ') != NULL) {
    fail("/proc/uptime", 0);
  }
  memcpy(uptime, contents, (size_t)(space - contents));
  uptime[space - contents] = '\0';
  memcpy(idle, space + 1, (size_t)(newline - space - 1));
  idle[newline - space - 1] = '\0';

  read_file("/proc/stat");
  unsigned long long btime = parse_decimal(line_value("btime "), '\n');
  const char *cpu0 = line_value("cpu0 ");
  while (*cpu0 == ' ') {
    cpu0++;
  }
  unsigned long long cpu0_first = parse_decimal(cpu0, ' ');

  char line[512];
  int printed = snprintf(
      line,
      sizeof(line),
      "%s sysinfo=%ld uptime=%s idle=%s btime=%llu cpu0=%llu\n",
      phase,
      (long)info.uptime,
      uptime,
      idle,
      btime,
      cpu0_first);
  if (printed <= 0 || (size_t)printed >= sizeof(line)) {
    fail("report line", 0);
  }
  write_all(1, line, (size_t)printed);
}

static void *thread_main(void *argument) {
  (void)argument;
  report("thread");
  return NULL;
}

int main(int argc, char **argv) {
  if (argc == 2 && strcmp(argv[1], "sysinfo") == 0) {
    struct sysinfo info;
    if (sysinfo(&info) != 0) {
      fail("sysinfo", errno);
    }
    char line[64];
    int printed =
        snprintf(line, sizeof(line), "sysinfo=%ld\n", (long)info.uptime);
    if (printed <= 0 || (size_t)printed >= sizeof(line)) {
      fail("sysinfo line", 0);
    }
    write_all(1, line, (size_t)printed);
    return 0;
  }

  if (argc == 1) {
    report("start");
    struct timespec remaining = {.tv_sec = 1, .tv_nsec = 0};
    while (nanosleep(&remaining, &remaining) != 0) {
      if (errno != EINTR) {
        fail("nanosleep", errno);
      }
    }
    char *exec_argv[] = {argv[0], "exec", NULL};
    execv(argv[0], exec_argv);
    fail("execv", errno);
  }

  if (argc != 2 || strcmp(argv[1], "exec") != 0) {
    fail("usage: uptime_read_sequence [sysinfo]", EINVAL);
  }

  report("exec");

  pthread_t thread;
  int error = pthread_create(&thread, NULL, thread_main, NULL);
  if (error != 0) {
    fail("pthread_create", error);
  }
  error = pthread_join(thread, NULL);
  if (error != 0) {
    fail("pthread_join", error);
  }

  pid_t child = fork();
  if (child < 0) {
    fail("fork", errno);
  }
  if (child == 0) {
    report("child");
    _exit(0);
  }
  int status = 0;
  while (waitpid(child, &status, 0) != child) {
    if (errno != EINTR) {
      fail("waitpid", errno);
    }
  }
  if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    fail("child status", 0);
  }

  report("end");
  return 0;
}
