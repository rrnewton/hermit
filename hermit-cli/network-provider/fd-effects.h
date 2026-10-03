/* SPDX-License-Identifier: MIT */
#ifndef HERMIT_PROVIDER_FD_EFFECTS_H
#define HERMIT_PROVIDER_FD_EFFECTS_H
#include "provider.h"
#include "stream-copy.h"
#include "stream-tx.h"

/* These are physical receipts consumed by the existing FilesId/slot/OFD
 * authority. There is no descriptor-alias count or semantic FD table here.
 * Pointer keys remain private to BPF and never identify a published receipt. */
#define AP_ACCEPT_EFFECT 4
#define AP_ORIGINAL_CONNECT 7
#define AP_ORIGINAL_CLOSE 9
#define AP_ORIGINAL_FILE 10
#define AP_ORIGINAL_READ 11
#define AP_ORIGINAL_SOCKET_CALL 12
#define AP_ORIGINAL_OPENAT_CALL 18
#define AP_ORIGINAL_EPOLL_CALL 19
#define AP_ORIGINAL_EPOLL_CTL 20
#define AP_ORIGINAL_RECVFROM_CALL 21
#define AP_ORIGINAL_RECVMSG_CALL 22
#define AP_RECVFROM_SYSCALL 45
#define AP_RECVMSG_SYSCALL 47
#define AP_EPOLL_CREATE_SYSCALL 213
#define AP_EPOLL_CREATE1_SYSCALL 291
#define AP_EPOLL_CLOEXEC 02000000U
#define AP_AUXILIARY_FILE 23
#define AP_OPENAT_SYSCALL 257
#define AP_SOCKET_SYSCALL 41
#define AP_READ_SYSCALL 0
#define AP_READ_MAX_COUNT 0x7ffff000ULL
/* Bound x86-64 first shared file operation. Other shapes require real issuers. */
#define AP_FILE_SYSCALL_FCNTL 72
#define AP_FILE_GET_FLAGS 3
static inline int ap_original_file_shape(u64 syscall_nr,s32 command) {
    return syscall_nr==AP_FILE_SYSCALL_FCNTL && command==AP_FILE_GET_FLAGS;
}
static inline int ap_original_allocator(u64 operation) {
    return operation==AP_ORIGINAL_SOCKET_CALL || operation==AP_ORIGINAL_OPENAT_CALL ||
        operation==AP_ORIGINAL_EPOLL_CALL;
}
/* Auxiliary F_GETFL has the same actual file issuer and exact operands as
 * guest op10, but cannot implicitly enroll its controller descriptor table. */
static inline int ap_original_file_operation(u64 operation) {
    return operation==AP_ORIGINAL_FILE || operation==AP_AUXILIARY_FILE;
}
enum ap_fd_table_access_mode {
    AP_TABLE_ENROLLED_LOOKUP=0, AP_TABLE_ENROLLED_CREATE=1,
    AP_TABLE_NORMALIZE=2, AP_TABLE_ANY_LOOKUP=3, AP_TABLE_CENSUS_ENROLL=4
};
/* 2 means the sole explicit census upgrade, never an ordinary selection. */
static inline int ap_fd_table_access(u64 enrolled,u32 mode) {
    if(enrolled>1 || mode>AP_TABLE_CENSUS_ENROLL)return 0;
    if(mode==AP_TABLE_NORMALIZE || mode==AP_TABLE_ANY_LOOKUP)return 1;
    if(enrolled)return 1;
    return mode==AP_TABLE_CENSUS_ENROLL?2:0;
}
/* Both typed receives join the existing command/Call and copy stream. The
 * initial supported shapes authenticate native helper Drain and Peek exactly;
 * additional guest flags/address/ancillary shapes need their own real facts. */
static inline int ap_original_recv(u64 operation) {
    return operation==AP_ORIGINAL_RECVFROM_CALL || operation==AP_ORIGINAL_RECVMSG_CALL;
}
static inline int ap_original_receive(u64 operation) {
    return operation==AP_ORIGINAL_READ || ap_original_recv(operation);
}
static inline u64 ap_original_copy_disposition(u64 operation,s32 flags) {
    if(operation==AP_ORIGINAL_READ && !flags)return AP_STREAM_COPY_CONSUME;
    if(operation==AP_ORIGINAL_RECVFROM_CALL && flags==0x40)return AP_STREAM_COPY_CONSUME;
    if(operation==AP_ORIGINAL_RECVMSG_CALL && flags==0x42)return AP_STREAM_COPY_OBSERVE;
    return 0;
}
/* Private argument snapshot only: no map, receipt or new authority. Keep
 * every exact original operand while sharing the predicate under BPF's five
 * register argument ABI. The public scalar wrapper remains unchanged. */
struct ap_recv_operand_snapshot {u64 nr,fd,address,count_or_flags,flags,source,source_length;};
#ifdef __BPF__
static __attribute__((noinline)) int
#else
static __attribute__((always_inline)) inline int
#endif
ap_original_recv_operands_shared(const struct ap_task_command *c,
        const struct ap_recv_operand_snapshot *p) {
    if(!p)return 0;
    const u64 nr=p->nr,fd=p->fd,address=p->address,count_or_flags=p->count_or_flags;
    const u64 flags=p->flags,source=p->source,source_length=p->source_length;
    if(!c || !ap_original_recv(c->operation) || !c->provider || !c->command ||
       !c->expected_object || c->original_count>AP_READ_MAX_COUNT ||
       !ap_original_copy_disposition(c->operation,c->expected_option) ||
       (s32)fd!=c->expected_level || address!=c->generation_before)return 0;
    if(c->operation==AP_ORIGINAL_RECVFROM_CALL)
        return nr==AP_RECVFROM_SYSCALL && count_or_flags==c->original_count &&
            (s32)flags==c->expected_option && !source && !source_length;
    return nr==AP_RECVMSG_SYSCALL && (s32)count_or_flags==c->expected_option;

}
/* BPF has five argument registers; this eight-operand adapter must inline. */
static __attribute__((always_inline)) inline int ap_original_recv_operands(const struct ap_task_command *c,
        u64 nr,u64 fd,u64 address,u64 count_or_flags,u64 flags,u64 source,u64 source_length) {
    const struct ap_recv_operand_snapshot p={nr,fd,address,count_or_flags,flags,source,source_length};
    return ap_original_recv_operands_shared(c,&p);
}
static inline int ap_original_operation(u64 operation) {
    return operation==AP_ORIGINAL_CONNECT || operation==AP_ORIGINAL_CLOSE ||
        ap_original_file_operation(operation) || ap_original_receive(operation) ||
        operation==AP_ORIGINAL_EPOLL_CTL || ap_original_allocator(operation) ||
        operation==AP_ORIGINAL_SENDTO_CALL;
}
#define AP_NATIVE_BIRTH 8
/* Exact x86-64 syscall shape retained from the original Tool invocation. */
static __attribute__((always_inline)) inline int ap_native_birth_syscall(s64 nr) {
    return nr==56 || nr==57 || nr==58 || nr==435;
}
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
    AP_FD_UNRESOLVED_TABLE_MUTATION=8, AP_FD_TABLE_RETIRED=9,
    AP_FD_REMOVE_BEGIN=10, AP_FD_REMOVE_NO_FILE=11,
    AP_FD_TABLE_PUT_BEGIN=12, AP_FD_TABLE_PUT_END=13,
    AP_FD_COPY_BEGIN=14, AP_FD_COPY_SLOT=15, AP_FD_COPY_END=16,
    AP_FD_EXEC_BEGIN=17, AP_FD_EXEC_REMOVE=18, AP_FD_EXEC_END=19,
    AP_FD_ENROLL_BEGIN=20, AP_FD_ENROLL_SLOT=21, AP_FD_ENROLL_END=22
};
enum ap_fd_problem {
    AP_FD_CAPACITY=1, AP_FD_DUPLICATE=2, AP_FD_MISSING=4,
    AP_FD_IDENTITY=8, AP_FD_OUTCOME=16, AP_FD_UNKNOWN_TABLE=32,
    AP_FD_COPY_CUSTODY=64
};
/* Private invocation state. word is a borrowed kernel pointer plus fdput
 * flags; it is never exported and never dereferenced after the native return.
 * An absent callback is UNKNOWN, including when missed counters are zero. */
struct ap_fd_selection { u64 entered, returned, word; };
#ifdef __BPF__
static __attribute__((noinline)) int
#else
static __attribute__((always_inline)) inline int
#endif
ap_fd_selection_enter(
    struct ap_fd_selection *s) {
    if(s->entered || s->returned || s->word)return 0;
    s->entered=1;return 1;
}
#ifdef __BPF__
static __attribute__((noinline)) int
#else
static __attribute__((always_inline)) inline int
#endif
ap_fd_selection_return(
    struct ap_fd_selection *s,u64 word) {
    /* fdget does not acquire the fdget_pos position lock. EMPTY_FD is exactly
     * zero. Both BORROWED_FD and CLONED_FD are valid, including files->count=1. */
    if(s->entered!=1 || s->returned || s->word || (word&2) ||
       ((word&~3ULL)==0 && word))return 0;
    s->word=word;s->returned=1;return 1;
}
#ifdef __BPF__
static __attribute__((noinline)) int ap_fd_selection_complete(
#else
static __attribute__((always_inline)) inline int ap_fd_selection_complete(
#endif

    const struct ap_fd_selection *s) {
    return s->entered==1 && s->returned==1 && !(s->word&2) &&
        ((s->word&~3ULL)!=0 || !s->word);
}
struct ap_fd_accept {
    u64 command, accept_lease, owner_mm, task, task_start;
    u64 table, file, install_begin, install_end;
    struct ap_identity listener, child;
    u64 creation, cookie, phases, problem;
    s32 requested_fd, flags, returned_fd, do_accept_errno;
};
/* Original connect's two publications belong to one existing command ticket.
 * Only normalized file/table identities cross the provider boundary. ready is
 * published after the complete immutable selection, before sockaddr uaccess;
 * complete is published after the actual native return. Neither a missing
 * hook nor a zero missed counter is a negative selection receipt. */
struct ap_original_selection {
    u64 command, call, owner_mm, provider, task, task_start;
    u64 table, file, user_address, fdput_flags, ready;
    s32 requested_fd, address_length;
    /* ABI7: exact scalar Read count; zero on Connect/Close/GetFlags. */
    u64 original_count;
};
/* Op12 occupies only the otherwise unused sockaddr storage. Existing ABI7
 * Connect/Close/GetFlags/Read layouts and bytes are unchanged. Neither a
 * requested protocol nor this installation implies the actual socket class. */
struct ap_original_socket_installation { u64 begin,end; s32 fd; u32 reserved; };
/* Openat additionally retains only an actual borrowed-file profile at
 * fd_install entry. This is class/status evidence for an already removed
 * installation, never filesystem getattr/stat or a live-slot certificate. */
struct ap_original_openat_installation {
    struct ap_original_socket_installation installed;
    u32 mode,status_flags,device_major,device_minor;
};
/* Epoll uses the original creator as the anonymous-file class witness. The
 * descriptor bit comes from this allocating table at fd_install entry, never
 * from f_flags, input flags, or a later descriptor lookup. */
struct ap_original_epoll_installation {
    struct ap_original_socket_installation installed;
    u32 status_flags,descriptor_flags,profiled,reserved;
};
static inline int ap_original_epoll_syscall(u64 nr) {
    return nr==AP_EPOLL_CREATE_SYSCALL || nr==AP_EPOLL_CREATE1_SYSCALL;
}
/* Immutable selected prefix ends before ctl_returned. Actual syscall entry
 * precedes guest copy; actual primary/secondary fdget callbacks are distinct.
 * Empty primary positively proves that secondary is not reached on this image.
 * These fields describe one original native operation, not a ready-list model. */
struct ap_original_epoll_ctl {
    u64 entered,ctl_entered,primary_selected,secondary_selected;
    u64 target_file,target_flags,before,primary_cut,target_cut,image_wakeup_policy;
    u8 event[12];u32 reserved;
    u64 ctl_returned;s32 ctl_result;u32 result_reserved;
};
struct ap_original_result {
    struct ap_original_selection selection;
    union { u8 address[128]; struct ap_original_socket_installation installation;
        struct ap_original_openat_installation opened;
        struct ap_original_epoll_installation epoll;
        struct ap_original_epoll_ctl epoll_ctl;
        struct ap_stream_copy_state stream_copy; struct ap_stream_tx_state stream_tx; };
    u64 copy_entered, copy_returned, copy_remaining;
    u64 audit_entered, audit_returned;
    u64 security_entered, security_returned, complete, problem;
    s32 audit_result, security_result, returned;
    u32 reserved;
};
/* One original clone invocation. ready is published once at the actual
 * sched_process_fork event, before wake_up_new_task. No guest clone3 bytes or
 * kernel pointer identities cross this boundary. A command can have ONE birth.
 * parent_* and effective exit_signal come from the exact pre-publication
 * klp_copy_process witness under tasklist_lock, committed only by matched fork. */
struct ap_native_birth {
    u64 command, call, owner_mm, provider;
    u64 creator_task, creator_start, creator_table;
    u64 child_task, child_start, child_table;
    u64 parent_task, parent_start;
    u64 copy_begin, copy_end;
    u64 pidfd_install_begin, pidfd_install_end, pidfd_file;
    u64 kernel_flags, ready, problem;
    u32 shared_mm, shared_files, same_thread_group;
    s32 exit_signal, requested_exit_signal, pidfd_fd;
    /* Actual child field at the chosen-parent witness, committed by fork.
     * Zero remains legal with CLEARTID; this is never CHILD_SETTID. */
    u64 clear_child_tid;
};
#define AP_CLONE_VM 0x100ULL
#define AP_CLONE_FILES 0x400ULL
#define AP_CLONE_PIDFD 0x1000ULL
#define AP_CLONE_THREAD 0x10000ULL
#define AP_CLONE_CHILD_CLEARTID 0x200000ULL
/* Validate only positive facts. An empty/missing record is never a no-child
 * result. Initial-namespace lineage still requires a semantic identity join. */
static __attribute__((always_inline)) inline int ap_native_birth_matches(
    const struct ap_task_command *c,const struct ap_native_birth *b) {
    if(!c || !b || c->operation!=AP_NATIVE_BIRTH || !c->provider ||
       !c->command || !c->expected_object || !c->generation_before ||
       b->command!=c->command || b->call!=c->expected_object ||
       b->provider!=c->provider || b->owner_mm!=c->generation_after ||
       b->creator_table!=c->generation_before || !b->creator_task ||
       !b->creator_start || !b->child_task || !b->child_start ||
       b->child_task==b->creator_task || !b->child_table ||
       !b->parent_task || !b->parent_start || b->ready!=1 || b->problem ||
       b->shared_mm>1 || b->shared_files>1 || b->same_thread_group>1 ||
       b->shared_mm!=!!(b->kernel_flags&AP_CLONE_VM) ||
       b->shared_files!=!!(b->kernel_flags&AP_CLONE_FILES) ||
       b->same_thread_group!=!!(b->kernel_flags&AP_CLONE_THREAD) ||
       b->same_thread_group!=((b->creator_task>>32)==(b->child_task>>32)) ||
       b->requested_exit_signal<0 || b->requested_exit_signal>64 ||
       b->exit_signal< -1 || b->exit_signal>64)return 0;
    if(!(b->kernel_flags&AP_CLONE_CHILD_CLEARTID) && b->clear_child_tid)return 0;
    if(b->shared_files) {
        if(b->child_table!=b->creator_table || b->copy_begin || b->copy_end)return 0;
    } else if(b->child_table==b->creator_table || !b->copy_begin ||
              b->copy_end<=b->copy_begin)return 0;
    if(b->kernel_flags&AP_CLONE_PIDFD) {
        if(!b->pidfd_install_begin || b->pidfd_install_end<=b->pidfd_install_begin ||
           !b->pidfd_file || b->pidfd_fd<0)return 0;
    } else if(b->pidfd_install_begin || b->pidfd_install_end || b->pidfd_file || b->pidfd_fd!=-1)return 0;
    return 1;
}
/* Physical retirement of a finally dead task; never a native return receipt.
 * Preserve whatever producer bytes existed, including DONE, without filling in
 * missing callbacks or changing the original command phase/result. */
struct ap_original_terminal {
    struct ap_command_result command;
    struct ap_original_result original;
    u64 call, fd_call_present, task_absent;
};
/* A finally dead creator and an already admitted positive child. This never
 * synthesizes the creator's syscall return. Unknown/no-child rows are retained. */
struct ap_native_birth_terminal {
    struct ap_command_result command;
    struct ap_native_birth birth;
    u64 call, fd_call_present, task_absent;
};
/* This is the existing fd_calls map value, not a second call registry. Raw
 * references stay private and are dereferenced only during their native
 * lifetime. Original-connect rows remain owned until the exact final ACK. */
#include "epoll-ctl-copy.h"
struct ap_fd_call {
    u64 command, raw_table, new_file, install_begin;
    /* File selection stores the raw classic-probe entry PC in the same
     * private storage; other operations retain their actual function base. */
    union { u64 function_ip; u64 file_entry_ip; };
    u64 selected_file, operation;
    struct ap_fd_selection selection;
    union {
        struct ap_original_result original;
        struct ap_native_birth birth;
        struct ap_epoll_ctl_copy epoll_copy;
    };
    union { u64 copied_address; u64 fdget_role; };
    u64 security_socket;
    /* Connect/File entry frame exists only until the separate post-fdget witness. That
     * witness clears it before the later security argument can occupy it. Read retains
     * its exact caller frame until sys_exit to disqualify duplicate direct callbacks. */
    union { u64 entry_stack; u64 security_address; };
};
/* Caller-owned fresh stack row only, before its first field store and
 * BPF_NOEXIST publication. Clearing a live fd_calls value is forbidden.
 * This shares the full existing object initialization, including all union
 * bytes; it creates no map scratch and does not issue an identity or phase. */
#ifdef __BPF__
static __attribute__((noinline)) void
#else
static __attribute__((always_inline)) inline void
#endif
ap_fd_call_clear(struct ap_fd_call *call) {
    __builtin_memset(call,0,sizeof(*call));
}
/* TASK_STORAGE on a newborn is an admission marker, not ownership of the
 * creator's syscall return. READY can exist only on the originally armed task:
 * fd_native_fork copies the marker only after the exact RUNNING claim. Once
 * claimed, both issuer and newborn are checked against that immutable actor
 * generation and the committed fork row before interpreting any return. */
enum ap_native_actor { AP_NATIVE_ACTOR_INVALID, AP_NATIVE_ACTOR_CREATOR, AP_NATIVE_ACTOR_CHILD };
static __attribute__((always_inline)) inline enum ap_native_actor ap_native_birth_actor(
    const struct ap_task_command *c,const struct ap_command_result *r,
    const struct ap_fd_call *creator,u64 task,u64 start) {
    if(!c || !r || !task || !start || !c->provider || !c->command ||
       c->operation!=AP_NATIVE_BIRTH || r->operation!=AP_NATIVE_BIRTH ||
       r->command!=c->command || !ap_native_birth_syscall(c->expected_level) ||
       c->expected_option)return AP_NATIVE_ACTOR_INVALID;
    if(r->phase==AP_COMMAND_READY)
        return !r->task && !r->start_boottime && !creator ? AP_NATIVE_ACTOR_CREATOR : AP_NATIVE_ACTOR_INVALID;
    if((r->phase!=AP_COMMAND_RUNNING && r->phase!=AP_COMMAND_DONE) ||
       !r->task || !r->start_boottime)return AP_NATIVE_ACTOR_INVALID;
    if(task==r->task && start==r->start_boottime)return AP_NATIVE_ACTOR_CREATOR;
    if(!creator || creator->operation!=AP_NATIVE_BIRTH || creator->command!=c->command ||
       creator->birth.command!=c->command || creator->birth.provider!=c->provider ||
       creator->birth.call!=c->expected_object || creator->birth.owner_mm!=c->generation_after ||
       creator->birth.creator_table!=c->generation_before || creator->birth.problem ||
       creator->birth.ready!=1 || creator->birth.creator_task!=r->task ||
       creator->birth.creator_start!=r->start_boottime || creator->birth.child_task==r->task ||
       creator->birth.child_task!=task || creator->birth.child_start!=start)
        return AP_NATIVE_ACTOR_INVALID;
    return AP_NATIVE_ACTOR_CHILD;
}
/* Return classification is shared by the actual sys_exit producer and pure
 * controls. READY plus an ACTUAL negative native return is an early error;
 * absence alone never reaches this predicate. RUNNING must already retain the
 * paired kernel_clone return and its positive/negative birth relation. */
enum ap_native_return { AP_NATIVE_RETURN_INVALID, AP_NATIVE_RETURN_EARLY_ERROR, AP_NATIVE_RETURN_KERNEL };
static __attribute__((always_inline)) inline enum ap_native_return ap_native_birth_return(
    const struct ap_task_command *c,const struct ap_command_result *r,
    const struct ap_fd_call *call,s64 syscall,s64 returned) {
    if(!c || !r || c->operation!=AP_NATIVE_BIRTH || !c->provider || !c->command ||
       !c->expected_object || !c->generation_before ||
       !ap_native_birth_syscall(c->expected_level) || c->expected_option ||
       syscall!=c->expected_level || r->command!=c->command || r->operation!=AP_NATIVE_BIRTH ||
       !returned || returned< -4095 || returned>0x7fffffffLL)return AP_NATIVE_RETURN_INVALID;
    if(r->phase==AP_COMMAND_READY)
        return !call && returned<0 ? AP_NATIVE_RETURN_EARLY_ERROR : AP_NATIVE_RETURN_INVALID;
    if(r->phase!=AP_COMMAND_RUNNING || !call || call->operation!=AP_NATIVE_BIRTH ||
       call->command!=c->command || call->copied_address!=1 || call->birth.problem ||
       !r->task || !r->start_boottime || call->birth.creator_task!=r->task ||
       call->birth.creator_start!=r->start_boottime || call->birth.provider!=c->provider ||
       call->birth.call!=c->expected_object || call->birth.owner_mm!=c->generation_after ||
       call->birth.creator_table!=c->generation_before || r->identity.provider!=c->provider ||
       r->returned!=returned || (returned>0 && call->birth.ready!=1) ||
       (returned<0 && call->birth.ready))return AP_NATIVE_RETURN_INVALID;
    return AP_NATIVE_RETURN_KERNEL;
}
/* Share the full predicate in BPF; every caller retains all operation checks. */
#ifdef __BPF__
static __attribute__((noinline)) int ap_original_selection_matches(
#else
static __attribute__((always_inline)) inline int ap_original_selection_matches(
#endif
    const struct ap_task_command *submitted,const struct ap_original_selection *s) {
    /* AUTONOMOUS-BOT-IMPLEMENTED: scalar original Read observation only.
     * TODO-HUMAN-REVIEW(PR-id): activation remains disabled pending review.
     * Common identity/operand guards are identical for every admitted opcode;
     * operation-specific constraints below remain mandatory. */
    if(!submitted || !s || !submitted->provider || !submitted->command || !submitted->expected_object ||
       s->provider!=submitted->provider || s->command!=submitted->command ||
       s->call!=submitted->expected_object || s->owner_mm!=submitted->generation_after ||
       s->requested_fd!=submitted->expected_level || s->user_address!=submitted->generation_before ||
       s->address_length!=submitted->expected_option || s->original_count!=submitted->original_count ||
       !s->task || !s->task_start || !s->table || s->ready!=1 ||
       s->fdput_flags>1 || (!s->file && s->fdput_flags))return 0;
    u64 operation=submitted->operation;
    if(operation==AP_ORIGINAL_READ)return !submitted->expected_option;
    if(operation==AP_ORIGINAL_SENDTO_CALL)
        return ap_stream_tx_command(submitted) && s->file && !s->fdput_flags;
    /* Actual protocol-entry selection has no fdget flag/phase issuer. */
    if(ap_original_recv(operation))
        return ap_original_copy_disposition(operation,submitted->expected_option) &&
            submitted->original_count<=AP_READ_MAX_COUNT && s->file && !s->fdput_flags;
    if(operation==AP_ORIGINAL_OPENAT_CALL)return !s->fdput_flags;
    if(operation==AP_ORIGINAL_EPOLL_CALL)
        return ap_original_epoll_syscall(submitted->generation_before) &&
            !submitted->expected_option && !submitted->original_count && !s->fdput_flags;
    if(operation==AP_ORIGINAL_EPOLL_CTL)return submitted->original_count<=0xffffffffULL;
    /* Both counts must remain zero for all older scalar command envelopes. */
    if(submitted->original_count)return 0;
    if(operation==AP_ORIGINAL_SOCKET_CALL)
        return submitted->generation_before<=0xffffffffULL && !s->fdput_flags;
    /* Op10/23 use syscall/command slots, never user-address/count operands. */
    if(ap_original_file_operation(operation))
        return ap_original_file_shape(submitted->generation_before,submitted->expected_option);
    if(operation==AP_ORIGINAL_CLOSE)
        return !submitted->generation_before && !submitted->expected_option && !s->fdput_flags;
    return operation==AP_ORIGINAL_CONNECT;
}
/* UNKNOWN is an observed raw return without sufficient positive path evidence.
 * In particular ENOTSOCK is not a negative security-hook certificate. The
 * consumer must authenticate a retained non-socket pin, or refuse it. */
enum ap_original_path {
    AP_ORIGINAL_UNKNOWN=0, AP_ORIGINAL_EMPTY=1, AP_ORIGINAL_LENGTH=2,
    AP_ORIGINAL_COPY_FAULT=3, AP_ORIGINAL_AUDIT_DENIED=4, AP_ORIGINAL_SOCKET=5
};
static __attribute__((always_inline)) inline int ap_original_path(
    const struct ap_original_result *r) {
    if(!r->selection.file)return r->returned==-9?AP_ORIGINAL_EMPTY:AP_ORIGINAL_UNKNOWN;
    if(r->selection.address_length<0 || r->selection.address_length>128)
        return r->returned==-22?AP_ORIGINAL_LENGTH:AP_ORIGINAL_UNKNOWN;
    if(r->copy_returned && r->copy_remaining)
        return r->returned==-14?AP_ORIGINAL_COPY_FAULT:AP_ORIGINAL_UNKNOWN;
    if(r->audit_returned && r->audit_result)
        return r->returned==r->audit_result?AP_ORIGINAL_AUDIT_DENIED:AP_ORIGINAL_UNKNOWN;
    if(r->security_returned)return AP_ORIGINAL_SOCKET;
    return AP_ORIGINAL_UNKNOWN;
}
/* Exact Linux scalar-read result bound. This checks the actual return before
 * narrowing; it never clips count, invents EBADF, or replaces the syscall. */
#ifdef __BPF__
static __attribute__((noinline)) int
#else
static __attribute__((always_inline)) inline int
#endif
ap_original_socket_operands(
    const struct ap_task_command *c,s64 nr,s32 domain,s32 type,s32 protocol) {
    return c && c->operation==AP_ORIGINAL_SOCKET_CALL && !c->original_count &&
        c->generation_before<=0xffffffffULL && nr==AP_SOCKET_SYSCALL &&
        domain==c->expected_level && (u64)(u32)type==c->generation_before && protocol==c->expected_option;
}
struct ap_socket_entry_snapshot { u64 nr,domain,type,protocol; };
static __attribute__((always_inline)) inline int ap_socket_entry_copy(
    const struct ap_task_command *c,const void *nr,const void *domain,const void *type,const void *protocol,
    long (*read)(void *,u32,const void *),struct ap_socket_entry_snapshot *out) {
    if(!c || !nr || !domain || !type || !protocol || !read || !out)return 0;
    struct ap_socket_entry_snapshot observed={0};
    if(read(&observed.nr,sizeof(observed.nr),nr) ||
       read(&observed.domain,sizeof(observed.domain),domain) ||
       read(&observed.type,sizeof(observed.type),type) ||
       read(&observed.protocol,sizeof(observed.protocol),protocol) ||
       !ap_original_socket_operands(c,(s64)observed.nr,(s32)observed.domain,
           (s32)observed.type,(s32)observed.protocol))return 0;
    *out=observed;return 1;
}
/* Allocators share the original fd_install/exit receipt. Openat's pathname is
 * never copied or interpreted by the provider. Linux truncates dfd/flags to int;
 * their ignored high bits remain legal. The full mode register is retained even
 * though the installed syscall wrapper converts it to umode_t. No operand is
 * rewritten and no unused register is required to be zero. */
struct ap_allocator_entry_snapshot { u64 nr,arg0,arg1,arg2,arg3; };
#ifdef __BPF__
static __attribute__((noinline)) int
#else
static __attribute__((always_inline)) inline int
#endif
ap_original_allocator_operands_shared(const struct ap_task_command *c,
    const struct ap_allocator_entry_snapshot *p) {
    if(!p)return 0;
    const s64 nr=(s64)p->nr;
    const u64 arg0=p->arg0,arg1=p->arg1,arg2=p->arg2,arg3=p->arg3;
    if(c && c->operation==AP_ORIGINAL_EPOLL_CALL)
        return ap_original_epoll_syscall((u64)nr) && (u64)nr==c->generation_before &&
            (s32)arg0==c->expected_level && !c->expected_option && !c->original_count;
    if(c && c->operation==AP_ORIGINAL_OPENAT_CALL)
        return nr==AP_OPENAT_SYSCALL && (s32)arg0==c->expected_level &&
            arg1==c->generation_before && (s32)arg2==c->expected_option &&
            arg3==c->original_count;
    return ap_original_socket_operands(c,nr,(s32)arg0,(s32)arg1,(s32)arg2);

}
static __attribute__((always_inline)) inline int ap_original_allocator_operands(
    const struct ap_task_command *c,s64 nr,u64 arg0,u64 arg1,u64 arg2,u64 arg3) {
    const struct ap_allocator_entry_snapshot p={(u64)nr,arg0,arg1,arg2,arg3};
    return ap_original_allocator_operands_shared(c,&p);
}
static __attribute__((always_inline)) inline int ap_allocator_entry_copy(
    const struct ap_task_command *c,const void *nr,const void *arg0,const void *arg1,
    const void *arg2,const void *arg3,long (*read)(void *,u32,const void *),
    struct ap_allocator_entry_snapshot *out) {
    if(!c || !nr || !arg0 || !arg1 || !arg2 || !arg3 || !read || !out)return 0;
    struct ap_allocator_entry_snapshot observed={0};
    if(read(&observed.nr,sizeof(observed.nr),nr) ||
       read(&observed.arg0,sizeof(observed.arg0),arg0) ||
       (c->operation!=AP_ORIGINAL_EPOLL_CALL &&
        (read(&observed.arg1,sizeof(observed.arg1),arg1) ||
         read(&observed.arg2,sizeof(observed.arg2),arg2))) ||
       (c->operation==AP_ORIGINAL_OPENAT_CALL &&
        read(&observed.arg3,sizeof(observed.arg3),arg3)) ||
       !ap_original_allocator_operands(c,(s64)observed.nr,observed.arg0,
           observed.arg1,observed.arg2,observed.arg3))return 0;
    *out=observed;return 1;
}
static __attribute__((always_inline)) inline int ap_original_allocator_result_matches(
    const struct ap_task_command *c,const struct ap_command_result *r,const struct ap_original_result *o) {
    if(!c || !r || !o || !ap_original_allocator(c->operation) ||
       !ap_original_selection_matches(c,&o->selection) || r->operation!=c->operation ||
       r->command!=c->command || r->phase!=AP_COMMAND_DONE || r->reserved ||
       r->original_count!=c->original_count ||
       r->identity.provider!=c->provider || r->identity.object || r->identity.namespace ||
       r->creation || r->cookie || r->task!=o->selection.task ||
       r->start_boottime!=o->selection.task_start || r->returned!=o->returned ||
       o->returned< -4095 || o->complete!=1 || o->problem || o->reserved ||
       o->copy_entered || o->copy_returned || o->copy_remaining ||
       o->audit_entered || o->audit_returned || o->audit_result ||
       o->security_entered || o->security_returned || o->security_result)return 0;
    const u8 *state_bytes=(const u8 *)&r->state;
    for(u32 i=0;i<sizeof(r->state);++i)if(state_bytes[i])return 0;
    u32 used=c->operation==AP_ORIGINAL_EPOLL_CALL
        ? sizeof(struct ap_original_epoll_installation) : c->operation==AP_ORIGINAL_OPENAT_CALL
        ? sizeof(struct ap_original_openat_installation) : sizeof(struct ap_original_socket_installation);
    for(u32 i=0;i<sizeof(o->address);++i)if(i>=used && o->address[i])return 0;
    if(o->returned<0) {
        for(u32 i=0;i<sizeof(o->address);++i)if(o->address[i])return 0;
        return !o->selection.file;
    }
    if(c->operation==AP_ORIGINAL_OPENAT_CALL &&
       (!(o->opened.mode&0170000U) || o->opened.mode>0177777U))return 0;
    if(c->operation==AP_ORIGINAL_EPOLL_CALL &&
       ((c->generation_before==AP_EPOLL_CREATE_SYSCALL && c->expected_level<=0) ||
        (c->generation_before==AP_EPOLL_CREATE1_SYSCALL && ((u32)c->expected_level&~AP_EPOLL_CLOEXEC)) ||
        (o->epoll.descriptor_flags&~1U) || o->epoll.profiled!=1 || o->epoll.reserved ||
        (o->epoll.status_flags&AP_EPOLL_CLOEXEC)))return 0;
    return o->selection.file && o->installation.begin &&
        o->installation.end>o->installation.begin && !o->installation.reserved &&
        o->installation.fd==o->returned;
}
static __attribute__((always_inline)) inline int ap_original_socket_result_matches(
    const struct ap_task_command *c,const struct ap_command_result *r,const struct ap_original_result *o) {
    return c && c->operation==AP_ORIGINAL_SOCKET_CALL && ap_original_allocator_result_matches(c,r,o);
}
static __attribute__((always_inline)) inline int ap_original_allocator_native_return(
    const struct ap_task_command *c,const struct ap_command_result *r,const struct ap_fd_call *call,
    u64 task,u64 start,s64 nr,u64 arg0,u64 arg1,u64 arg2,u64 arg3,s64 returned) {
    if(!ap_original_allocator_operands(c,nr,arg0,arg1,arg2,arg3) || !r || !call ||
       !task || !start || returned< -4095 || returned>0x7fffffffLL ||
       r->phase!=AP_COMMAND_RUNNING || r->operation!=c->operation || r->command!=c->command ||
       r->original_count!=c->original_count || r->task!=task || r->start_boottime!=start || call->command!=c->command ||
       call->operation!=c->operation || !ap_original_selection_matches(c,&call->original.selection) ||
       call->original.selection.task!=task || call->original.selection.task_start!=start ||
       call->original.complete || call->original.problem)return 0;
    if(returned<0)return !call->original.selection.file && !call->install_begin && !call->new_file &&
        !call->original.installation.begin && !call->original.installation.end;
    return call->new_file && call->original.selection.file && call->install_begin &&
        call->original.installation.begin==call->install_begin &&
        call->original.installation.end>call->install_begin && call->original.installation.fd==returned;
}

static __attribute__((always_inline)) inline int ap_original_socket_native_return(
    const struct ap_task_command *c,const struct ap_command_result *r,const struct ap_fd_call *call,
    u64 task,u64 start,s64 nr,s32 domain,s32 type,s32 protocol,s64 returned) {
    return c && c->operation==AP_ORIGINAL_SOCKET_CALL &&
        ap_original_allocator_native_return(c,r,call,task,start,nr,
            (u64)(u32)domain,(u64)(u32)type,(u64)(u32)protocol,0,returned);
}

static __attribute__((always_inline)) inline int ap_original_read_return_value(
    u64 count,s64 returned) {
    return returned>=-4095 && returned<=(s64)AP_READ_MAX_COUNT &&
        (returned<0 || (u64)returned<=count);
}
static __attribute__((always_inline)) inline int ap_original_read_result_matches(
    const struct ap_task_command *submitted,const struct ap_command_result *result,
    const struct ap_original_result *original) {
    if(!submitted || !ap_original_receive(submitted->operation) || !result || !original ||
       !ap_original_selection_matches(submitted,&original->selection) ||
       result->command!=submitted->command || result->operation!=submitted->operation ||
       result->phase!=AP_COMMAND_DONE || result->identity.provider!=submitted->provider ||
       result->identity.object || result->identity.namespace || result->creation || result->cookie ||
       result->reserved || result->original_count!=submitted->original_count ||
       result->task!=original->selection.task || result->start_boottime!=original->selection.task_start ||
       result->returned!=original->returned ||
       !ap_original_read_return_value(submitted->original_count,original->returned) ||
       original->complete!=1 || original->problem || original->reserved ||
       original->copy_entered || original->copy_returned || original->copy_remaining ||
       original->audit_entered || original->audit_returned || original->audit_result ||
       original->security_entered || original->security_returned || original->security_result)return 0;
    for(u32 i=0;i<sizeof(original->address);i++)if(original->address[i])return 0;
    const unsigned char *raw=(const unsigned char *)&result->state;
    for(u32 i=0;i<sizeof(result->state);i++)if(raw[i])return 0;
    return original->selection.file || original->returned==-9;
}
/* F_GETFL may return positive flag bits. Keep the existing Connect/Close
 * nonpositive result predicates below byte-for-byte: no shared widening. */
static __attribute__((always_inline)) inline int ap_original_file_result_matches(
    const struct ap_task_command *submitted,const struct ap_command_result *result,
    const struct ap_original_result *original) {
    if(!submitted || !ap_original_file_operation(submitted->operation) || !result || !original ||
       result->original_count ||
       !ap_original_selection_matches(submitted,&original->selection) ||
       result->command!=submitted->command || result->operation!=submitted->operation ||
       result->phase!=AP_COMMAND_DONE || result->identity.provider!=submitted->provider ||
       result->task!=original->selection.task || result->start_boottime!=original->selection.task_start ||
       result->returned!=original->returned || original->returned< -4095 ||
       original->complete!=1 || original->problem || original->reserved ||
       original->copy_entered || original->copy_returned || original->copy_remaining ||
       original->audit_entered || original->audit_returned || original->audit_result ||
       original->security_entered || original->security_returned || original->security_result)return 0;
    for(u32 i=0;i<sizeof(original->address);i++)if(original->address[i])return 0;
    return original->selection.file || original->returned==-9;
}
static __attribute__((always_inline)) inline int ap_original_result_matches(
    const struct ap_task_command *submitted,const struct ap_command_result *result,
    const struct ap_original_result *original) {
    if(submitted && submitted->operation==AP_ORIGINAL_SENDTO_CALL) {
        if(!result || !original || !ap_original_selection_matches(submitted,&original->selection) ||
           result->command!=submitted->command || result->operation!=submitted->operation ||
           result->phase!=AP_COMMAND_DONE || result->identity.provider!=submitted->provider ||
           result->identity.object || result->identity.namespace || result->creation || result->cookie ||
           result->reserved ||
           result->task!=original->selection.task || result->start_boottime!=original->selection.task_start ||
           result->original_count!=submitted->original_count || result->returned!=original->returned ||
           original->complete!=1 || original->problem || original->reserved ||
           original->copy_entered || original->copy_returned || original->copy_remaining ||
           original->audit_entered || original->audit_returned || original->audit_result ||
           original->security_entered || original->security_returned || original->security_result ||
           !ap_stream_tx_summary_valid(&original->stream_tx.summary,original->selection.file,
               submitted->original_count,original->returned))return 0;
        for(u32 i=sizeof(struct ap_stream_tx_summary);i<sizeof(original->address);i++)
            if(original->address[i])return 0;
        const unsigned char *state=(const unsigned char *)&result->state;
        for(u32 i=0;i<sizeof(result->state);i++)if(state[i])return 0;
        return 1;
    }
    if(submitted && submitted->operation==AP_ORIGINAL_EPOLL_CTL)
        return ap_original_epoll_ctl_result_matches(submitted,result,original);
    if(submitted && ap_original_allocator(submitted->operation))
        return ap_original_allocator_result_matches(submitted,result,original);
    if(submitted && ap_original_receive(submitted->operation))
        return ap_original_read_result_matches(submitted,result,original);
    if(result && result->original_count)return 0;
    if(submitted && ap_original_file_operation(submitted->operation))
        return ap_original_file_result_matches(submitted,result,original);
    if(!result || !original || !ap_original_selection_matches(submitted,&original->selection) ||
       result->command!=submitted->command || result->operation!=submitted->operation ||
       result->phase!=AP_COMMAND_DONE || result->identity.provider!=submitted->provider ||
       result->task!=original->selection.task || result->start_boottime!=original->selection.task_start ||
       result->returned!=original->returned || original->returned>0 || original->returned< -4095 ||
       original->complete!=1 || original->problem || original->reserved ||
       original->copy_entered>1 || original->copy_returned!=original->copy_entered ||
       original->audit_entered>1 || original->audit_returned!=original->audit_entered ||
       original->security_entered>1 || original->security_returned!=original->security_entered)return 0;
    if(submitted->operation==AP_ORIGINAL_CLOSE) {
        /* Selection is the real file_close_fd return, before flush. Neither an
         * errno nor absence of a journal callback creates this observation. */
        if(original->copy_entered || original->copy_returned || original->copy_remaining ||
           original->audit_entered || original->audit_returned || original->audit_result ||
           original->security_entered || original->security_returned || original->security_result)
            return 0;
        for(u32 i=0;i<sizeof(original->address);i++)if(original->address[i])return 0;
        return original->selection.file || original->returned==-9;
    }
    int length=original->selection.address_length;
    if(!original->selection.file || length<0 || length>128) {
        if(original->copy_entered || original->audit_entered || original->security_entered)return 0;
        if(ap_original_path(original)==AP_ORIGINAL_UNKNOWN)return 0;
    }
    if(original->copy_returned) {
        if(length<=0 || length>128 || original->copy_remaining>(u64)length)return 0;
        if(original->copy_remaining && (original->returned!=-14 ||
           original->audit_entered || original->security_entered))return 0;
    } else if(original->copy_remaining)return 0;
    if(original->audit_returned) {
        if(!original->copy_returned || original->copy_remaining ||
           original->audit_result>0 || original->audit_result< -4095)return 0;
        if(original->audit_result && (original->returned!=original->audit_result ||
           original->security_entered))return 0;
    } else if(original->audit_result)return 0;
    if(original->security_returned) {
        if(!original->selection.file || length<0 || length>128 ||
           (length && (!original->copy_returned || original->copy_remaining)) ||
           original->security_result>0 || original->security_result< -4095 ||
           (original->security_result && original->returned!=original->security_result))return 0;
        for(u32 n=(u32)length;n<128;n++)if(original->address[n])return 0;
    } else {
        if(!original->returned || original->security_result)return 0;
        for(u32 n=0;n<128;n++)if(original->address[n])return 0;
    }
    return 1;
}
/* Common actual fdget-selected context only. Each return wrapper still
 * checks its exact syscall/operands, count, physical entry-stack polarity and
 * operation-specific return range. The running result is never published here. */
#ifdef __BPF__
static __attribute__((noinline)) int
#else
static __attribute__((always_inline)) inline int
#endif
ap_original_fdget_native_context(const struct ap_task_command *c,const struct ap_command_result *r,
    const struct ap_fd_call *call,u64 task,u64 start) {
    return c && r && call && call->operation==c->operation && call->command==c->command &&
        r->operation==c->operation && r->command==c->command && r->phase==AP_COMMAND_RUNNING &&
        r->task==task && r->start_boottime==start && r->identity.provider==c->provider &&
        ap_original_selection_matches(c,&call->original.selection) &&
        call->original.selection.task==task && call->original.selection.task_start==start &&
        ap_fd_selection_complete(&call->selection) &&
        call->selected_file==call->original.selection.file &&
        ((call->selection.word&~3ULL)!=0)==(call->original.selection.file!=0) &&
        (call->selection.word&1)==call->original.selection.fdput_flags &&
        !call->original.complete && !call->original.problem;
}
/* Exact actual sys_exit validation, shared with maintained host controls.
 * The raw value is checked before narrowing it into the unchanged s32 wire. */
static __attribute__((always_inline)) inline int ap_original_file_native_return(
    const struct ap_task_command *c,const struct ap_command_result *r,const struct ap_fd_call *call,
    u64 task,u64 start,s64 nr,s32 fd,s32 file_command,s64 returned) {
    return c && r && call && !r->original_count && ap_original_file_operation(c->operation) &&
        nr==AP_FILE_SYSCALL_FCNTL && fd==c->expected_level && file_command==c->expected_option &&
        ap_original_fdget_native_context(c,r,call,task,start) && !call->entry_stack &&
        returned>=-4095 && returned<=0x7fffffffLL &&
        (call->original.selection.file || returned==-9);
}
/* Same original Read owner and full raw operands at actual sys_exit. No
 * selection callback, even on EBADF, remains unknown rather than EMPTY. */
static __attribute__((always_inline)) inline int ap_original_read_native_return(
    const struct ap_task_command *c,const struct ap_command_result *r,const struct ap_fd_call *call,
    u64 task,u64 start,s64 nr,s32 fd,u64 buffer,u64 count,s64 returned) {
    return c && r && call && c->operation==AP_ORIGINAL_READ && nr==AP_READ_SYSCALL &&
        fd==c->expected_level && buffer==c->generation_before && count==c->original_count &&
        r->original_count==count && ap_original_fdget_native_context(c,r,call,task,start) &&
        call->entry_stack &&
        ap_original_read_return_value(count,returned) &&
        (call->original.selection.file || returned==-9);
}
/* Helpers are selected by the actual protocol socket reference, independently
 * of fdget_pos. These zero phases are meaningful and must never be forged. */
#ifdef __BPF__
static __attribute__((noinline)) int ap_original_recv_context(
#else
static inline int ap_original_recv_context(
#endif
const struct ap_task_command *c,
    const struct ap_command_result *r,const struct ap_fd_call *call,u64 task,u64 start) {
    return c && r && call && ap_original_recv(c->operation) &&
        call->operation==c->operation && call->command==c->command &&
        r->operation==c->operation && r->command==c->command && r->phase==AP_COMMAND_RUNNING &&
        r->original_count==c->original_count && r->task==task && r->start_boottime==start &&
        r->identity.provider==c->provider &&
        ap_original_selection_matches(c,&call->original.selection) &&
        call->original.selection.task==task && call->original.selection.task_start==start &&
        !call->selection.entered && !call->selection.returned &&
        call->selection.word && !(call->selection.word&3) && !call->entry_stack && !call->file_entry_ip &&
        call->selected_file==call->original.selection.file &&
        !call->original.complete && !call->original.problem;
}
/* The helper receive exit accepts only the packaged copy contract. The
 * producer stamps AP_NATIVE_COPY_VERSION; a fixed V4 literal refuses V5. */
static __attribute__((always_inline)) inline int ap_original_recv_copy_version(
    const struct ap_fd_call *call) {
    return call && call->original.stream_copy.summary.version==AP_NATIVE_COPY_VERSION;
}
/* BEGIN/END delimit callbacks, not a linearization point. A remove with the
 * same actual file can depend on an installation whose END is still pending.
 * A successful replacement names its incoming file and the actual old file
 * passed to filp_close, not a guessed pre-entry target-table snapshot. */
struct ap_fd_event {
    u64 sequence, kind, task, task_start, table, file, previous_file;
    u64 dependency, accept_command;
    s32 fd, returned;
    u64 complete;
    /* Present only on ENROLL_SLOT. These are the actual held file's inode mode
     * and f_flags observed before immutable completion, not host stdio guesses.
     * Other event kinds require both fields zero. Mode does not prove socket
     * domain, peer ownership, or the watch graph of an anonymous epoll object. */
    u32 mode, status_flags;
    /* Normalized kernel MAJOR/MINOR(i_rdev), not libc st_rdev encoding.
     * Both are zero for non-device inodes. */
    u32 device_major, device_minor;
    /* Actual immutable dispatch table observed on this held file at the
     * initial census. No numeric fd, inode kind or ioctl command issues it. */
    u64 source_ioctl_dispatch;
};
#define AP_SOURCE_IOCTL_DISPATCH_UNKNOWN 0ULL
#define AP_SOURCE_IOCTL_DISPATCH_NULL 1ULL
#define AP_SOURCE_IOCTL_DISPATCH_BTRFS 2ULL
static __attribute__((always_inline)) inline int ap_fd_source_ioctl_valid(
        u64 dispatch,u32 mode,u32 major,u32 minor) {
    switch(dispatch) {
    case AP_SOURCE_IOCTL_DISPATCH_UNKNOWN:return 1;
    case AP_SOURCE_IOCTL_DISPATCH_NULL:return (mode&0170000)==0020000 && major==1 && minor==3;
    case AP_SOURCE_IOCTL_DISPATCH_BTRFS:return (mode&0170000)==0100000 && !major && !minor;
    default:return 0;
    }
}
static __attribute__((always_inline)) inline int ap_fd_profile_valid(u32 mode) {
    switch(mode & 0170000) {
    case 0000000: case 0010000: case 0020000: case 0040000: case 0060000:
    case 0100000: case 0120000: case 0140000: return !(mode & ~0177777U);
    default: return 0;
    }
}
static __attribute__((always_inline)) inline int ap_fd_device_valid(u32 mode,u32 major,u32 minor) {
    if(major>0xfff || minor>0xfffff)return 0;
    return (mode & 0170000)==0020000 || (mode & 0170000)==0060000 || (!major && !minor);
}
/* The supported kernel's include/linux/kdev_t.h uses MINORBITS=20.
 * Do not use the legacy UAPI MKDEV or compare this with libc st_rdev. */
static __attribute__((always_inline)) inline u32 ap_fd_device_major(u32 kernel_dev) { return kernel_dev>>20; }
static __attribute__((always_inline)) inline u32 ap_fd_device_minor(u32 kernel_dev) { return kernel_dev&0xfffff; }
/* file_close_fd releases file_lock before its return callback. Its BEGIN and
 * actual-file REMOVE (or NO_FILE) bound an observation interval, not the
 * removal linearization point. The original under-lock REMOVE has dependency
 * zero. Consumers retain both interval endpoints and reconcile by actual file;
 * they cannot apply a late END as numeric-FD/current-slot deletion. */
static __attribute__((always_inline)) inline int ap_fd_remove_matches(
    const struct ap_fd_event *begin,const struct ap_fd_event *end) {
    if(!begin || !end || begin->kind!=AP_FD_REMOVE_BEGIN ||
       (end->kind!=AP_FD_REMOVE && end->kind!=AP_FD_REMOVE_NO_FILE) ||
       !begin->sequence || end->sequence<=begin->sequence ||
       end->dependency!=begin->sequence || begin->dependency ||
       begin->complete!=1 || end->complete!=1 ||
       !begin->table || end->table!=begin->table ||
       !begin->task || !begin->task_start || end->task!=begin->task ||
       end->task_start!=begin->task_start || end->fd!=begin->fd ||
       begin->file || begin->previous_file || end->previous_file ||
       begin->accept_command || end->accept_command ||
       begin->returned || end->returned)return 0;
    return end->kind==AP_FD_REMOVE ? end->file!=0 : end->file==0;
}
/* All linked table receipts bind a physical callback interval. They never
 * grant current slot authority by sequence order alone. Unknown/missing
 * callback results stay unresolved in the existing lifetime consumer. */
static __attribute__((always_inline)) inline int ap_fd_table_event(
    const struct ap_fd_event *e,u64 kind) {
    return e && e->kind==kind && e->sequence && e->complete==1 &&
        e->task && e->task_start && !e->previous_file && !e->accept_command;
}
static __attribute__((always_inline)) inline int ap_fd_same_table_call(
    const struct ap_fd_event *begin,const struct ap_fd_event *e) {
    return e->task==begin->task && e->task_start==begin->task_start &&
        e->sequence>begin->sequence;
}
/* Nonfinal RETURN uses only the captured generation; it deliberately never
 * reads the possibly freed/reused files pointer after the decrement. Final
 * RETURN must instead link the actual allocator receipt from this invocation.
 * Allocator receipt means all original close_files callbacks returned, not
 * that deferred __fput or any outside custody has completed. */
static __attribute__((always_inline)) inline int ap_fd_put_matches(
    const struct ap_fd_event *begin,const struct ap_fd_event *retired,
    const struct ap_fd_event *end) {
    if(!ap_fd_table_event(begin,AP_FD_TABLE_PUT_BEGIN) || !begin->table ||
       begin->fd!=-1 || begin->returned || begin->file || begin->dependency ||
       !ap_fd_table_event(end,AP_FD_TABLE_PUT_END) ||
       !ap_fd_same_table_call(begin,end) || end->table!=begin->table ||
       end->fd!=-1 || end->file)return 0;
    if(!retired)return end->returned==0 && end->dependency==begin->sequence;
    return ap_fd_table_event(retired,AP_FD_TABLE_RETIRED) &&
        ap_fd_same_table_call(begin,retired) && retired->table==begin->table &&
        retired->fd==-1 && !retired->file && !retired->returned &&
        retired->dependency==begin->sequence && retired->sequence<end->sequence &&
        end->returned==1 && end->dependency==retired->sequence;
}
/* COPY_SLOT has actual new-table FD/file and actual close-on-exec bit in
 * returned. COPY_END has the full max_fds population in fd and nonnull count
 * in returned. Zero entries are covered by END's successful complete scan,
 * never by an omitted/truncated population. Failed dup_fd has table0/fd-1 and
 * the actual ERR_PTR result, with no slot receipts. */
static __attribute__((always_inline)) inline int ap_fd_copy_matches(
    const struct ap_fd_event *begin,const struct ap_fd_event *slots,u32 count,
    const struct ap_fd_event *end) {
    if(!ap_fd_table_event(begin,AP_FD_COPY_BEGIN) || !begin->table ||
       begin->fd!=-1 || begin->returned || begin->file || begin->dependency ||
       !ap_fd_table_event(end,AP_FD_COPY_END) || !ap_fd_same_table_call(begin,end) ||
       end->file || end->dependency!=begin->sequence || count>AP_FD_FILES)return 0;
    if(end->returned<0)return end->returned>=-4095 && !end->table && end->fd==-1 && !count;
    if(!end->table || end->table==begin->table || end->fd<=0 ||
       (u32)end->fd>AP_FD_FILES || end->fd%64 || end->returned!=(s32)count ||
       (count && !slots))return 0;
    for(u32 i=0;i<count;i++) {
        const struct ap_fd_event *e=&slots[i];
        if(!ap_fd_table_event(e,AP_FD_COPY_SLOT) || !ap_fd_same_table_call(begin,e) ||
           e->sequence>=end->sequence || e->table!=end->table ||
           e->dependency!=begin->sequence || !e->file || e->fd<0 || e->fd>=end->fd ||
           (e->returned!=0 && e->returned!=1) ||
           (i && (e->fd<=slots[i-1].fd || e->sequence<=slots[i-1].sequence)))return 0;
    }
    return 1;
}
/* EXEC_REMOVE is an actual held-file/direct-call-site observation. Its fd is
 * decoded from the exact reviewed kernel register contract, not inferred from
 * file identity: multiple aliases of the same file remain distinct. */
static __attribute__((always_inline)) inline int ap_fd_exec_matches(
    const struct ap_fd_event *begin,const struct ap_fd_event *removed,u32 count,
    const struct ap_fd_event *end) {
    if(!ap_fd_table_event(begin,AP_FD_EXEC_BEGIN) || !begin->table ||
       begin->fd!=-1 || begin->returned || begin->file || begin->dependency ||
       !ap_fd_table_event(end,AP_FD_EXEC_END) || !ap_fd_same_table_call(begin,end) ||
       end->table!=begin->table || end->file || end->fd!=-1 ||
       end->dependency!=begin->sequence || end->returned!=(s32)count ||
       count>AP_FD_JOURNAL || (count && !removed))return 0;
    for(u32 i=0;i<count;i++) {
        const struct ap_fd_event *e=&removed[i];
        if(!ap_fd_table_event(e,AP_FD_EXEC_REMOVE) || !ap_fd_same_table_call(begin,e) ||
           e->sequence>=end->sequence || e->table!=begin->table ||
           e->dependency!=begin->sequence || !e->file || e->fd<0 || e->returned ||
           (i && (e->fd<=removed[i-1].fd || e->sequence<=removed[i-1].sequence)))return 0;
    }
    return 1;
}
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
int ap_cancel_uninvoked_original(struct ap_session *,int,u64);
int ap_cancel_uninvoked_birth(struct ap_session *,int,u64);
int ap_prepare_native_birth(struct ap_session *,int creator_pidfd,u64 call,u64 mm,u64 table,int syscall,u64 *command);
/* Early read does not collect/disarm/ACK the creator command. It carries no
 * child admission by itself. A survivor separately requires its held pidfd;
 * terminal consumption separately requires the backend's actual final wait. */
int ap_read_native_birth(struct ap_session *,u64 command,struct ap_native_birth *);
int ap_admit_native_birth_child(struct ap_session *,int child_pidfd,u64 command,
                                struct ap_native_birth *);
/* Trusted backend terminal consumption, not a TASK_STORAGE-absence claim. */
int ap_admit_native_birth_terminal(struct ap_session *,u64 command,struct ap_native_birth *);
int ap_collect_native_birth(struct ap_session *,int creator_pidfd,u64 command,
                            struct ap_command_result *,struct ap_native_birth *);
int ap_retire_dead_original(struct ap_session *,int,u64,struct ap_original_terminal *);
int ap_retire_dead_birth(struct ap_session *,int,u64,struct ap_native_birth_terminal *);
int ap_prepare_original_close(struct ap_session *,int exact_pidfd,u64 call,u64 mm,
                              int fd,u64 *command);
int ap_prepare_auxiliary_file(struct ap_session *,int exact_worker_pidfd,u64 call,u64 mm,
                              int fd,u64 *command);
int ap_prepare_original_recvfrom(struct ap_session *,int,u64,u64,int,u64,u64,int,u64 *);
int ap_prepare_original_recvmsg(struct ap_session *,int,u64,u64,int,u64,u64,int,u64 *);
int ap_prepare_original_read(struct ap_session *,int exact_pidfd,u64 call,u64 mm,
                             int fd,u64 buffer,u64 count,u64 *command);
int ap_prepare_original_openat(struct ap_session *,int,u64,u64,int,u64,int,u64,u64 *);
int ap_prepare_original_file(struct ap_session *,int exact_pidfd,u64 call,u64 mm,
                             int fd,int syscall_nr,int file_command,u64 *command);
int ap_prepare_original_connect(struct ap_session *, int exact_task_pidfd,
                                u64 call, u64 mm, int fd, u64 address, int length,
                                u64 *command);
int ap_read_original_selection(struct ap_session *, int exact_task_pidfd,
                               u64 command, struct ap_original_selection *);
int ap_collect_original_connect(struct ap_session *, int exact_task_pidfd,
                                u64 command, struct ap_command_result *,
                                struct ap_original_result *);
int ap_read_fd_status(struct ap_session *, struct ap_fd_status *);
int ap_read_fd_event(struct ap_session *, u64 sequence, struct ap_fd_event *);
/* ACK uses exact immutable bytes and cannot retire an incomplete callback.
 * It only retires physical evidence already retained by the engine; it does
 * not close a file, authorize an installed slot, or retire a semantic owner. */
int ap_ack_fd_event(struct ap_session *, const struct ap_fd_event *);
#endif
#include "fd-enrollment.h"
#endif
