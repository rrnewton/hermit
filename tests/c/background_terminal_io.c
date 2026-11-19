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
 *
 * With the argument "non-utf8-name", each child first names its thread with
 * bytes that are not UTF-8 (PR_SET_NAME allows any bytes but NUL, and the
 * thread's /proc status shows them on its Name: line), and says so in a line
 * of its own before its call.
 *
 * With the argument "settings", there is one child instead, which changes the
 * terminal's settings (tcsetattr) from its background group: Linux stops it
 * with SIGTTOU whether or not TOSTOP is set, and where SIGTTOU is ignored
 * the change goes through.
 *
 * With the argument "sibling", there is only the reader, and it first starts
 * a sibling in its own background group, which waits to read a pipe whose
 * write end only the reader holds; with "sibling-poll", the sibling waits for
 * the pipe in poll instead. Linux sends the reader's SIGTTIN to the whole
 * group, so the sibling's call, which cannot raise the signal itself, is
 * interrupted and the sibling stopped too. Once its own read is done, the
 * reader writes to the pipe, gives the sibling a second to act, then kills
 * it: a SIGCONT would discard a stop signal still pending, so it is not
 * sent. On Linux the sibling, still stopped, never sees the write.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/prctl.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

/* Whether each child names its thread with bytes that are not UTF-8. */
static int non_utf8_name;
/* Whether the one child changes the terminal's settings instead. */
static int settings_only;
/* Whether the reader, the only child, has a sibling in its group. */
static int with_sibling;
/* Whether that sibling waits in poll rather than in read. */
static int sibling_polls;

static void fail(const char *what) {
  printf("FAIL %s: %s\n", what, strerror(errno));
  _exit(1);
}

/* The reader's sibling, in the reader's background group: it waits for the
 * pipe `input`, whose write end only the reader holds, in one read or one
 * poll, then prints how that call ended. */
static void sibling_wait(int input) {
  if (sibling_polls) {
    struct pollfd ready = {.fd = input, .events = POLLIN, .revents = 0};
    int got = poll(&ready, 1, -1);
    if (got < 0) {
      printf("sibling: poll failed: %s\n", strerror(errno));
    } else {
      printf("sibling: poll returned %d\n", got);
    }
  } else {
    char line[16];
    ssize_t got = read(input, line, sizeof line);
    if (got < 0) {
      printf("sibling: read failed: %s\n", strerror(errno));
    } else {
      printf("sibling: read %zd bytes\n", got);
    }
  }
  _exit(0);
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
  if (non_utf8_name) {
    /* A lone continuation byte and 0xff: neither starts a UTF-8 sequence. */
    if (prctl(PR_SET_NAME, "\x80\xff-call", 0, 0, 0) != 0) {
      fail("PR_SET_NAME");
    }
    printf("%s: thread name is not UTF-8\n", what);
  }
  if (strcmp(what, "settings") == 0) {
    struct termios settings;
    /* Reading the settings raises nothing; setting them does. */
    if (tcgetattr(tty, &settings) != 0) {
      fail("tcgetattr in the child");
    }
    if (tcsetattr(tty, TCSANOW, &settings) != 0) {
      printf("%s: failed: %s\n", what, strerror(errno));
    } else {
      printf("%s: changed\n", what);
    }
    _exit(0);
  }
  pid_t sibling = -1;
  int input[2] = {-1, -1};
  if (with_sibling) {
    if (pipe(input) != 0) {
      fail("pipe for the sibling");
    }
    sibling = fork();
    if (sibling < 0) {
      fail("fork the sibling");
    }
    if (sibling == 0) {
      close(input[1]);
      sibling_wait(input[0]);
    }
    /* Time for the sibling to start waiting. */
    struct timespec pause = {0, 100 * 1000 * 1000};
    nanosleep(&pause, NULL);
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
  if (with_sibling) {
    /* Wake the sibling, give it a second to act on its signal, then end
     * it. */
    if (write(input[1], "x", 1) != 1) {
      fail("write to the sibling's pipe");
    }
    struct timespec pause = {1, 0};
    nanosleep(&pause, NULL);
    if (kill(sibling, SIGKILL) != 0) {
      fail("kill the sibling");
    }
    int status;
    if (waitpid(sibling, &status, 0) != sibling) {
      fail("waitpid for the sibling");
    }
    if (WIFSIGNALED(status)) {
      printf("%s: sibling killed by signal %d\n", what, WTERMSIG(status));
    } else {
      printf("%s: sibling exited %d\n", what, WIFEXITED(status) ? WEXITSTATUS(status) : -1);
    }
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
  if (settings_only) {
    run_child(tty, "settings");
    _exit(0);
  }
  /* The reader's line, waiting in the terminal's input queue. */
  if (write(master, "in\n", 3) != 3) {
    fail("write to the master");
  }
  run_child(tty, "read");
  if (!with_sibling) {
    run_child(tty, "write");
  }
  _exit(0);
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  if (argc > 2 || (argc == 2 && strcmp(argv[1], "non-utf8-name") != 0 &&
                   strcmp(argv[1], "settings") != 0 && strcmp(argv[1], "sibling") != 0 &&
                   strcmp(argv[1], "sibling-poll") != 0)) {
    printf("FAIL usage: background_terminal_io [non-utf8-name|settings|sibling|sibling-poll]\n");
    return 2;
  }
  non_utf8_name = argc == 2 && strcmp(argv[1], "non-utf8-name") == 0;
  settings_only = argc == 2 && strcmp(argv[1], "settings") == 0;
  sibling_polls = argc == 2 && strcmp(argv[1], "sibling-poll") == 0;
  with_sibling = sibling_polls || (argc == 2 && strcmp(argv[1], "sibling") == 0);
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
