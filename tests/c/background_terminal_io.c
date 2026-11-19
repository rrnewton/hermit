/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A background read and a background write on the controlling terminal.
 *
 * The guest opens a pseudoterminal. A session leader makes it its controlling
 * terminal, with TOSTOP set, and starts two children, each in a background
 * process group of its own:
 *   - one reads the terminal: Linux stops it with SIGTTIN;
 *   - one writes to it: with TOSTOP, Linux stops it with SIGTTOU.
 * The leader waits for each stop, gives the terminal to the stopped child's
 * group and continues the child, and the child's call then completes in the
 * foreground. Where the signal is ignored, the read fails with EIO instead,
 * and the write goes through without a stop.
 *
 * Every step prints one line, so a run under Hermit can be compared with a
 * native run line by line. The first line names the dispositions the guest
 * started with. The guest continues each stopped child by its pid: each child
 * is alone in its group, and Detcore refuses signals sent to a group.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <termios.h>
#include <unistd.h>

static void fail(const char *what) {
  printf("FAIL %s: %s\n", what, strerror(errno));
  _exit(1);
}

static const char *disposition(int signal) {
  struct sigaction action;
  if (sigaction(signal, NULL, &action) != 0) {
    fail("sigaction");
  }
  if (action.sa_handler == SIG_IGN) {
    return "ignored";
  }
  return action.sa_handler == SIG_DFL ? "default" : "caught";
}

static const char *signal_name(int signal) {
  switch (signal) {
    case SIGTTIN:
      return "SIGTTIN";
    case SIGTTOU:
      return "SIGTTOU";
    case SIGTSTP:
      return "SIGTSTP";
    case SIGSTOP:
      return "SIGSTOP";
    default:
      return "another signal";
  }
}

/* The child's one call, from a background process group of its own. */
static void background_call(int tty, const char *what) {
  if (setpgid(0, 0) != 0) {
    fail("setpgid in the child");
  }
  ssize_t done;
  if (strcmp(what, "write") == 0) {
    done = write(tty, "out\n", 4);
  } else {
    char line[16];
    done = read(tty, line, sizeof line);
  }
  if (done < 0) {
    printf("%s: failed: %s\n", what, strerror(errno));
  } else {
    printf("%s: %zd bytes\n", what, done);
  }
  _exit(0);
}

/* Starts the child, reports how its call first ended, and lets a stopped
 * child finish in the foreground. */
static void run_child(int tty, const char *what) {
  pid_t child = fork();
  if (child < 0) {
    fail("fork");
  }
  if (child == 0) {
    background_call(tty, what);
  }
  /* Both sides set the group, so neither waits for the other. */
  if (setpgid(child, child) != 0 && errno != EACCES) {
    fail("setpgid in the leader");
  }
  int status;
  if (waitpid(child, &status, WUNTRACED) != child) {
    fail("waitpid");
  }
  if (WIFSTOPPED(status)) {
    printf("%s: stopped by %s\n", what, signal_name(WSTOPSIG(status)));
    if (tcsetpgrp(tty, child) != 0) {
      fail("tcsetpgrp to the child");
    }
    if (kill(child, SIGCONT) != 0) {
      fail("kill SIGCONT");
    }
    if (waitpid(child, &status, 0) != child) {
      fail("waitpid after SIGCONT");
    }
    /* Taking the terminal back from the background sends SIGTTOU unless it
     * is blocked. */
    sigset_t ttou;
    sigset_t previous;
    sigemptyset(&ttou);
    sigaddset(&ttou, SIGTTOU);
    sigprocmask(SIG_BLOCK, &ttou, &previous);
    if (tcsetpgrp(tty, getpgrp()) != 0) {
      fail("tcsetpgrp back to the leader");
    }
    sigprocmask(SIG_SETMASK, &previous, NULL);
  }
  if (WIFEXITED(status)) {
    printf("%s: child exited %d\n", what, WEXITSTATUS(status));
  } else {
    printf("%s: child ended with wait status 0x%x\n", what, status);
  }
}

static void run_session(int master, const char *name) {
  if (setsid() < 0) {
    fail("setsid");
  }
  int tty = open(name, O_RDWR | O_NOCTTY);
  if (tty < 0) {
    fail("open the terminal");
  }
  if (ioctl(tty, TIOCSCTTY, 0) != 0) {
    fail("TIOCSCTTY");
  }
  struct termios settings;
  if (tcgetattr(tty, &settings) != 0) {
    fail("tcgetattr");
  }
  settings.c_lflag |= TOSTOP;
  if (tcsetattr(tty, TCSANOW, &settings) != 0) {
    fail("tcsetattr");
  }
  /* The reader's line, waiting in the terminal's input queue. */
  if (write(master, "in\n", 3) != 3) {
    fail("write to the master");
  }
  run_child(tty, "read");
  run_child(tty, "write");
  _exit(0);
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  printf("started with SIGTTIN %s, SIGTTOU %s\n", disposition(SIGTTIN),
         disposition(SIGTTOU));
  int master = posix_openpt(O_RDWR | O_NOCTTY);
  if (master < 0 || grantpt(master) != 0 || unlockpt(master) != 0) {
    fail("posix_openpt");
  }
  const char *name = ptsname(master);
  if (name == NULL) {
    fail("ptsname");
  }
  pid_t session = fork();
  if (session < 0) {
    fail("fork the session");
  }
  if (session == 0) {
    run_session(master, name);
  }
  int status;
  if (waitpid(session, &status, 0) != session) {
    fail("waitpid for the session");
  }
  printf("session: exited %d\n", WIFEXITED(status) ? WEXITSTATUS(status) : -1);
  return 0;
}
