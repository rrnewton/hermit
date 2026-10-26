/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * Licensed under the BSD-style license in the root LICENSE file. */
#include "dr_api.h"
#include "record.h"
#include <string.h>
#include <sys/syscall.h>

static _Atomic uint64_t *record;
static bool shared;

static void
retired(void)
{
    /* No DR, syscall, allocation, wait or lock in the actual observer. */
    if (atomic_load(&record[EXIT_EVENTS]) != 0)
        atomic_fetch_add(&record[LATE], 1);
    else if (atomic_load(&record[PHASE]) == 1)
        atomic_fetch_add(&record[EARLY], 1);
    else if (atomic_load(&record[PHASE]) == 2)
        atomic_fetch_add(&record[TERMINAL], 1);
}

static bool
filter(void *context, int number)
{
    (void)context;
    return number == SYS_fork || number == SYS_write;
}

static bool
before(void *context, int number)
{
    if (!shared && number == SYS_fork)
        atomic_store(&record[PHASE], 1);
    if (shared && number == SYS_write && dr_syscall_get_param(context, 0) == 197) {
        /* Child application already wrote its ready byte to the parent. Start
         * the non-resettable observation before the parent actually exits. */
        atomic_store(&record[PHASE], 1);
    }
    if (shared && number == SYS_write && dr_syscall_get_param(context, 0) == 1) {
        uint64_t early = atomic_load(&record[EARLY]);
        atomic_store(&record[EARLY_AT_ARMING], early);
        if (early != 0 || atomic_load(&record[PHASE]) != 1 ||
            atomic_load(&record[EXIT_EVENTS]) != 0 ||
            atomic_load(&record[PARENT_READY]) != 1)
            atomic_store(&record[FAILED], 1);
        atomic_store(&record[CHILD_PID], dr_get_process_id());
        /* EARLY is never reset: this arms only the final write/exit interval. */
        atomic_store(&record[TERMINAL], 0);
        atomic_store(&record[PHASE], 2);
    }
    return true;
}

static void
after(void *context, int number)
{
    if (shared || number != SYS_fork || (ptr_int_t)dr_syscall_get_result(context) <= 0)
        return;
    while (atomic_load_explicit(&record[FORK_CALLS], memory_order_acquire) == 0)
        __asm__ volatile("pause" ::: "memory");
    atomic_store_explicit(&record[PARENT_READY], 1, memory_order_release);
    while (atomic_load_explicit(&record[CHILD_DONE], memory_order_acquire) == 0)
        __asm__ volatile("pause" ::: "memory");
}

static void
forked(void *context)
{
    (void)context;
    atomic_fetch_add(&record[FORK_CALLS], 1);
    if (shared) {
        atomic_store(&record[FAILED], 1);
        return;
    }
    while (atomic_load_explicit(&record[PARENT_READY], memory_order_acquire) == 0)
        __asm__ volatile("pause" ::: "memory");
    uint64_t before_count = atomic_load(&record[EARLY]);
    reg_t args[6] = {(reg_t)(ptr_uint_t)dr_get_private_reservations, 1, 0, 0, 0, 0};
    dr_test_private_reservation_syscall(SYS_munmap, args);
    uint64_t delta = atomic_load(&record[EARLY]) - before_count;
    atomic_store(&record[FORK_DELTA], delta);
    if (delta != 0)
        atomic_store(&record[FAILED], 1);
    atomic_store_explicit(&record[CHILD_DONE], 1, memory_order_release);
}

static void
exiting(void)
{
    if (!shared) {
        if ((uint64_t)dr_get_process_id() == atomic_load(&record[OWNER_PID]))
            dr_fprintf(STDERR, "PRIVATE_MEMORY_FORK separate delta=%llu failed=%llu\n",
                       (unsigned long long)atomic_load(&record[FORK_DELTA]),
                       (unsigned long long)atomic_load(&record[FAILED]));
        return;
    }
    uint64_t count = atomic_load(&record[TERMINAL]);
    if (atomic_load(&record[PHASE]) != 2 || count != 1 ||
        atomic_load(&record[EARLY]) != 0 ||
        (uint64_t)dr_get_process_id() == atomic_load(&record[OWNER_PID]))
        atomic_store(&record[FAILED], 1);
    atomic_fetch_add(&record[EXIT_EVENTS], 1);
    dr_fprintf(STDERR, "PRIVATE_MEMORY_FORK shared early=%llu terminal=%llu failed=%llu\n",
               (unsigned long long)atomic_load(&record[EARLY]),
               (unsigned long long)count,
               (unsigned long long)atomic_load(&record[FAILED]));
}

DR_EXPORT void
dr_client_main(client_id_t id, int argc, const char *argv[])
{
    (void)id;
    DR_ASSERT(argc == 3);
    DR_ASSERT(strcmp(argv[1], "separate") == 0 || strcmp(argv[1], "shared") == 0);
    shared = strcmp(argv[1], "shared") == 0;
    file_t file = dr_open_file(argv[2], DR_FILE_READ | DR_FILE_WRITE_APPEND);
    DR_ASSERT(file != INVALID_FILE);
    size_t size = 4096;
    record = dr_map_file(file, &size, 0, NULL, DR_MEMPROT_READ | DR_MEMPROT_WRITE, 0);
    DR_ASSERT(record != NULL && size >= WORDS * sizeof(*record));
    dr_close_file(file);
    for (size_t i = 0; i < WORDS; ++i)
        atomic_init(&record[i], 0);
    atomic_store(&record[MAGIC], FORK_MAGIC);
    atomic_store(&record[OWNER_PID], dr_get_process_id());
    dr_set_private_reservation_retirement_callback(retired);
    dr_register_filter_syscall_event(filter);
    dr_register_pre_syscall_event(before);
    dr_register_post_syscall_event(after);
    dr_register_fork_init_event(forked);
    dr_register_exit_event(exiting);
}
