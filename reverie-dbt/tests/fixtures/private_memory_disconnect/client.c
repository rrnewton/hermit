/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * Licensed under the BSD-style license in the root LICENSE file. */
#include "dr_api.h"
#include "record.h"
#include <sys/syscall.h>

static _Atomic uint64_t *record;
static bool synchronize;

static void
retired(void)
{
    if (atomic_load_explicit(&record[ARMED], memory_order_acquire) == 0)
        return;
    uint64_t call = atomic_fetch_add(&record[CALLS], 1);
    if (atomic_load(&record[DESTROYED]) != 0)
        atomic_fetch_add(&record[LATE], 1);
    if (call == 0) {
        atomic_store_explicit(&record[ENTERED], 1, memory_order_release);
        /* Deliberately held by an independent controller. No DR call, I/O,
         * allocation or SDK lock inside the actual ownership observer. */
        while (atomic_load_explicit(&record[RELEASE], memory_order_acquire) == 0)
            __asm__ volatile("pause" ::: "memory");
        atomic_store_explicit(&record[COMPLETED], 1, memory_order_release);
    }
}

static void
notifier(void *argument)
{
    (void)argument;
    void *context = dr_get_current_drcontext();
    DR_ASSERT(context != NULL);
    atomic_store(&record[OWNER_TID], dr_get_thread_id(context));
    atomic_store_explicit(&record[READY], 1, memory_order_release);
    while (atomic_load_explicit(&record[START], memory_order_acquire) == 0)
        dr_thread_yield();
    atomic_store_explicit(&record[ARMED], 1, memory_order_release);
    /* The actual public application-style API reaches normalized SHM_REMAP
     * retirement before this deliberately invalid shmid fails in Linux. */
    reg_t result = dr_invoke_syscall_as_app(
        context, SYS_shmat, 3, (reg_t)-1, (reg_t)0, (reg_t)040000);
    atomic_store(&record[API_RESULT], (uint64_t)result);
    atomic_store_explicit(&record[API_RETURNED], 1, memory_order_release);
}

static void
synchronizer(void *argument)
{
    (void)argument;
    while (atomic_load_explicit(&record[SYNC_START], memory_order_acquire) == 0)
        dr_thread_yield();
    void **contexts = NULL;
    uint suspended = 0, unsuspended = 0;
    bool ok = dr_suspend_all_other_threads_ex(&contexts, &suspended, &unsuspended,
                                              DR_SUSPEND_NATIVE);
    DR_ASSERT(ok);
    for (uint i = 0; i < suspended; ++i) {
        if ((uint64_t)dr_get_thread_id(contexts[i]) == atomic_load(&record[OWNER_TID]) &&
            atomic_load_explicit(&record[COMPLETED], memory_order_acquire) == 0)
            atomic_store(&record[HELD_WAS_SUSPENDED], 1);
    }
    atomic_store(&record[UNSUSPENDED], unsuspended);
    bool resumed = dr_resume_all_other_threads(contexts, suspended);
    atomic_store(&record[RESUME_OK], resumed);
    atomic_store_explicit(&record[SYNC_DONE], 1, memory_order_release);
}

static void
exiting(void)
{
    atomic_store(&record[PROCESS_EXIT], 1);
    if (atomic_load_explicit(&record[COMPLETED], memory_order_acquire) != 1)
        atomic_store_explicit(&record[EARLY], 1, memory_order_release);
    /* This is the real process callback's client-state destruction boundary.
     * The shared observation page stays alive in the external controller. */
    atomic_store_explicit(&record[DESTROYED], 1, memory_order_release);
}

DR_EXPORT void
dr_client_main(client_id_t id, int argc, const char *argv[])
{
    (void)id;
    DR_ASSERT(argc == 3);
    synchronize = argv[2][0] == 's';
    file_t file = dr_open_file(argv[1], DR_FILE_READ | DR_FILE_WRITE_APPEND);
    DR_ASSERT(file != INVALID_FILE);
    size_t size = 4096;
    record = dr_map_file(file, &size, 0, NULL, DR_MEMPROT_READ | DR_MEMPROT_WRITE, 0);
    DR_ASSERT(record != NULL && size >= WORDS * sizeof(*record));
    dr_close_file(file);
    DR_ASSERT(atomic_load(&record[MAGIC]) == DISCONNECT_MAGIC);
    dr_set_private_reservation_retirement_callback(retired);
    dr_register_exit_event(exiting);
    DR_ASSERT(dr_create_client_thread(notifier, NULL));
    if (synchronize)
        DR_ASSERT(dr_create_client_thread(synchronizer, NULL));
}
