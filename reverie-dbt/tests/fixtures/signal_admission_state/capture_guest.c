#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

/* Only the paired native client uses these permanently mapped fixture symbols. */
int capture_parent, capture_children[2], capture_release_fds[2];
volatile int capture_stage, capture_progress;
uint64_t capture_guest_mask;
_Atomic unsigned capture_seen;
static volatile sig_atomic_t handled, bad_handler;
static const uint64_t child_bit = UINT64_C(1) << (SIGCHLD - 1);

static void require(int condition, const char *message) {
  if (!condition) {
    fprintf(stderr, "capture guest failure: %s\n", message);
    exit(41);
  }
}

static uint64_t mask_now(void) {
  uint64_t mask = 0;
  require(syscall(SYS_rt_sigprocmask, SIG_SETMASK, NULL, &mask, 8) == 0,
          "query complete virtual mask");
  return mask;
}

static void child_handler(int sig, siginfo_t *info, void *context) {
  (void)context;
  if (sig != SIGCHLD || info->si_pid != capture_children[0] ||
      info->si_code != CLD_EXITED || info->si_status != 31 || capture_progress != 0)
    bad_handler = 1;
  ++handled;
}

/* The client checks at these exact post-syscall PCs, before the first resumed
 * application instruction. No helper waits for a callback being tested. */
__attribute__((noinline, noclone)) static long capture_point(void) {
  long result;
  __asm__ volatile("syscall\n.globl capture_return_marker\ncapture_return_marker:"
                   : "=a"(result) : "0"((long)SYS_gettid)
                   : "rcx", "r11", "cc", "memory");
  return result;
}

__attribute__((noinline, noclone)) static long unblock_point(const uint64_t *mask) {
  register long size __asm__("r10") = 8;
  long result;
  __asm__ volatile("syscall\n.globl capture_unblock_marker\ncapture_unblock_marker:"
                   : "=a"(result)
                   : "0"((long)SYS_rt_sigprocmask), "D"((long)SIG_SETMASK),
                     "S"(mask), "d"(0L), "r"(size)
                   : "rcx", "r11", "cc", "memory");
  return result;
}

static void make_children(unsigned count, int no_notification) {
  int release[2][2], ready[2];
  require(pipe(ready) == 0, "child readiness pipe");
  for (unsigned i = 0; i < count; ++i)
    require(pipe(release[i]) == 0, "independent child release pipe");
  for (unsigned i = 0; i < count; ++i) {
    /* exit_signal=0 is a real Linux clone child, not an ignored SIGCHLD.
     * With no CLONE_VM and a NULL stack this has fork-like address spaces. */
    pid_t child = no_notification ? (pid_t)syscall(SYS_clone, 0, NULL, NULL, NULL, 0)
                                  : fork();
    require(child >= 0, "create real child");
    if (child == 0) {
      close(ready[0]);
      for (unsigned j = 0; j < count; ++j) {
        close(release[j][1]);
        if (j != i) close(release[j][0]);
      }
      char byte = (char)i;
      if (write(ready[1], &byte, 1) != 1) _exit(51);
      close(ready[1]);
      if (read(release[i][0], &byte, 1) != 1 || byte != 'x') _exit(52);
      close(release[i][0]);
      _exit(31 + (int)i);
    }
    capture_children[i] = child;
    capture_release_fds[i] = release[i][1];
  }
  close(ready[1]);
  for (unsigned i = 0; i < count; ++i) close(release[i][0]);
  unsigned ready_bits = 0;
  for (unsigned i = 0; i < count; ++i) {
    unsigned char byte;
    require(read(ready[0], &byte, 1) == 1 && byte < count &&
                !(ready_bits & (1u << byte)), "each child reached application code");
    ready_bits |= 1u << byte;
  }
  require(ready_bits == (1u << count) - 1, "all independent children ready");
  close(ready[0]);
}

int main(int argc, char **argv) {
  require(argc == 2, "one exact case");
  const char *mode = argv[1];
  int blocked = !strcmp(mode, "guest-blocked") || !strcmp(mode, "physical-blocked");
  int refused = !strcmp(mode, "ignore-refused") || !strcmp(mode, "autoreap-refused");
  int no_notification = !strcmp(mode, "no-notification");
  int caught = !strcmp(mode, "caught");
  unsigned children = refused ? 0 :
      (!strcmp(mode, "stale-domain") || !strcmp(mode, "coalescing")) ? 2 : 1;
  capture_parent = getpid();
  struct sigaction action = {0}, before, after;
  sigemptyset(&action.sa_mask);
  sigaddset(&action.sa_mask, SIGUSR2);
  if (caught) {
    action.sa_sigaction = child_handler;
    action.sa_flags = SA_SIGINFO;
  } else {
    action.sa_handler = !strcmp(mode, "ignore-refused") ? SIG_IGN : SIG_DFL;
    if (!strcmp(mode, "autoreap-refused")) action.sa_flags = SA_NOCLDWAIT;
  }
  require(sigaction(SIGCHLD, &action, NULL) == 0 &&
              sigaction(SIGCHLD, NULL, &before) == 0, "install exact guest action");
  uint64_t initial_mask = mask_now();
  uint64_t wanted = (initial_mask | (UINT64_C(1) << (SIGUSR1 - 1)) |
                     (UINT64_C(1) << (SIGUSR2 - 1))) & ~child_bit;
  if (blocked) wanted |= child_bit;
  require(syscall(SYS_rt_sigprocmask, SIG_SETMASK, &wanted, NULL, 8) == 0,
          "install full guest mask with unrelated blocked signals");
  capture_guest_mask = mask_now();
  require(capture_guest_mask == wanted, "exact complete guest mask installed");
  if (children != 0) make_children(children, no_notification);
  capture_stage = 1;
  require(capture_point() == capture_parent, "original syscall result survives callback");
  require(mask_now() == capture_guest_mask, "capture preserves full virtual mask");
  capture_stage = 2;
  if (blocked) {
    require(atomic_load_explicit(&capture_seen, memory_order_acquire) == 0,
            "guest block prevents premature admission");
    uint64_t unblocked = capture_guest_mask & ~child_bit;
    require(unblock_point(&unblocked) == 0 && mask_now() == unblocked,
            "lawful guest unblock preserves every other mask bit");
  }
  unsigned expected = refused || no_notification ? 0 : !strcmp(mode, "coalescing") ? 1 : children;
  require(atomic_load_explicit(&capture_seen, memory_order_acquire) == expected,
          "authentic pending records admitted before marked guest progress");
  require(handled == caught && bad_handler == 0,
          "caught action runs only after native ownership ends; default stays default");
  capture_progress = 1;
  for (unsigned i = 0; i < children; ++i) {
    int status = -1;
    int options = no_notification ? __WCLONE : 0;
    require(waitpid(capture_children[i], &status, options) == capture_children[i] &&
                WIFEXITED(status) && WEXITSTATUS(status) == 31 + (int)i,
            "real child retains its exact reap status after WNOWAIT");
    errno = 0;
    require(waitpid(capture_children[i], &status, WNOHANG | options) == -1 &&
                errno == ECHILD, "child can be reaped exactly once");
  }
  require(sigaction(SIGCHLD, NULL, &after) == 0 &&
              before.sa_handler == after.sa_handler && before.sa_flags == after.sa_flags,
          "guest disposition and flags preserved");
  for (int sig = 1; sig < NSIG; ++sig)
    require(sigismember(&before.sa_mask, sig) == sigismember(&after.sa_mask, sig),
            "complete guest action mask preserved");
  require(syscall(SYS_rt_sigprocmask, SIG_SETMASK, &initial_mask, NULL, 8) == 0,
          "restore initial guest mask");
  printf("capture guest completed case=%s children=%u admissions=%u\n",
         mode, children, expected);
  return 0;
}
