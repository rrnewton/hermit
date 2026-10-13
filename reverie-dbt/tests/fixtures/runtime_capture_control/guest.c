#define _GNU_SOURCE
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

/* Shared with the explicitly selected native_control_probe test Tool. */
#define PROBE_TAG UINT64_C(0x43505250)
#define FIRST_REPLY 31337L
#define SECOND_REPLY 41414L

static void require(int condition, const char *message) {
  if (!condition) {
    fprintf(stderr, "runtime capture guest failure: %s\n", message);
    exit(90);
  }
}

static void current_mask(sigset_t *mask) {
  memset(mask, 0, sizeof(*mask));
  require(sigprocmask(SIG_SETMASK, NULL, mask) == 0, "read complete guest mask");
}

static void current_action(struct sigaction *action) {
  memset(action, 0, sizeof(*action));
  require(sigaction(SIGCHLD, NULL, action) == 0, "read SIGCHLD action");
}

/* Linux x86-64 defines signals 1..64. rt_sigaction does not return the
 * remaining libc sigset_t storage, so compare every defined membership. */
static int same_signal_membership(const sigset_t *before, const sigset_t *after) {
  int equal = 1;
  for (int signal = 1; signal <= 64; ++signal) {
    int before_member = sigismember(before, signal);
    int after_member = sigismember(after, signal);
    require(before_member >= 0 && after_member >= 0,
            "query every defined signal membership");
    if (before_member != after_member)
      equal = 0;
  }
  return equal;
}

static void unchanged(const sigset_t *mask, const struct sigaction *action) {
  sigset_t after_mask;
  struct sigaction after_action;
  current_mask(&after_mask);
  current_action(&after_action);
  require(memcmp(mask, &after_mask, sizeof(*mask)) == 0,
          "complete guest mask survives control transaction");
  require(action->sa_handler == after_action.sa_handler &&
              action->sa_flags == after_action.sa_flags &&
              action->sa_restorer == after_action.sa_restorer &&
              same_signal_membership(&action->sa_mask, &after_action.sa_mask),
          "complete SIGCHLD action survives control transaction");
}

int main(void) {
  sigset_t requested, before_mask;
  struct sigaction requested_action, before_action;
  require(sigemptyset(&requested) == 0 &&
              sigaddset(&requested, SIGUSR1) == 0 &&
              sigaddset(&requested, SIGUSR2) == 0,
          "prepare unrelated mask bits");
  require(sigprocmask(SIG_SETMASK, &requested, NULL) == 0,
          "install test guest mask");
  memset(&requested_action, 0, sizeof(requested_action));
  requested_action.sa_handler = SIG_DFL;
  require(sigemptyset(&requested_action.sa_mask) == 0 &&
              sigaction(SIGCHLD, &requested_action, NULL) == 0,
          "install default SIGCHLD without auto-reap");
  current_mask(&before_mask);
  current_action(&before_action);
  require(sigismember(&before_mask, SIGCHLD) == 0 &&
              before_action.sa_handler == SIG_DFL &&
              (before_action.sa_flags & SA_NOCLDWAIT) == 0,
          "literal eligible guest disposition and mask");

  long first = syscall(SYS_getuid, PROBE_TAG, UINT64_C(1));
  require(first == FIRST_REPLY, "first tagged RPC result is unchanged");
  unchanged(&before_mask, &before_action);
  long second = syscall(SYS_getuid, PROBE_TAG, UINT64_C(2));
  require(second == SECOND_REPLY, "next RPC receives its own ordinary result");
  unchanged(&before_mask, &before_action);
  printf("runtime capture guest completed first=%ld second=%ld\n", first, second);
  return 0;
}
