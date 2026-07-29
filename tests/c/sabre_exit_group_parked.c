/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <assert.h>
#include <linux/futex.h>
#include <pthread.h>
#include <stdbool.h>
#include <stdatomic.h>
#include <stdint.h>
#include <sys/syscall.h>
#include <unistd.h>

static atomic_bool child_started;
static uint32_t futex_word;

static void *park_child(void *unused)
{
    (void)unused;
    atomic_store_explicit(&child_started, true, memory_order_release);

    /* The parent deliberately never wakes this wait. */
    (void)syscall(SYS_futex, &futex_word,
                  FUTEX_WAIT | FUTEX_PRIVATE_FLAG, 0, NULL, NULL, 0);
    _exit(91);
}

int main(void)
{
    pthread_t child;

    assert(pthread_create(&child, NULL, park_child, NULL) == 0);
    while (!atomic_load_explicit(&child_started, memory_order_acquire)) {
        assert(syscall(SYS_sched_yield) == 0);
    }

    (void)syscall(SYS_exit_group, 0);
    return 92;
}
