/* SPDX-License-Identifier: MIT */
#ifndef HERMIT_PROVIDER_FD_EFFECTS_H
#define HERMIT_PROVIDER_FD_EFFECTS_H
#include "provider.h"

/* These are physical receipts consumed by the existing FilesId/slot/OFD
 * authority. There is no descriptor-alias count or semantic FD table here.
 * Pointer keys remain private to BPF and never identify a published receipt. */
#define AP_ACCEPT_EFFECT 4
#define AP_FD_FILES 256
#define AP_FD_TABLES 64
#define AP_FD_JOURNAL 128
enum ap_fd_phase {
    AP_FD_ENTERED=1, AP_FD_LISTENER=2, AP_FD_DEQUEUED=4,
    AP_FD_FILE_RETURNED=8, AP_FD_INSTALL_ENTERED=16,
    AP_FD_INSTALL_RETURNED=32, AP_FD_SYSCALL_RETURNED=64
};
enum ap_fd_event_kind {
    AP_FD_INSTALL_BEGIN=1, AP_FD_INSTALL_END=2, AP_FD_REMOVE=3,
    AP_FD_REPLACE_BEGIN=4, AP_FD_REPLACE_OLD_FILE=5,
    AP_FD_REPLACE_END=6, AP_FD_FILE_RETIRED=7,
    AP_FD_UNRESOLVED_TABLE_MUTATION=8, AP_FD_TABLE_RETIRED=9
};
enum ap_fd_problem {
    AP_FD_CAPACITY=1, AP_FD_DUPLICATE=2, AP_FD_MISSING=4,
    AP_FD_IDENTITY=8, AP_FD_OUTCOME=16, AP_FD_UNKNOWN_TABLE=32
};
struct ap_fd_accept {
    u64 command, accept_lease, owner_mm, task, task_start;
    u64 table, file, install_begin, install_end;
    struct ap_identity listener, child;
    u64 creation, cookie, phases, problem;
    s32 requested_fd, flags, returned_fd, do_accept_errno;
};
/* BEGIN/END delimit callbacks, not a linearization point. A remove with the
 * same actual file can depend on an installation whose END is still pending.
 * A successful replacement names its incoming file and the actual old file
 * passed to filp_close, not a guessed pre-entry target-table snapshot. */
struct ap_fd_event {
    u64 sequence, kind, task, task_start, table, file, previous_file;
    u64 dependency, accept_command;
    s32 fd, returned;
    u64 complete;
};
struct ap_fd_status {
    u64 problem, next_table, next_file, next_event;
};
/* Used by actual collection and pure causal controls. This authenticates a
 * physical receipt, not current slot occupancy or semantic lifetime release. */
static __attribute__((always_inline)) inline int ap_fd_accept_matches(
    const struct ap_task_command *submitted,const struct ap_command_result *result,
    const struct ap_fd_accept *receipt) {
    if(!submitted || !result || !receipt || submitted->operation!=AP_ACCEPT_EFFECT ||
       !submitted->provider || !submitted->command ||
       result->command!=submitted->command || receipt->command!=submitted->command ||
       result->operation!=AP_ACCEPT_EFFECT || result->phase!=AP_COMMAND_DONE ||
       result->identity.provider!=submitted->provider ||
       receipt->accept_lease!=submitted->generation_before ||
       receipt->owner_mm!=submitted->generation_after ||
       !result->task || !result->start_boottime || receipt->task!=result->task ||
       receipt->task_start!=result->start_boottime ||
       receipt->requested_fd!=submitted->expected_level ||
       receipt->flags!=submitted->expected_option || !receipt->table || receipt->problem ||
       (receipt->phases&~127ULL) ||
       (receipt->phases&(AP_FD_ENTERED|AP_FD_SYSCALL_RETURNED))!=(AP_FD_ENTERED|AP_FD_SYSCALL_RETURNED))return 0;
    if(receipt->phases&AP_FD_LISTENER) {
        if(receipt->listener.provider!=submitted->provider ||
           receipt->listener.object!=submitted->expected_object || !receipt->listener.namespace)return 0;
    }
    if(receipt->phases&AP_FD_DEQUEUED) {
        if(!(receipt->phases&AP_FD_LISTENER) || !receipt->child.object ||
           receipt->child.provider!=submitted->provider ||
           receipt->child.namespace!=receipt->listener.namespace ||
           receipt->child.provider!=result->identity.provider ||
           receipt->child.object!=result->identity.object ||
           receipt->child.namespace!=result->identity.namespace ||
           !receipt->creation || receipt->creation!=result->creation ||
           !receipt->cookie || receipt->cookie!=result->cookie)return 0;
    } else if(result->identity.object || result->identity.namespace || result->creation || result->cookie)return 0;
    if(result->returned>=0) {
        return receipt->phases==127 && receipt->returned_fd==result->returned &&
            receipt->file && receipt->install_begin && receipt->install_end && !receipt->do_accept_errno;
    }
    return result->returned>=-4095 &&
        !(receipt->phases&(AP_FD_FILE_RETURNED|AP_FD_INSTALL_ENTERED|AP_FD_INSTALL_RETURNED)) &&
        !receipt->file && !receipt->install_begin && !receipt->install_end &&
        (!receipt->do_accept_errno || receipt->do_accept_errno==-result->returned);
}
#ifndef __BPF__
/* The exact task must already have a retained TASK_STORAGE registration.
 * This uses the same command reservation/collection/ACK owner as other ap_*
 * commands. No table permit is held while the guest accept blocks. */
int ap_prepare_accept(struct ap_session *, int exact_task_pidfd,
                      struct ap_identity listener, u64 accept_lease,
                      u64 owner_mm, int fd, int flags, u64 *command);
int ap_collect_accept(struct ap_session *, int exact_task_pidfd, u64 command,
                      struct ap_command_result *, struct ap_fd_accept *);
int ap_read_fd_status(struct ap_session *, struct ap_fd_status *);
int ap_read_fd_event(struct ap_session *, u64 sequence, struct ap_fd_event *);
/* ACK uses exact immutable bytes and cannot retire an incomplete callback.
 * It only retires physical evidence already retained by the engine; it does
 * not close a file, authorize an installed slot, or retire a semantic owner. */
int ap_ack_fd_event(struct ap_session *, const struct ap_fd_event *);
#endif
#endif
