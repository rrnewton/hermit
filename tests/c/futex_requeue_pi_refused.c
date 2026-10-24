/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Issues FUTEX_CMP_REQUEUE_PI, the requeue-PI command a PI condition variable
 * would use, with no waiter on either word, and prints the result. Natively it
 * succeeds (0). Detcore emulates the PI lock commands but not the requeue-PI
 * pair, so under Hermit the command is refused by name: a fail-closed run stops
 * with the policy-refusal status, and a run with --allow-unsupported-syscalls
 * returns ENOSYS, which is what Linux returns for every PI command when built
 * without CONFIG_FUTEX_PI (https://github.com/rrnewton/hermit/issues/3958).
 */

#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static uint32_t condition;
static uint32_t mutex;

int main(void) {
  long ret = syscall(
      SYS_futex, &condition, FUTEX_CMP_REQUEUE_PI_PRIVATE, 1, 0, &mutex, 0);
  if (ret < 0) {
    printf("cmp_requeue_pi: -1 %s\n", strerrorname_np(errno));
  } else {
    printf("cmp_requeue_pi: %ld\n", ret);
  }
  return 0;
}
