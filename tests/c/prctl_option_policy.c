/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Both halves of Hermit's prctl option policy, in one guest.
 *
 * Hermit does not refuse prctl. It refuses individual prctl OPTIONS: the
 * handler at detcore/src/syscalls/misc.rs passes a supported option through to
 * Linux and answers everything else with ENOSYS. Two different properties hold
 * at once, and a cell that checks only one of them cannot defend the policy.
 *
 * POSITIVE HALF -- PR_SET_TIMERSLACK / PR_GET_TIMERSLACK round-trip.
 * Timer slack is deliberately PASSED THROUGH rather than virtualized, so this
 * half is byte-identical under Hermit and natively and carries NO signal
 * against the host. It is here for a different reason: it reds the moment
 * TIMERSLACK is dropped from is_supported_prctl_option, which would silently
 * turn a working guest control into ENOSYS. There is open work on this very
 * mechanism (rrnewton/hermit#2150) with nothing else defending it.
 *
 * NEGATIVE HALF -- PR_SET_NO_NEW_PRIVS must be refused with ENOSYS.
 * This is the half that discriminates. The option succeeds natively and is
 * refused under Hermit, so running this guest without Hermit FAILS it. It reds
 * if the fail-closed refusal is ever weakened into a silent passthrough.
 *
 * ENOSYS here is the prctl handler's own unconditional answer, not the general
 * unsupported-SYSCALL default that rrnewton/hermit#2357 changes; that work does
 * not touch detcore. Pinning the errno is therefore safe against it.
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <unistd.h>

/* Arbitrary non-default nanosecond slack; any value Linux accepts will do. */
#define REQUESTED_TIMERSLACK 123456L

int main(void) {
  errno = 0;
  if (prctl(PR_SET_TIMERSLACK, REQUESTED_TIMERSLACK, 0, 0, 0) != 0) {
    fprintf(stderr, "PR_SET_TIMERSLACK failed with errno %d (%s), expected success\n",
            errno, strerror(errno));
    return 1;
  }

  errno = 0;
  long slack = prctl(PR_GET_TIMERSLACK, 0, 0, 0, 0);
  if (slack != REQUESTED_TIMERSLACK) {
    fprintf(stderr,
            "PR_GET_TIMERSLACK returned %ld (errno %d), expected %ld\n",
            slack, errno, REQUESTED_TIMERSLACK);
    return 1;
  }

  errno = 0;
  int refused = prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
  if (refused != -1 || errno != ENOSYS) {
    fprintf(stderr,
            "PR_SET_NO_NEW_PRIVS returned %d with errno %d (%s), expected -1 ENOSYS\n",
            refused, errno, strerror(errno));
    return 1;
  }

  puts("prctl supported option round-trips and unsupported option is refused");
  return 0;
}
