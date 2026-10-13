/* Copyright (c) Meta Platforms, Inc. and affiliates. BSD-style license: LICENSE. */
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

/* Permanent test symbols, read by the paired real native client. */
int finalizer_parent, finalizer_child, finalizer_release_fd;
volatile int finalizer_stage;
static int child_status;
static siginfo_t child_info;

static void require(int yes, const char *what) {
  if (!yes) {
    fprintf(stderr, "wait finalizer guest failure: %s\n", what);
    exit(41);
  }
}

/* Raw six-register operands deliberately include nonzero unused registers.
 * The SDK must compare these exact bits, not a reconstructed typed subset. */
__attribute__((noinline, noclone)) static long wait_point(int use_waitid) {
  register long nr __asm__("rax") = use_waitid ? SYS_waitid : SYS_wait4;
  register long a0 __asm__("rdi") = use_waitid ? P_PID : finalizer_child;
  register long a1 __asm__("rsi") = use_waitid ? finalizer_child : (long)&child_status;
  register long a2 __asm__("rdx") = use_waitid ? (long)&child_info : 0;
  register long a3 __asm__("r10") = use_waitid ? WEXITED : 0;
  register long a4 __asm__("r8") = use_waitid ? 0 : 0x12345678;
  register long a5 __asm__("r9") = 0x23456789;
  __asm__ volatile("syscall\n.globl finalizer_wait_return\nfinalizer_wait_return:"
                   : "+a"(nr)
                   : "r"(a0), "r"(a1), "r"(a2), "r"(a3), "r"(a4), "r"(a5)
                   : "rcx", "r11", "cc", "memory");
  return nr;
}

int main(int argc, char **argv) {
  require(argc == 2, "one declared mode");
  int use_waitid = !strcmp(argv[1], "waitid-cancel");
  finalizer_parent = getpid();
  struct sigaction action = {0}, before, after;
  action.sa_handler = SIG_DFL;
  require(sigemptyset(&action.sa_mask) == 0 &&
              sigaddset(&action.sa_mask, SIGUSR2) == 0 &&
              sigaction(SIGCHLD, &action, NULL) == 0 &&
              sigaction(SIGCHLD, NULL, &before) == 0, "default real action");
  uint64_t mask = (UINT64_C(1) << (SIGUSR1 - 1)) | (UINT64_C(1) << (SIGUSR2 - 1));
  require(syscall(SYS_rt_sigprocmask, SIG_SETMASK, &mask, NULL, 8) == 0,
          "complete unrelated guest mask");
  int release[2], ready[2];
  require(pipe(release) == 0 && pipe(ready) == 0, "independent child pipes");
  finalizer_child = fork();
  require(finalizer_child >= 0, "real child");
  if (finalizer_child == 0) {
    close(release[1]); close(ready[0]);
    char value = 'r';
    if (write(ready[1], &value, 1) != 1) _exit(51);
    close(ready[1]);
    if (read(release[0], &value, 1) != 1 || value != 'x') _exit(52);
    close(release[0]);
    _exit(31);
  }
  close(release[0]); close(ready[1]);
  finalizer_release_fd = release[1];
  char ready_byte = 0;
  require(read(ready[0], &ready_byte, 1) == 1 && ready_byte == 'r',
          "child reached its independent application wait");
  close(ready[0]);
  finalizer_stage = 1;
  long result = wait_point(use_waitid);
  /* Later-client rejection modes must never reach this line. */
  if (!strncmp(argv[1], "late-", 5)) {
    fputs("wait finalizer forbidden application continuation\n", stderr);
    _exit(42);
  }
  require(use_waitid ? result == 0 && child_info.si_pid == finalizer_child &&
                          child_info.si_code == CLD_EXITED && child_info.si_status == 31
                    : result == finalizer_child && WIFEXITED(child_status) &&
                          WEXITSTATUS(child_status) == 31,
          "original guest wait executes exactly once with exact child status");
  finalizer_stage = 2;
  require(syscall(SYS_getuid, 0x5746494eL) == 77, "native completed-retirement checkpoint");
  errno = 0;
  require(waitpid(finalizer_child, &child_status, WNOHANG) == -1 && errno == ECHILD,
          "child was reaped exactly once");
  uint64_t actual_mask = 0;
  require(syscall(SYS_rt_sigprocmask, SIG_SETMASK, NULL, &actual_mask, 8) == 0 &&
              actual_mask == mask && sigaction(SIGCHLD, NULL, &after) == 0 &&
              before.sa_handler == after.sa_handler && before.sa_flags == after.sa_flags &&
              before.sa_restorer == after.sa_restorer,
          "complete mask/action/restorer survives");
  for (int sig = 1; sig < NSIG; ++sig)
    require(sigismember(&before.sa_mask, sig) == sigismember(&after.sa_mask, sig),
            "complete action mask survives");
  printf("wait finalizer guest completed case=%s child_status=31 reaps=1\n", argv[1]);
  return 0;
}
