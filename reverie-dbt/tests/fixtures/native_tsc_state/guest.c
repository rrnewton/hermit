/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
#define _GNU_SOURCE
#include "state.h"
#include <cpuid.h>
#include <errno.h>
#include <signal.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define PROBE_TAG UINT64_C(0x4f535050)
_Static_assert(offsetof(struct state_image, flags) == STATE_FLAGS, "flags offset");
_Static_assert(offsetof(struct state_image, fx) == STATE_FX, "FX offset");
_Static_assert(offsetof(struct state_image, simd) == STATE_SIMD, "SIMD offset");
_Static_assert(offsetof(struct state_image, mask) == STATE_MASK, "mask offset");
_Static_assert(sizeof(struct state_image) == STATE_SIZE, "image size");

struct state_image before_state, after_state;
unsigned char caller_fx[512] __attribute__((aligned(64)));
unsigned char seed_fx[512] __attribute__((aligned(64)));
uint64_t vector_seed[32][8] __attribute__((aligned(64)));
uint64_t mask_seed[8] __attribute__((aligned(64)));
uint32_t continuations;
static unsigned char initial_fx[512] __attribute__((aligned(64)));

static void require(int condition, const char *message) {
  if (!condition) {
    fprintf(stderr, "native TSC state failure: %s\n", message);
    exit(90);
  }
}

static ssize_t read_retry(int fd, void *buffer, size_t size) {
  ssize_t result;
  do {
    result = read(fd, buffer, size);
  } while (result < 0 && errno == EINTR);
  return result;
}

/* CPU+OS availability selects instructions only. The host independently
 * requires the actual DR validity/count marker for this chosen vector level. */
static unsigned vector_level(void) {
  unsigned a, b, c, d;
  require(__get_cpuid(1, &a, &b, &c, &d), "CPUID leaf1");
  require((d & bit_FXSAVE) && (d & bit_SSE2), "x86-64 FXSAVE/SSE2");
  if (!(c & bit_AVX) || !(c & bit_OSXSAVE))
    return 0;
  unsigned lo, hi;
  __asm__ volatile("xgetbv" : "=a"(lo), "=d"(hi) : "c"(0));
  uint64_t xcr0 = ((uint64_t)hi << 32) | lo;
  if ((xcr0 & 6) != 6)
    return 0;
  if (__get_cpuid_max(0, NULL) < 7)
    return 1;
  __cpuid_count(7, 0, a, b, c, d);
  if (!(b & bit_AVX512F) || (xcr0 & 0xe6) != 0xe6)
    return 1;
  return (b & bit_AVX512BW) ? 3 : 2;
}

static void prepare_seed(int mmx) {
  static const double numbers[8] = {1.25, -2.5, 3.75, -4.125,
                                    5.5, -6.75, 7.875, -8.25};
  static const uint64_t mm[8] = {
      0x0102030405060708, 0x1112131415161718, 0x2122232425262728,
      0x3132333435363738, 0x4142434445464748, 0x5152535455565758,
      0x6162636465666768, 0x7172737475767778};
  const uint16_t control = 0x077f; /* all exceptions masked, round downward */
  const uint32_t mxcsr = 0x3f80;  /* legal flags, round downward */
  __asm__ volatile(
      "fxsave64 %0\n\t"
      "fninit\n\t"
      "fldcw %1\n\t"
      "ldmxcsr %2\n\t"
      "fldl 0(%3)\n\tfldl 8(%3)\n\tfldl 16(%3)\n\tfldl 24(%3)\n\t"
      "fldl 32(%3)\n\tfldl 40(%3)\n\tfldl 48(%3)\n\tfldl 56(%3)"
      : "=m"(initial_fx)
      : "m"(control), "m"(mxcsr), "r"(numbers)
      : "memory");
  if (mmx) {
    __asm__ volatile(
        "movq 0(%0), %%mm0\n\tmovq 8(%0), %%mm1\n\t"
        "movq 16(%0), %%mm2\n\tmovq 24(%0), %%mm3\n\t"
        "movq 32(%0), %%mm4\n\tmovq 40(%0), %%mm5\n\t"
        "movq 48(%0), %%mm6\n\tmovq 56(%0), %%mm7"
        :
        : "r"(mm)
        : "memory", "mm0", "mm1", "mm2", "mm3", "mm4", "mm5", "mm6", "mm7");
  }
  __asm__ volatile("fxsave64 %0\n\tfxrstor64 %1"
                   : "=m"(seed_fx)
                   : "m"(initial_fx)
                   : "memory");
  for (unsigned i = 0; i < 32; ++i)
    for (unsigned j = 0; j < 8; ++j)
      vector_seed[i][j] = UINT64_C(0x1020304050607000) + i * 8 + j;
  for (unsigned i = 0; i < 8; ++i)
    mask_seed[i] = UINT64_C(0x1f2e3d4c5b6a7900) + i;
}

static void same(const void *a, const void *b, size_t bytes, const char *name) {
  require(memcmp(a, b, bytes) == 0, name);
}

static void verify_state(int tscp, unsigned level) {
  require(continuations == 1, "decoded continuation executes exactly once");
  for (unsigned i = 0; i < 16; ++i) {
    uint64_t expected = before_state.gpr[i];
    if (i == 0)
      expected = (uint32_t)((uint64_t)TSC_WORD);
    if (i == 3)
      expected = (uint32_t)(((uint64_t)TSC_WORD) >> 32);
    if (i == 2 && tscp)
      expected = ((uint32_t)TSC_AUX);
    require(after_state.gpr[i] == expected,
            "every GPR/output preserves its architectural value");
  }
  require(before_state.flags == after_state.flags,
          "RFLAGS including DF and condition flags are preserved");
  /* Compare defined FXSAVE fields, excluding byte5 and reserved ST padding.
   * FIP/FDP, control/status/tag/opcode and MXCSR are all checked. */
  same(before_state.fx, after_state.fx, 5, "x87 control/status/tag");
  same(before_state.fx + 6, after_state.fx + 6, 26,
       "x87 opcode/FIP/FDP/MXCSR/mask");
  for (unsigned i = 0; i < 8; ++i)
    same(before_state.fx + 32 + i * 16, after_state.fx + 32 + i * 16,
         10, "all eight x87/MMX payloads");
  unsigned width = level == 0 ? 16 : (level == 1 ? 32 : 64);
  unsigned count = level >= 2 ? 32 : 16;
  for (unsigned i = 0; i < count; ++i) {
    same(before_state.simd[i], vector_seed[i], width, "SIMD seed is nontrivial");
    same(before_state.simd[i], after_state.simd[i], width,
         "complete feature-qualified SIMD register");
  }
  if (level >= 2)
    for (unsigned i = 0; i < 8; ++i) {
      uint64_t expected = level == 3 ? mask_seed[i] : (uint16_t)mask_seed[i];
      require(before_state.mask[i] == expected &&
                  after_state.mask[i] == expected,
              "all available opmask bits");
    }
}

static void announce_child(unsigned role) {
  long response = syscall(SYS_getuid, PROBE_TAG, (uint64_t)(role + 2),
                          (long)getpid(), (long)getppid());
  require(response == 4411392L + role, "real held child receives exact reply");
}

static void reap(pid_t pid, int expected) {
  int status;
  require(waitpid(pid, &status, 0) == pid &&
              WIFEXITED(status) && WEXITSTATUS(status) == expected,
          "exact controlled child reap");
}

int main(int argc, char **argv) {
  require(argc == 2 && (!strcmp(argv[1], "tsc") || !strcmp(argv[1], "tscp")),
          "exact opcode argument");
  int tscp = !strcmp(argv[1], "tscp");
  if (tscp) {
    unsigned a, b, c, d;
    require(__get_cpuid(0x80000001, &a, &b, &c, &d) && (d & (1u << 27)),
            "RDTSCP architectural availability");
  }
  unsigned level = vector_level();
  void (*functions[2][4])(void) = {
      {tsc_xmm, tsc_ymm, tsc_zmm16, tsc_zmm64},
      {tscp_xmm, tscp_ymm, tscp_zmm16, tscp_zmm64}};
  sigset_t mask;
  struct sigaction action, installed_action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = SIG_DFL;
  require(sigemptyset(&action.sa_mask) == 0 &&
              sigaction(SIGCHLD, &action, NULL) == 0,
          "default SIGCHLD without auto-reap");
  memset(&installed_action, 0, sizeof(installed_action));
  require(sigaction(SIGCHLD, NULL, &installed_action) == 0 &&
              installed_action.sa_handler == SIG_DFL &&
              !(installed_action.sa_flags & SA_NOCLDWAIT),
          "capture complete actual installed action before execution");
  require(sigemptyset(&mask) == 0 &&
              sigaddset(&mask, SIGUSR1) == 0 &&
              sigaddset(&mask, SIGUSR2) == 0 &&
              sigprocmask(SIG_SETMASK, &mask, NULL) == 0,
          "unblocked SIGCHLD with nontrivial unrelated mask");

  pid_t children[2];
  for (unsigned i = 0; i < 2; ++i) {
    children[i] = fork();
    require(children[i] >= 0, "two real publishers");
    if (!children[i]) {
      announce_child(i + 1); /* host withholds this real ordinary reply */
      _exit(i == 0 ? 31 : 47);
    }
  }
  require(syscall(SYS_getuid, PROBE_TAG, UINT64_C(1), (long)children[0],
                  (long)children[1], (long)getpid()) == 31337,
          "parent starts only after three authenticated announcements");

  for (unsigned phase = 0; phase < 2; ++phase) {
    prepare_seed(phase != 0);
    memset(&before_state, 0, sizeof(before_state));
    memset(&after_state, 0, sizeof(after_state));
    continuations = 0;
    functions[tscp][level]();
    verify_state(tscp, level);
    /* No child reads stdin. The host sends this only after all three actual
     * Signal RPCs for the corresponding publisher have completed. It cannot
     * substitute an input byte for a capture or retirement receipt. */
    char permit = 0;
    require(read_retry(STDIN_FILENO, &permit, 1) == 1 &&
                permit == (char)('A' + phase),
            "host observed complete actual signal checkpoints");
    reap(children[phase], phase == 0 ? 31 : 47);
  }
  require(syscall(SYS_getuid, PROBE_TAG, UINT64_C(2), (long)children[0],
                  (long)children[1], (long)getpid()) == 41414,
          "original Tool oracle requires both completed signal callbacks");
  sigset_t actual;
  struct sigaction actual_action;
  require(sigprocmask(SIG_SETMASK, NULL, &actual) == 0 &&
              sigaction(SIGCHLD, NULL, &actual_action) == 0,
          "read final guest signal state");
  for (int s = 1; s <= 64; ++s) {
    int expected_mask = sigismember(&mask, s);
    int observed_mask = sigismember(&actual, s);
    int expected_action_mask = sigismember(&installed_action.sa_mask, s);
    int observed_action_mask = sigismember(&actual_action.sa_mask, s);
    require(expected_mask >= 0 && observed_mask == expected_mask &&
                expected_action_mask >= 0 &&
                observed_action_mask == expected_action_mask,
            "full defined guest and action masks preserved");
  }
  require(actual_action.sa_handler == installed_action.sa_handler &&
              actual_action.sa_flags == installed_action.sa_flags &&
              actual_action.sa_restorer == installed_action.sa_restorer,
          "complete supported default action preserved");
  printf("native TSC state opcode=%s vector_level=%u x87=ok mmx=ok once=2\n",
         argv[1], level);
  return 0;
}

