/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-786)

// Regression for PR-786. Detcore virtualizes the inoperative I/O-priority
// (ioprio_get/ioprio_set) and extended CPU-scheduling (sched_getattr/
// sched_setattr) syscalls to fixed, host-independent results, and pairs
// getitimer with the already-determinized setitimer/alarm handling. Without
// these, each of ioprio_get, sched_getattr, and getitimer fail-closes under
// --strict (see detcore/src/syscall_classification.rs and the handlers in
// detcore/src/lib.rs). The strict and strict+verify runs live in the sibling
// .lit files; the native run below only checks the host-invariant lines (the
// scheduling policy and the interval-timer round-trip), since the host's I/O
// priority is not fixed.
// RUN: %me | FileCheck %s --check-prefix=NATIVE

#define _GNU_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <unistd.h>

#define IOPRIO_CLASS_SHIFT 13
#define IOPRIO_WHO_PROCESS 1

// Kernel struct sched_attr (SCHED_ATTR_SIZE_VER0). Not exposed by glibc.
struct sched_attr_local {
  uint32_t size;
  uint32_t sched_policy;
  uint64_t sched_flags;
  int32_t sched_nice;
  uint32_t sched_priority;
  uint64_t sched_runtime;
  uint64_t sched_deadline;
  uint64_t sched_period;
};

int main(void) {
  // I/O scheduling priority: Detcore reports a fixed best-effort priority.
  long ip = syscall(SYS_ioprio_get, IOPRIO_WHO_PROCESS, 0);
  if (ip >= 0) {
    printf("ioprio class %ld data %ld\n",
           ip >> IOPRIO_CLASS_SHIFT,
           ip & ((1L << IOPRIO_CLASS_SHIFT) - 1));
  } else {
    printf("ioprio error\n");
  }

  // Extended CPU-scheduling attributes: Detcore reports SCHED_OTHER (policy 0).
  struct sched_attr_local sa;
  memset(&sa, 0xff, sizeof(sa));
  long r = syscall(SYS_sched_getattr, 0, &sa, (unsigned)sizeof(sa), 0u);
  printf("sched_getattr rc %ld policy %u\n", r, r == 0 ? sa.sched_policy : 0u);

  // Interval timer: a disarmed ITIMER_REAL reads back as zero.
  struct itimerval it;
  memset(&it, 0, sizeof(it));
  getitimer(ITIMER_REAL, &it);
  printf("itimer disarmed %ld.%06ld\n",
         (long)it.it_value.tv_sec, (long)it.it_value.tv_usec);

  // After arming a one-shot 5s timer, getitimer reads back the remaining time
  // from the deterministic virtual alarm state (a little under 5s, so 4s).
  struct itimerval arm;
  memset(&arm, 0, sizeof(arm));
  arm.it_value.tv_sec = 5;
  setitimer(ITIMER_REAL, &arm, NULL);
  getitimer(ITIMER_REAL, &it);
  printf("itimer armed sec %ld\n", (long)it.it_value.tv_sec);
  return 0;
}

// NATIVE: sched_getattr rc 0 policy 0
// NATIVE: itimer disarmed 0.000000
// NATIVE: itimer armed sec 4
