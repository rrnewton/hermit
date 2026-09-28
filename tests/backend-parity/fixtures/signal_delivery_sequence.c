/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Signal handling sequence: sigaction, blocking, pending inspection, delivery.
 *
 * Installs a counting SIGUSR1 handler, blocks SIGUSR1 via sigprocmask, raises
 * it N times while blocked (so the kernel coalesces them into ONE pending
 * instance), asserts it shows in sigpending, then unblocks and observes exactly
 * one delivery. Finally it raises N times UNBLOCKED and observes N deliveries.
 *
 * This is the standard-but-easily-broken POSIX contract that non-realtime
 * signals do not queue: a backend that replays or re-injects blocked signals
 * naively reports N deliveries in the coalesced phase instead of 1.
 *
 * Deterministic by construction: the observables are two delivery counts and a
 * pending-bit, all fixed by the standard. No pid, timestamp, or ordering.
 */

/* The e2e harness compiles with -std=c11, which hides POSIX declarations. */
#define _GNU_SOURCE

#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/select.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#define RAISES 5

static volatile sig_atomic_t delivered = 0;

static void handler(int signo) {
  (void)signo;
  delivered++;
}

static void fail(const char *message) {
  fprintf(stderr, "%s: %s\n", message, strerror(errno));
  exit(1);
}

/*
 * The e2e harness has no golden-output field: its verify oracle is exit status
 * plus cross-attempt determinism. A deterministically wrong stdout therefore
 * passes unnoticed unless the guest checks itself, so every invariant below is
 * asserted rather than merely printed.
 */
static int violations;

static void expect(const char *name, long long observed, long long wanted) {
  if (observed != wanted) {
    fprintf(stderr, "invariant %s: observed %lld, wanted %lld\n", name, observed,
            wanted);
    violations++;
  }
}

/* Layout consumed by the x86-64 rt_sigaction syscall, not glibc's wrapper. */
struct kernel_sigaction {
  uint64_t handler;
  uint64_t flags;
  uint64_t restorer;
  uint64_t mask;
};

_Static_assert(sizeof(struct kernel_sigaction) == 32,
               "unexpected kernel sigaction layout");

struct pselect6_sigmask_arg {
  uint64_t *sigmask;
  size_t sigsetsize;
};

static void expect_errno(const char *name, long result, int wanted) {
  int observed = errno;
  if (result != -1 || observed != wanted) {
    fprintf(stderr, "invariant %s: result %ld errno %d, wanted -1/%d\n", name,
            result, observed, wanted);
    violations++;
  }
}

/*
 * Keep raw kernel signal objects immediately before an inaccessible page. This
 * distinguishes the kernel's eight-byte mask from glibc's 128-byte sigset_t:
 * reading either object as the latter crosses into the guard page and fails.
 */
static void check_raw_signal_abi_boundaries(void) {
  long page_size = sysconf(_SC_PAGESIZE);
  if (page_size <= 0)
    fail("sysconf page size");

  unsigned char *mapping = mmap(NULL, (size_t)page_size * 2,
                                PROT_READ | PROT_WRITE,
                                MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (mapping == MAP_FAILED)
    fail("mmap signal ABI guard pages");
  if (mprotect(mapping + page_size, (size_t)page_size, PROT_NONE) != 0)
    fail("mprotect signal ABI guard page");

  void *inaccessible = mapping + page_size;

  errno = 0;
  expect_errno("rt_sigprocmask size before pointer",
               syscall(SYS_rt_sigprocmask, SIG_BLOCK, inaccessible, NULL,
                       sizeof(uint64_t) + 1),
               EINVAL);

  errno = 0;
  expect_errno("rt_sigaction size before pointer",
               syscall(SYS_rt_sigaction, SIGUSR2, inaccessible, NULL,
                       sizeof(uint64_t) + 1),
               EINVAL);

  errno = 0;
  expect_errno("rt_sigsuspend size before pointer",
               syscall(SYS_rt_sigsuspend, inaccessible,
                       sizeof(uint64_t) + 1),
               EINVAL);

  errno = 0;
  expect_errno("reserved rt_sigaction size before pointer",
               syscall(SYS_rt_sigaction, SIGSTKFLT, inaccessible, NULL,
                       sizeof(uint64_t) + 1),
               EINVAL);

  errno = 0;
  expect_errno("reserved rt_sigaction action pointer",
               syscall(SYS_rt_sigaction, SIGSTKFLT, inaccessible, NULL,
                       sizeof(uint64_t)),
               EFAULT);

  errno = 0;
  expect_errno("reserved rt_sigaction old-action pointer",
               syscall(SYS_rt_sigaction, SIGSTKFLT, NULL, inaccessible,
                       sizeof(uint64_t)),
               EFAULT);

  uint64_t valid_wait_mask = 0;
  errno = 0;
  expect_errno("rt_sigtimedwait size before timeout pointer",
               syscall(SYS_rt_sigtimedwait, &valid_wait_mask, NULL,
                       inaccessible, sizeof(uint64_t) + 1),
               EINVAL);

  errno = 0;
  expect_errno("rt_sigprocmask inaccessible valid-size pointer",
               syscall(SYS_rt_sigprocmask, SIG_BLOCK, inaccessible, NULL,
                       sizeof(uint64_t)),
               EFAULT);

  unsigned char *partial_mask = mapping + page_size - sizeof(uint32_t);
  memset(partial_mask, 0, sizeof(uint32_t));
  errno = 0;
  expect_errno("rt_sigprocmask signal mask crosses inaccessible page",
               syscall(SYS_rt_sigprocmask, SIG_BLOCK, partial_mask, NULL,
                       sizeof(uint64_t)),
               EFAULT);

  void *wrapping_mask = (void *)(UINTPTR_MAX - sizeof(uint32_t) + 1);
  errno = 0;
  expect_errno("rt_sigprocmask signal mask wraps address space",
               syscall(SYS_rt_sigprocmask, SIG_BLOCK, wrapping_mask, NULL,
                       sizeof(uint64_t)),
               EFAULT);

  errno = 0;
  expect_errno("rt_sigsuspend inaccessible valid-size pointer",
               syscall(SYS_rt_sigsuspend, inaccessible, sizeof(uint64_t)),
               EFAULT);

  int pselect_pipe[2];
  if (pipe(pselect_pipe) != 0 || write(pselect_pipe[1], "r", 1) != 1)
    fail("pselect6 ready pipe");
  fd_set ready_readfds;
  FD_ZERO(&ready_readfds);
  FD_SET(pselect_pipe[0], &ready_readfds);
  struct timespec ready_timeout = {1, 0};
  struct pselect6_sigmask_arg inaccessible_pselect_mask = {
      .sigmask = inaccessible,
      .sigsetsize = sizeof(uint64_t),
  };
  errno = 0;
  expect_errno("pselect6 inaccessible signal-mask pointer",
               syscall(SYS_pselect6, pselect_pipe[0] + 1, &ready_readfds, NULL,
                       NULL, &ready_timeout, &inaccessible_pselect_mask),
               EFAULT);
  if (close(pselect_pipe[0]) != 0 || close(pselect_pipe[1]) != 0)
    fail("close pselect6 ready pipe");

  struct timespec short_timeout = {0, 1};
  errno = 0;
  expect_errno("ppoll inaccessible signal-mask pointer",
               syscall(SYS_ppoll, NULL, 0, &short_timeout, inaccessible,
                       sizeof(uint64_t)),
               EFAULT);

  struct kernel_sigaction *read_only_action =
      (struct kernel_sigaction *)mapping;
  memset(read_only_action, 0, sizeof(*read_only_action));
  read_only_action->handler = 1; /* SIG_IGN in the raw kernel ABI. */
  read_only_action->mask = UINT64_C(1) << (SIGSTKFLT - 1);
  struct kernel_sigaction expected_read_only_action = *read_only_action;
  if (mprotect(mapping, (size_t)page_size, PROT_READ) != 0)
    fail("mprotect read-only signal action");
  if (syscall(SYS_rt_sigaction, SIGUSR2, read_only_action, NULL,
              sizeof(uint64_t)) != 0)
    fail("read-only rt_sigaction input");
  if (memcmp(read_only_action, &expected_read_only_action,
             sizeof(*read_only_action)) != 0)
    fail("rt_sigaction changed its input object");
  if (mprotect(mapping, (size_t)page_size, PROT_READ | PROT_WRITE) != 0)
    fail("restore writable signal action page");

  uint64_t *boundary_mask =
      (uint64_t *)(mapping + page_size - sizeof(uint64_t));
  *boundary_mask = 0;
  if (syscall(SYS_rt_sigprocmask, SIG_BLOCK, boundary_mask, NULL,
              sizeof(uint64_t)) != 0)
    fail("boundary rt_sigprocmask");

  struct timespec zero_timeout = {0, 0};
  errno = 0;
  expect_errno("boundary rt_sigtimedwait",
               syscall(SYS_rt_sigtimedwait, boundary_mask, NULL, &zero_timeout,
                       sizeof(uint64_t)),
               EAGAIN);

  struct kernel_sigaction *boundary_action =
      (struct kernel_sigaction *)(mapping + page_size -
                                  sizeof(struct kernel_sigaction));
  memset(boundary_action, 0, sizeof(*boundary_action));
  boundary_action->handler = 1; /* SIG_IGN in the raw kernel ABI. */
  if (syscall(SYS_rt_sigaction, SIGUSR2, boundary_action, NULL,
              sizeof(uint64_t)) != 0)
    fail("boundary rt_sigaction");

  struct kernel_sigaction default_action;
  memset(&default_action, 0, sizeof(default_action));
  if (syscall(SYS_rt_sigaction, SIGUSR2, &default_action, NULL,
              sizeof(uint64_t)) != 0)
    fail("restore rt_sigaction");

  if (munmap(mapping, (size_t)page_size * 2) != 0)
    fail("munmap signal ABI guard pages");
}

int main(void) {
  check_raw_signal_abi_boundaries();

  struct sigaction sa;
  memset(&sa, 0, sizeof(sa));
  sa.sa_handler = handler;
  sigemptyset(&sa.sa_mask);
  if (sigaction(SIGUSR1, &sa, NULL) != 0)
    fail("sigaction");

  sigset_t block, previous;
  sigemptyset(&block);
  sigaddset(&block, SIGUSR1);
  if (sigprocmask(SIG_BLOCK, &block, &previous) != 0)
    fail("sigprocmask block");

  for (int i = 0; i < RAISES; ++i)
    if (raise(SIGUSR1) != 0)
      fail("raise while blocked");

  sigset_t pending;
  sigemptyset(&pending);
  if (sigpending(&pending) != 0)
    fail("sigpending");
  int was_pending = sigismember(&pending, SIGUSR1) == 1 ? 1 : 0;

  if (sigprocmask(SIG_SETMASK, &previous, NULL) != 0)
    fail("sigprocmask restore");
  int coalesced = (int)delivered;

  delivered = 0;
  for (int i = 0; i < RAISES; ++i)
    if (raise(SIGUSR1) != 0)
      fail("raise while unblocked");
  int direct = (int)delivered;

  expect("raised", (long long)RAISES, 5);
  expect("pending", (long long)was_pending, 1);
  expect("coalesced", (long long)coalesced, 1);
  expect("direct", (long long)direct, 5);
  printf("signals raised=%d pending=%d coalesced=%d direct=%d\n", RAISES,
         was_pending, coalesced, direct);
  return violations == 0 ? 0 : 1;
}
