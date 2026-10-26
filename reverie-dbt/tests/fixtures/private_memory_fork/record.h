/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * Licensed under the BSD-style license in the root LICENSE file. */
#include <stdatomic.h>
#include <stdint.h>
enum {
    MAGIC, OWNER_PID, PHASE, EARLY, TERMINAL, EXIT_EVENTS, LATE, FAILED,
    CHILD_PID, FORK_CALLS, FORK_DELTA, PARENT_READY, CHILD_DONE,
    EARLY_BEFORE_RELEASE, EXITS_BEFORE_RELEASE, EARLY_AT_ARMING, WORDS
};
#define FORK_MAGIC UINT64_C(0x464f524b4f574e52)
