/* SPDX-License-Identifier: GPL-2.0 */
#ifndef HERMIT_FD_JOURNAL_BPF_H
#define HERMIT_FD_JOURNAL_BPF_H
/* Reserve the same immutable journal row once through a shared helper. Every
 * caller retains all profile stores and the original final publication. */
static __attribute__((noinline)) struct ap_fd_event *fd_event_reserve(u64 *reserved_sequence) {
    struct ap_fd_status *s=fd_stats();if(!s)return 0;
    u64 sequence=__sync_fetch_and_add(&s->next_event,1)+1;
    if(!sequence) { fd_problem(AP_FD_CAPACITY);return 0; }
    /* Never reuse a journal key. Exact deletion of an immutable completed hash
     * entry permits a new, different key without a whole-value clear racing a
     * producer's payload stores (an array-slot memset would not be safe here). */
    struct ap_fd_event pending={.sequence=sequence,.complete=2};
    if(update(&fd_journal,&sequence,&pending,BPF_NOEXIST)) {
        fd_problem(AP_FD_CAPACITY);return 0;
    }
    struct ap_fd_event *e=lookup(&fd_journal,&sequence);
    if(!e) { fd_problem(AP_FD_MISSING);return 0; }
    *reserved_sequence=sequence;return e;
}
/* Share repeated stores without changing any row value or publication order.
 * The same already-reserved map row stays private until the final CAS. */
static __attribute__((noinline)) void fd_event_actor_fields(
        struct ap_fd_event *e,u64 kind,u64 actor,u64 start,u64 table) {
    e->kind=kind;e->task=actor;e->task_start=start;e->table=table;
}
static __attribute__((noinline)) void fd_event_file_fields(
        struct ap_fd_event *e,u64 file,u64 previous,u64 dependency,u64 command) {
    e->file=file;e->previous_file=previous;e->dependency=dependency;e->accept_command=command;
}
/* Every call first evaluates the same scalar operands into its private bounded
 * stack value. One shared publisher owns reservation and the exact stores;
 * complete remains2 until the original final CAS. No input issues a sequence. */
static __attribute__((noinline)) u64 fd_event_publish(const struct ap_fd_event *fields) {
    u64 sequence=0;struct ap_fd_event *e=fd_event_reserve(&sequence);if(!e)return 0;
    e->sequence=sequence;
    fd_event_actor_fields(e,fields->kind,fields->task,fields->task_start,fields->table);
    fd_event_file_fields(e,fields->file,fields->previous_file,fields->dependency,fields->accept_command);
    e->fd=fields->fd;e->returned=fields->returned;
    e->mode=fields->mode;e->status_flags=fields->status_flags;
    e->device_major=fields->device_major;e->device_minor=fields->device_minor;
    e->source_ioctl_dispatch=fields->source_ioctl_dispatch;
    __sync_val_compare_and_swap(&e->complete,2,1);
    return sequence;
}
INLINE u64 fd_event_for_profile(u64 actor,u64 start,u64 kind,u64 table,s32 fd,u64 file,u64 previous,
                        u64 dependency,u64 command,s32 returned,u32 mode,u32 status_flags,u32 device_major,u32 device_minor,u64 source_ioctl_dispatch) {
    const struct ap_fd_event fields={.kind=kind,.task=actor,.task_start=start,.table=table,
        .file=file,.previous_file=previous,.dependency=dependency,.accept_command=command,
        .fd=fd,.returned=returned,.mode=mode,.status_flags=status_flags,
        .device_major=device_major,.device_minor=device_minor,.source_ioctl_dispatch=source_ioctl_dispatch};
    return fd_event_publish(&fields);
}
INLINE u64 fd_event_for(u64 actor,u64 start,u64 kind,u64 table,s32 fd,u64 file,u64 previous,
                        u64 dependency,u64 command,s32 returned) {
    return fd_event_for_profile(actor,start,kind,table,fd,file,previous,dependency,command,returned,0,0,0,0,0);
}
/* Immutable caller-owned scalar operands only. Every caller evaluates the
 * same original arguments before current actor sampling. Expanding the full
 * event's zero/default fields here avoids repeated whole-event initializers;
 * neither a map row nor a publication state is used as scratch. */
struct ap_fd_event_seed {
    u64 kind,table,file,previous_file,dependency,accept_command;
    s32 fd,returned;
};
static __attribute__((noinline)) u64 fd_event_current(const struct ap_fd_event_seed *seed) {
    struct ap_fd_event fields={.kind=seed->kind,.table=seed->table,.file=seed->file,
        .previous_file=seed->previous_file,.dependency=seed->dependency,
        .accept_command=seed->accept_command,.fd=seed->fd,.returned=seed->returned};
    fields.task=pid_tgid();fields.task_start=CORE(current_task()->start_boottime);
    return fd_event_publish(&fields);
}
INLINE u64 fd_event(u64 kind,u64 table,s32 fd,u64 file,u64 previous,
                    u64 dependency,u64 command,s32 returned) {
    const struct ap_fd_event_seed seed={.kind=kind,.table=table,.file=file,.previous_file=previous,
        .dependency=dependency,.accept_command=command,.fd=fd,.returned=returned};
    return fd_event_current(&seed);
}
#endif
