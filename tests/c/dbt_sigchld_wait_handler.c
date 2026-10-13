/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <setjmp.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <ucontext.h>
#include <unistd.h>

/* Linux x86-64 rt_sigframe starts with pretcode, then ucontext. Read only
 * the ABI-defined general registers and the first 64 signal-mask bits; the
 * larger libc fpstate/sigset storage is not assumed present in the frame.
 * The assembly handler obtains this address from its actual entry RSP. */
struct handler_frame {
  void (*restorer)(void);
  ucontext_t context;
};
_Static_assert(sizeof(greg_t) == 8 && REG_RIP == 16, "x86-64 greg ABI");
_Static_assert(offsetof(ucontext_t, uc_mcontext) == 40, "x86-64 mcontext ABI");
_Static_assert(offsetof(ucontext_t, uc_sigmask) == 296, "x86-64 mask ABI");
_Static_assert(offsetof(struct handler_frame, context) == 8, "rt frame ABI");

extern void wait_handler_entry(int);
extern long inspected_wait4(long, int *, struct rusage *);
extern char inspected_wait4_instruction[], inspected_wait4_fallthrough[];
volatile sig_atomic_t wait_returned;
uintptr_t wait_original_sp;
static volatile sig_atomic_t handlers, depth, frame_valid, transfer_seen;
static int release_b = -1, target_status;
static struct rusage target_usage;
static pid_t first, second;
static sigset_t original_mask;
static struct sigaction installed;
static sigjmp_buf escape;
static int mode; /* 0 flags0, 1 restart, 2 longjmp, 3 exec, 4 exit */
static char *exec_args[5];
static char *exec_env[] = {"PATH=/usr/bin:/bin", NULL};
static char first_text[32], second_text[32];

static void require(int ok, const char *message) {
  if (!ok) {
    fprintf(stderr, "wait-handler failure: %s\n", message);
    exit(90);
  }
}

static void handler_require(int ok) {
  if (!ok) {
    static const char failure[] = "wait-handler frame/control failure\n";
    (void)write(STDERR_FILENO, failure, sizeof(failure) - 1);
    _exit(91);
  }
}

static int same_mask(const sigset_t *left, const sigset_t *right) {
  for (int sig = 1; sig <= 64; ++sig) {
    int a = sigismember(left, sig), b = sigismember(right, sig);
    if (a < 0 || b < 0 || a != b)
      return 0;
  }
  return 1;
}

static void retain_frame(const greg_t *r) {
  /* Fixed-width, allocation-free diagnostic, safe inside the real handler.
   * Order: RAX, RIP, RSP, RDI, RSI, RDX, R10, R8, R9. These are raw evidence,
   * not a canonical comparator or input to the product. */
  static const int fields[] = {REG_RAX, REG_RIP, REG_RSP, REG_RDI, REG_RSI,
                               REG_RDX, REG_R10, REG_R8, REG_R9};
  static const char hex[] = "0123456789abcdef";
  char text[9 * 17 + 1];
  size_t cursor = 0;
  for (size_t i = 0; i < sizeof(fields) / sizeof(fields[0]); ++i) {
    uint64_t value = (uint64_t)r[fields[i]];
    for (int shift = 60; shift >= 0; shift -= 4)
      text[cursor++] = hex[(value >> shift) & 15];
    text[cursor++] = i == 8 ? '\n' : ' ';
  }
  handler_require(write(STDERR_FILENO, text, cursor) == (ssize_t)cursor);
}

void wait_handler_inspect(int signal, const struct handler_frame *frame) {
  int saved_errno = errno;
  sigset_t current;
  ++depth;
  handler_require(signal == SIGCHLD && depth == 1);
  handler_require(sigprocmask(SIG_SETMASK, NULL, &current) == 0 &&
                  sigismember(&current, SIGCHLD) == 1 &&
                  sigismember(&current, SIGUSR1) == 1 &&
                  sigismember(&current, SIGUSR2) == 1);
  ++handlers;
  if (handlers == 1) {
    const greg_t *r = frame->context.uc_mcontext.gregs;
    retain_frame(r);
    handler_require(wait_returned == 0 &&
                    frame->restorer == installed.sa_restorer &&
                    same_mask(&frame->context.uc_sigmask, &original_mask));
    handler_require(r[REG_RDI] == second &&
                    (uintptr_t)r[REG_RSI] == (uintptr_t)&target_status &&
                    r[REG_RDX] == 0 &&
                    (uintptr_t)r[REG_R10] == (uintptr_t)&target_usage &&
                    r[REG_R8] == 0x13579bdf && r[REG_R9] == 0x2468ace0 &&
                    (uintptr_t)r[REG_RSP] == wait_original_sp);
    handler_require(r[REG_RAX] == (mode == 0 ? -EINTR : SYS_wait4) &&
                    (uintptr_t)r[REG_RIP] ==
                        (uintptr_t)(mode == 0 ? inspected_wait4_fallthrough
                                              : inspected_wait4_instruction));
    frame_valid = 1;
    static const char entered[] = "wait-handler frame-valid\n";
    handler_require(write(STDOUT_FILENO, entered, sizeof(entered) - 1) ==
                    (ssize_t)(sizeof(entered) - 1));
    /* B is alive until the authentic A handler has checked the real frame.
     * A's active frame cannot coalesce with B's later publication. */
    handler_require(write(release_b, "B", 1) == 1);
    if (mode >= 2) {
      transfer_seen = 1;
      --depth;
      errno = saved_errno;
      if (mode == 2)
        siglongjmp(escape, 77);
      if (mode == 3) {
        static const char message[] = "wait-handler transfer exec\n";
        handler_require(write(STDOUT_FILENO, message, sizeof(message) - 1) ==
                        (ssize_t)(sizeof(message) - 1));
        /* exec preserves the mask; explicitly choose the pre-handler mask.
         * Successful exec resets the caught disposition and discards this
         * stack, so no old private wait continuation may run afterward. */
        handler_require(sigprocmask(SIG_SETMASK, &original_mask, NULL) == 0);
        (void)syscall(SYS_execve, exec_args[0], exec_args, exec_env);
        handler_require(0);
      }
      static const char message[] = "wait-handler transfer exit\n";
      handler_require(write(STDOUT_FILENO, message, sizeof(message) - 1) ==
                      (ssize_t)(sizeof(message) - 1));
      (void)syscall(SYS_exit_group, 63);
      handler_require(0);
    }
  }
  --depth;
  errno = saved_errno;
}

static long reap(pid_t pid, int expected) {
  int status = 0;
  long result;
  do {
    result = waitpid(pid, &status, 0);
  } while (result < 0 && errno == EINTR);
  require(result == pid && WIFEXITED(status) && WEXITSTATUS(status) == expected,
          "exact child status and wait identity");
  return result;
}

static void read_byte(int fd, char expected) {
  char byte = 0;
  ssize_t result;
  do {
    result = read(fd, &byte, 1);
  } while (result < 0 && errno == EINTR);
  require(result == 1 && byte == expected, "exact release byte, never EOF");
}

static pid_t parse_pid(const char *text) {
  char *end = NULL;
  long value = strtol(text, &end, 10);
  require(end && *end == 0 && value > 0 && (pid_t)value == value,
          "exec carries exact logical child identity");
  return (pid_t)value;
}

int main(int argc, char **argv) {
  require(argc >= 2, "literal mode");
  if (strcmp(argv[1], "exec-complete") == 0) {
    require(argc == 4, "exec child identities");
    struct sigaction action;
    sigset_t mask, expected;
    require(sigemptyset(&expected) == 0 && sigaddset(&expected, SIGUSR2) == 0 &&
                sigprocmask(SIG_SETMASK, NULL, &mask) == 0 &&
                same_mask(&mask, &expected) &&
                sigaction(SIGCHLD, NULL, &action) == 0 &&
                action.sa_handler == SIG_DFL && wait_returned == 0,
            "exec resets caught action and image state, preserves selected mask");
    reap(parse_pid(argv[3]), 47);
    reap(parse_pid(argv[2]), 31);
    puts("wait-handler completed exec second=47 first=31 old-wait-returned=0");
    return 0;
  }
  require(argc == 2, "one original mode");
  const char *names[] = {"frame-interrupt", "frame-restart", "longjmp", "exec", "exit"};
  for (mode = 0; mode < 5 && strcmp(argv[1], names[mode]) != 0; ++mode) {}
  require(mode < 5, "recognized mode");
  struct sigaction requested;
  memset(&requested, 0, sizeof(requested));
  requested.sa_handler = wait_handler_entry;
  requested.sa_flags = mode == 0 ? 0 : SA_RESTART;
  require(sigemptyset(&requested.sa_mask) == 0 &&
              sigaddset(&requested.sa_mask, SIGUSR1) == 0 &&
              sigaction(SIGCHLD, &requested, NULL) == 0 &&
              sigaction(SIGCHLD, NULL, &installed) == 0 &&
              installed.sa_handler == wait_handler_entry &&
              (installed.sa_flags & (SA_SIGINFO | SA_NODEFER | SA_RESETHAND |
                                     SA_NOCLDWAIT | SA_ONSTACK)) == 0 &&
              !!(installed.sa_flags & SA_RESTART) == (mode != 0),
          "stable one-argument caught disposition");
  require(sigemptyset(&original_mask) == 0 &&
              sigaddset(&original_mask, SIGUSR2) == 0 &&
              sigprocmask(SIG_SETMASK, &original_mask, NULL) == 0,
          "original guest signal mask");
  int channel[2];
  require(pipe(channel) == 0, "dedicated handler-to-B pipe");
  first = fork();
  require(first >= 0, "fork A");
  if (first == 0) {
    require(close(channel[0]) == 0 && close(channel[1]) == 0, "A closes B pipe");
    read_byte(STDIN_FILENO, 'A');
    _exit(31);
  }
  second = fork();
  require(second >= 0, "fork B");
  if (second == 0) {
    require(close(STDIN_FILENO) == 0 && close(channel[1]) == 0,
            "B cannot consume A's input or release itself");
    read_byte(channel[0], 'B');
    _exit(47);
  }
  require(close(STDIN_FILENO) == 0 && close(channel[0]) == 0,
          "parent owns only B writer");
  release_b = channel[1];
  require(snprintf(first_text, sizeof(first_text), "%ld", (long)first) > 0 &&
              snprintf(second_text, sizeof(second_text), "%ld", (long)second) > 0,
          "precompute exec arguments outside handler");
  exec_args[0] = argv[0];
  exec_args[1] = "exec-complete";
  exec_args[2] = first_text;
  exec_args[3] = second_text;
  exec_args[4] = NULL;
  printf("wait-interruption ready parent=%ld first=%ld second=%ld\n",
         (long)getpid(), (long)first, (long)second);
  require(fflush(stdout) == 0, "publish logical identities");
  int jumped = sigsetjmp(escape, 1);
  if (jumped == 0) {
    long result = inspected_wait4(second, &target_status, &target_usage);
    require(mode < 2 && wait_returned == 1 && frame_valid,
            "non-returning handler must not resume old wait");
    if (mode == 0) {
      require(result == -EINTR, "flags0 raw wait returns literal EINTR");
      reap(second, 47);
    } else {
      require(result == second && WIFEXITED(target_status) &&
                  WEXITSTATUS(target_status) == 47,
              "SA_RESTART resumes exact B wait after the real handler");
    }
  } else {
    require(mode == 2 && jumped == 77 && transfer_seen && wait_returned == 0 &&
                frame_valid && depth == 0,
            "siglongjmp abandoned the pending original wait");
    sigset_t mask;
    require(sigprocmask(SIG_SETMASK, NULL, &mask) == 0 &&
                same_mask(&mask, &original_mask),
            "siglongjmp restores saved mask before later syscalls");
    reap(second, 47);
  }
  reap(first, 31);
  sigset_t mask;
  struct sigaction action;
  require(sigprocmask(SIG_SETMASK, NULL, &mask) == 0 &&
              same_mask(&mask, &original_mask) &&
              sigaction(SIGCHLD, NULL, &action) == 0 &&
              action.sa_handler == installed.sa_handler &&
              action.sa_flags == installed.sa_flags &&
              action.sa_restorer == installed.sa_restorer &&
              same_mask(&action.sa_mask, &installed.sa_mask) && handlers == 2 &&
              depth == 0 && close(release_b) == 0,
          "complete mask/action and two separate depth-one handlers");
  printf("wait-handler completed %s second=47 first=31 old-wait-returned=%d\n",
         names[mode], (int)wait_returned);
  return 0;
}
