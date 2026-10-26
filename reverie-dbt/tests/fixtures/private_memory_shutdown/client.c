/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * Licensed under the BSD-style license in the root LICENSE file. */
#include "dr_api.h"
#include <stdatomic.h>
#include <stdint.h>
#include <string.h>
#include <sys/syscall.h>

/* Parent-owned shared observation storage survives client state teardown. It
 * is not an ownership-provider input. Only this fixed scalar record is shared. */
enum {
    MAGIC,
    THREAD_READY,
    ARMED,
    RETIREMENTS,
    EXIT_SEEN,
    LATE_RETIREMENTS,
    WRITES,
    FAILED,
    WORDS
};
static _Atomic uint64_t *record;
static bool force_exit;

static void
retired(void)
{
    /* The observer contract permits only preallocated state updates: no DR,
     * allocation, I/O, injected syscall or lock. */
    if (atomic_load(&record[ARMED]) != 0) {
        if (atomic_load(&record[EXIT_SEEN]) != 0)
            atomic_fetch_add(&record[LATE_RETIREMENTS], 1);
        else
            atomic_fetch_add(&record[RETIREMENTS], 1);
    }
}

static void
thread_init(void *context)
{
    (void)context;
    atomic_store(&record[THREAD_READY], 1);
}

static bool
filter(void *context, int number)
{
    (void)context;
    return number == SYS_write;
}

static bool
before(void *context, int number)
{
    if (number != SYS_write || dr_syscall_get_param(context, 0) != 1)
        return true;
    if (atomic_fetch_add(&record[WRITES], 1) != 0 ||
        atomic_load(&record[THREAD_READY]) != 1)
        atomic_store(&record[FAILED], 1);
    /* Ignore initialization notifications. The no-libc guest has only this
     * write and exit after arming, with no mapping-replacement operations. */
    atomic_store(&record[RETIREMENTS], 0);
    atomic_store(&record[ARMED], 1);
    if (force_exit) {
        dr_fprintf(STDERR, "PRIVATE_MEMORY_SHUTDOWN requested-exit=101\n");
        dr_exit_process(101);
        atomic_store(&record[FAILED], 1);
    }
    return true;
}

static void
exiting(void)
{
    uint64_t count = atomic_load(&record[RETIREMENTS]);
    if (atomic_load(&record[THREAD_READY]) != 1 ||
        atomic_load(&record[ARMED]) != 1 ||
        atomic_load(&record[WRITES]) != 1 || count != 1)
        atomic_store(&record[FAILED], 1);
    dr_fprintf(STDERR,
               "PRIVATE_MEMORY_SHUTDOWN exit retirement=%llu failed=%llu\n",
               (unsigned long long)count,
               (unsigned long long)atomic_load(&record[FAILED]));
    /* Poison the observer's logical client-state lifetime only after checking
     * final withdrawal. A forbidden later call records a separate failure if
     * the code remains mapped; an unloaded callback instead fails by status. */
    atomic_store(&record[EXIT_SEEN], 1);
}

DR_EXPORT void
dr_client_main(client_id_t id, int argc, const char *argv[])
{
    (void)id;
    DR_ASSERT(argc == 3);
    DR_ASSERT(strcmp(argv[1], "exit101") == 0 || strcmp(argv[1], "normal") == 0);
    force_exit = strcmp(argv[1], "exit101") == 0;
    file_t file = dr_open_file(argv[2], DR_FILE_READ | DR_FILE_WRITE_APPEND);
    DR_ASSERT(file != INVALID_FILE);
    size_t size = 4096;
    record = dr_map_file(file, &size, 0, NULL, DR_MEMPROT_READ | DR_MEMPROT_WRITE, 0);
    DR_ASSERT(record != NULL && size >= WORDS * sizeof(*record));
    dr_close_file(file);
    for (size_t i = 0; i < WORDS; ++i)
        atomic_init(&record[i], 0);
    atomic_store(&record[MAGIC], UINT64_C(0x53485554444f574e));
    dr_set_private_reservation_retirement_callback(retired);
    dr_register_thread_init_event(thread_init);
    dr_register_filter_syscall_event(filter);
    dr_register_pre_syscall_event(before);
    dr_register_exit_event(exiting);
}
