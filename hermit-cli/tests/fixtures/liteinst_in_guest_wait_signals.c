/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Signals that arrive while Detcore is blocked in a guest's wait4.
 *
 * Detcore blocks every signal it can around a blocking wait4 and restores the
 * guest's mask afterwards. Under in-guest LiteInst both of those are real
 * rt_sigprocmask calls made by the guest preload, so each mode below checks
 * what the guest observes when a signal lands inside that window. Every mode
 * prints one line per step and must print the same lines natively and under
 * every backend, except for the one mask line that block-all prints.
 *
 * Each child that signals the parent first waits for a "go" byte, which the
 * parent writes immediately before its wait4. That makes it likely, but not
 * certain, that the signal arrives while the parent is inside the wait; no
 * mode's output depends on which way that race goes.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static void say(const char* format, ...) {
  char line[256];
  va_list args;
  va_start(args, format);
  int length = vsnprintf(line, sizeof(line), format, args);
  va_end(args);
  if (length < 0 || (size_t)length >= sizeof(line))
    _exit(90);
  if (write(STDOUT_FILENO, line, (size_t)length) != length)
    _exit(91);
}

static void fail(const char* what) {
  char line[256];
  int length =
      snprintf(line, sizeof(line), "FAIL %s: %s\n", what, strerror(errno));
  if (length > 0)
    (void)!write(STDERR_FILENO, line, (size_t)length);
  _exit(1);
}

/* The kernel's one-word signal set, read and written with the raw syscall so
 * libc's handling of its reserved signals does not hide anything. */
static long raw_sigprocmask(int how, const uint64_t* set, uint64_t* old) {
  return syscall(SYS_rt_sigprocmask, how, set, old, sizeof(uint64_t));
}

static uint64_t current_mask(void) {
  uint64_t mask = 0;
  if (raw_sigprocmask(SIG_BLOCK, NULL, &mask) != 0)
    fail("read mask");
  return mask;
}

static void read_byte(int fd) {
  char byte;
  ssize_t got;
  do {
    got = read(fd, &byte, 1);
  } while (got < 0 && errno == EINTR);
  if (got != 1)
    fail("read go byte");
}

static void write_byte(int fd) {
  if (write(fd, "g", 1) != 1)
    fail("write go byte");
}

/* Waits for `child` and returns its exit status, retrying EINTR. */
static int wait_exit(pid_t child) {
  int status = 0;
  pid_t got;
  do {
    got = wait4(child, &status, 0, NULL);
  } while (got < 0 && errno == EINTR);
  if (got != child)
    fail("wait4");
  if (!WIFEXITED(status))
    fail("child did not exit normally");
  return WEXITSTATUS(status);
}

static void set_disposition(int signal_number, void (*handler)(int)) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = handler;
  sigemptyset(&action.sa_mask);
  if (sigaction(signal_number, &action, NULL) != 0)
    fail("sigaction");
}

/* (a) A default-ignore SIGCHLD from one child while the parent waits for
 * another child that is not ready yet. The slow child exits only once the fast
 * child is gone (it reads a pipe that only the fast child holds open). */
static int sigchld_during_wait(void) {
  int go[2];
  int fast_gone[2];
  if (pipe(go) != 0 || pipe(fast_gone) != 0)
    fail("pipe");
  pid_t fast = fork();
  if (fast < 0)
    fail("fork fast");
  if (fast == 0) {
    close(go[1]);
    close(fast_gone[0]);
    read_byte(go[0]);
    _exit(3);
  }
  close(fast_gone[1]);
  pid_t slow = fork();
  if (slow < 0)
    fail("fork slow");
  if (slow == 0) {
    close(go[0]);
    close(go[1]);
    char byte;
    while (read(fast_gone[0], &byte, 1) < 0 && errno == EINTR) {
    }
    _exit(5);
  }
  close(go[0]);
  close(fast_gone[0]);
  write_byte(go[1]);
  close(go[1]);
  int slow_status = wait_exit(slow);
  int fast_status = wait_exit(fast);
  say("sigchld-during-wait slow=%d fast=%d\n", slow_status, fast_status);
  say("mask-after=%#llx\n", (unsigned long long)current_mask());
  return 0;
}

/* Forks a child that waits for the go byte, sends `signal_number` to the
 * parent, and then either exits with 5 or, when `outlive_parent` is set, waits
 * until the parent is gone. Writes the go byte and returns the child. */
static pid_t fork_signaller(int signal_number, int outlive_parent) {
  int go[2];
  int parent_alive[2];
  if (pipe(go) != 0 || pipe(parent_alive) != 0)
    fail("pipe");
  pid_t parent = getpid();
  pid_t child = fork();
  if (child < 0)
    fail("fork signaller");
  if (child == 0) {
    close(go[1]);
    close(parent_alive[1]);
    read_byte(go[0]);
    if (kill(parent, signal_number) != 0)
      fail("kill parent");
    if (outlive_parent) {
      char byte;
      while (read(parent_alive[0], &byte, 1) < 0 && errno == EINTR) {
      }
    }
    _exit(5);
  }
  close(go[0]);
  close(parent_alive[0]);
  /* The parent keeps parent_alive[1] open for the rest of its life. */
  write_byte(go[1]);
  close(go[1]);
  return child;
}

/* (a) A SIG_IGN signal while the parent waits for a child that is not ready. */
static int ignored_during_wait(void) {
  set_disposition(SIGUSR1, SIG_IGN);
  pid_t child = fork_signaller(SIGUSR1, 0);
  int status = wait_exit(child);
  say("ignored-during-wait child=%d\n", status);
  say("mask-after=%#llx\n", (unsigned long long)current_mask());
  return 0;
}

/* (a) A signal whose default action terminates the process, sent while the
 * parent waits for a child that will not exit until the parent is gone. Linux
 * kills the parent inside its wait4; a runtime that held the signal until the
 * child exited would deadlock instead. */
static int terminate_during_wait(void) {
  say("terminate-during-wait waiting\n");
  pid_t child = fork_signaller(SIGUSR1, 1);
  int status = wait_exit(child);
  say("UNEXPECTED wait4 returned child=%d\n", status);
  return 2;
}

/* (b) The child signals and then exits at once, so the parent may find it
 * ready and reap it before the signal is acted on, and the signal is then
 * delivered when Detcore restores the guest's mask. Either way the parent
 * must die before wait4 returns to it. */
static int terminate_then_exit(void) {
  say("terminate-then-exit waiting\n");
  pid_t child = fork_signaller(SIGUSR1, 0);
  int status = wait_exit(child);
  say("UNEXPECTED wait4 returned child=%d\n", status);
  return 2;
}

/* (c) A signal the guest itself blocks stays pending through the wait and the
 * mask restore, and is still there to be taken afterwards. */
static int guest_blocked(void) {
  sigset_t usr2;
  sigemptyset(&usr2);
  sigaddset(&usr2, SIGUSR2);
  if (sigprocmask(SIG_BLOCK, &usr2, NULL) != 0)
    fail("block SIGUSR2");
  pid_t child = fork_signaller(SIGUSR2, 0);
  int status = wait_exit(child);
  sigset_t pending;
  if (sigpending(&pending) != 0)
    fail("sigpending");
  int was_pending = sigismember(&pending, SIGUSR2);
  struct timespec zero = {0, 0};
  int taken = sigtimedwait(&usr2, NULL, &zero);
  if (taken < 0)
    fail("sigtimedwait");
  if (sigpending(&pending) != 0)
    fail("sigpending after");
  say("guest-blocked child=%d pending=%d taken=%d pending-after=%d\n", status,
      was_pending, taken, sigismember(&pending, SIGUSR2));
  say("mask-after=%#llx\n", (unsigned long long)current_mask());
  return 0;
}

/* (d) The guest blocks every signal itself, through Detcore's rt_sigprocmask
 * handler, and then waits. Detcore's own block and restore must leave the
 * guest's mask exactly as the guest set it, and a signal sent during the wait
 * must still be pending afterwards. Also checks the how=-1 probe. The mask it
 * prints differs by backend, because each backend keeps some signals for
 * itself; the test that runs this mode names every such bit. */
static int block_all(void) {
  const uint64_t everything = ~UINT64_C(0);
  uint64_t before = current_mask();

  /* Linux validates `how` only when a set is supplied. */
  uint64_t probe = 0;
  long probe_result = raw_sigprocmask(-1, NULL, &probe);
  say("how=-1 without a set: result=%ld old-matches=%d\n", probe_result,
      probe == before);
  errno = 0;
  long bad_how = raw_sigprocmask(-1, &everything, NULL);
  say("how=-1 with a set: result=%ld errno=%s\n", bad_how,
      bad_how == 0 ? "none" : strerrorname_np(errno));
  say("mask unchanged by the refused call=%d\n", current_mask() == before);

  if (raw_sigprocmask(SIG_SETMASK, &everything, NULL) != 0)
    fail("block all");
  uint64_t blocked = current_mask();
  say("block-all mask=%#llx\n", (unsigned long long)blocked);

  pid_t child = fork_signaller(SIGUSR1, 0);
  int status = wait_exit(child);
  say("block-all child=%d mask-kept=%d\n", status, current_mask() == blocked);

  sigset_t pending;
  if (sigpending(&pending) != 0)
    fail("sigpending");
  say("block-all SIGUSR1 pending=%d SIGCHLD pending=%d\n",
      sigismember(&pending, SIGUSR1), sigismember(&pending, SIGCHLD));
  sigset_t usr1;
  sigemptyset(&usr1);
  sigaddset(&usr1, SIGUSR1);
  struct timespec zero = {0, 0};
  int taken = sigtimedwait(&usr1, NULL, &zero);
  if (taken < 0)
    fail("sigtimedwait");
  say("block-all taken=%d\n", taken);
  return 0;
}

/* (d) Linux copies the set in with the kernel's user-copy routine, which can
 * read a page mapped write-only on x86-64 (such a page is readable in
 * hardware), so this call succeeds natively. */
static int write_only_set(void) {
  uint64_t* write_only =
      mmap(NULL, 4096, PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (write_only == MAP_FAILED)
    fail("mmap");
  *write_only = UINT64_C(1) << (SIGUSR2 - 1);
  if (raw_sigprocmask(SIG_BLOCK, write_only, NULL) != 0)
    fail("block SIGUSR2 from a write-only page");
  say("write-only-set SIGUSR2 blocked=%d\n",
      (current_mask() & (UINT64_C(1) << (SIGUSR2 - 1))) != 0);
  return 0;
}

int main(int argc, char** argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s MODE\n", argv[0]);
    return 64;
  }
  const char* mode = argv[1];
  if (strcmp(mode, "sigchld-during-wait") == 0)
    return sigchld_during_wait();
  if (strcmp(mode, "ignored-during-wait") == 0)
    return ignored_during_wait();
  if (strcmp(mode, "terminate-during-wait") == 0)
    return terminate_during_wait();
  if (strcmp(mode, "terminate-then-exit") == 0)
    return terminate_then_exit();
  if (strcmp(mode, "guest-blocked") == 0)
    return guest_blocked();
  if (strcmp(mode, "block-all") == 0)
    return block_all();
  if (strcmp(mode, "write-only-set") == 0)
    return write_only_set();
  fprintf(stderr, "unknown mode %s\n", mode);
  return 64;
}
