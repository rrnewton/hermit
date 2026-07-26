/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-787)

// Regression for PR-787. Detcore virtualizes the kernel NTP clock discipline
// (adjtimex/clock_adjtime) and the kernel log ring buffer (syslog) to fixed,
// host-independent results. Without these, each of adjtimex, clock_adjtime, and
// syslog fail-closes under --strict (see detcore/src/syscall_classification.rs
// and the handlers in detcore/src/syscalls/time.rs and misc.rs). The strict and
// strict+verify runs live in the sibling .lit files; the native run below only
// checks the host-invariant lines (the query calls succeed with rc 0), since the
// host's clock-discipline status and kernel-log buffer size are not fixed.
// RUN: %me | FileCheck %s --check-prefix=NATIVE

#define _GNU_SOURCE
#include <stdio.h>
#include <sys/syscall.h>
#include <sys/timex.h>
#include <time.h>
#include <unistd.h>

int main(void) {
  // NTP clock discipline: Detcore reports a fixed, unsynchronized snapshot.
  struct timex tx;
  tx.modes = 0;
  long a = syscall(SYS_adjtimex, &tx);
  printf("adjtimex rc=%ld status=%d\n", a, a < 0 ? -1 : tx.status);

  struct timex cx;
  cx.modes = 0;
  long c = syscall(SYS_clock_adjtime, CLOCK_REALTIME, &cx);
  printf("clock_adjtime rc=%ld status=%d tick=%ld\n", c, c < 0 ? -1 : cx.status,
         c < 0 ? -1L : cx.tick);

  // Kernel log ring buffer: Detcore presents a deterministic empty log, so
  // SYSLOG_ACTION_SIZE_BUFFER (10) reports zero.
  long s = syscall(SYS_syslog, 10, (char *)0, 0);
  printf("syslog size_buffer=%ld\n", s);
  return 0;
}

// NATIVE: adjtimex rc=0
// NATIVE: clock_adjtime rc=0
