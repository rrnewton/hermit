/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * Licensed under the BSD-style license in the root LICENSE file. */
#ifndef PRIVATE_MEMORY_DISCONNECT_RECORD_H
#define PRIVATE_MEMORY_DISCONNECT_RECORD_H
#include <stdatomic.h>
#include <stdint.h>

enum {
    MAGIC, READY, START, ARMED, OWNER_TID, ENTERED, RELEASE, COMPLETED,
    CALLS, EXIT_REQUESTED, PROCESS_EXIT, EARLY, DESTROYED, API_RETURNED,
    API_RESULT, WAIT_OBSERVED, LATE, SYNC_START, SYNC_DONE,
    HELD_WAS_SUSPENDED, RESUME_OK, UNSUSPENDED, SAFE_OBSERVED, UNSAFE_OBSERVED, WORDS
};
#define DISCONNECT_MAGIC UINT64_C(0x444953434f4e4e31)
#define WITNESS_MAGIC UINT64_C(0x434c4f5345575431)
#define WITNESS_WAIT UINT64_C(1)
#define WITNESS_SAFE UINT64_C(2)
#define WITNESS_UNSAFE UINT64_C(3)
#define WITNESS_FD 253
struct disconnect_witness {
    uint64_t magic;
    uint64_t kind;
    uint64_t tid;
    uint64_t detail;
};
#endif
