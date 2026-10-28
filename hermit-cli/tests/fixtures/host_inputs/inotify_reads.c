/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Watches the directory it runs from with inotify, reads the file F there,
// runs the program "child" from there (this source built with that
// directory's ld.so as its program interpreter; with the argument "child" it
// exits at once), and prints the events the watch reported, one per line: the
// name the event is for, then its flags. Hermit must add none by recording
// the files the guest opened, or the interpreter the kernel mapped for the
// exec, which it once did by reading them while the guest ran.
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/wait.h>
#include <unistd.h>

static const struct {
  unsigned mask;
  const char* name;
} FLAGS[] = {
    {IN_ACCESS, "ACCESS"},
    {IN_MODIFY, "MODIFY"},
    {IN_ATTRIB, "ATTRIB"},
    {IN_CLOSE_WRITE, "CLOSE_WRITE"},
    {IN_CLOSE_NOWRITE, "CLOSE_NOWRITE"},
    {IN_OPEN, "OPEN"},
    {IN_MOVED_FROM, "MOVED_FROM"},
    {IN_MOVED_TO, "MOVED_TO"},
    {IN_CREATE, "CREATE"},
    {IN_DELETE, "DELETE"},
};

int main(int argc, char** argv) {
  if (argc > 1 && strcmp(argv[1], "child") == 0) {
    return 0;
  }
  char self[PATH_MAX];
  if (!realpath(argv[0], self)) {
    return 2;
  }
  char dir[PATH_MAX];
  memcpy(dir, self, sizeof dir);
  *strrchr(dir, '/') = '\0';
  int notify = inotify_init1(IN_NONBLOCK);
  if (notify < 0 || inotify_add_watch(notify, dir, IN_ALL_EVENTS) < 0) {
    return 3;
  }
  char path[PATH_MAX + 2];
  char bytes[64];
  snprintf(path, sizeof path, "%s/F", dir);
  int fd = open(path, O_RDONLY);
  if (fd < 0 || read(fd, bytes, sizeof bytes) < 0 || close(fd) < 0) {
    return 4;
  }
  char program[PATH_MAX + 8];
  snprintf(program, sizeof program, "%s/child", dir);
  pid_t child = fork();
  if (child == 0) {
    execl(program, program, "child", (char*)NULL);
    _exit(5);
  }
  int status;
  if (child < 0 || waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    return 6;
  }
  char events[4096]
      __attribute__((aligned(__alignof__(struct inotify_event))));
  ssize_t length;
  while ((length = read(notify, events, sizeof events)) > 0) {
    for (char* at = events; at < events + length;) {
      const struct inotify_event* event = (const struct inotify_event*)at;
      printf("%s", event->len ? event->name : ".");
      for (size_t i = 0; i < sizeof FLAGS / sizeof FLAGS[0]; i++) {
        if (event->mask & FLAGS[i].mask) {
          printf(" %s", FLAGS[i].name);
        }
      }
      printf("\n");
      at += sizeof *event + event->len;
    }
  }
  return length < 0 && errno != EAGAIN ? 7 : 0;
}
