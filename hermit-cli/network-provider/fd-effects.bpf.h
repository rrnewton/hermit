/* SPDX-License-Identifier: GPL-2.0 */
/* Included by the maintained provider after its identity/command helpers.
 * This source is not selected until its complete composed artifact is built,
 * checked against running BTF, and the actual installation consumer is joined. */
#include "fd-effects.h"
#include "connect-copy.h"
#include "fdupfd.h"

#include "stream-frontier.h"
struct ap_fd_table { u64 identity,enrolled; };
struct ap_fd_put_call { u64 table, begin, retired; };
struct ap_fd_copy_call { u64 table, begin, birth_command; };
struct ap_fd_exec_call { u64 raw_table, table, begin, function_ip, removed; };
struct ap_fd_replace {
    u64 table, file, previous_file, begin;
    u32 fd, old_seen;
};
struct ap_fd_remove_call { u64 raw_table, table, begin; u32 fd; };
struct ap_fd_install_call { u64 table, file, raw_file, begin, accept_command; u32 fd; struct ap_fdupfd fdup; };
ARRAY(fd_accepts,struct ap_fd_accept,AP_COMMANDS);
ARRAY(fd_enrollments,struct ap_fd_enrollment,AP_COMMANDS);
ARRAY(fd_status,struct ap_fd_status,1);
HASH(fd_journal,u64,struct ap_fd_event,AP_FD_JOURNAL);
HASH(fd_files,u64,struct ap_fd_file,AP_FD_FILES);
HASH(fd_tables,u64,struct ap_fd_table,AP_FD_TABLES);
HASH(fd_calls,struct ap_invocation_key,struct ap_fd_call,AP_CALLS);
HASH(fd_replacements,struct ap_invocation_key,struct ap_fd_replace,AP_CALLS);
HASH(fd_install_calls,struct ap_invocation_key,struct ap_fd_install_call,AP_CALLS);
HASH(fd_removals,struct ap_invocation_key,struct ap_fd_remove_call,AP_CALLS);
HASH(fd_puts,struct ap_invocation_key,struct ap_fd_put_call,AP_CALLS);
HASH(fd_copies,struct ap_invocation_key,struct ap_fd_copy_call,AP_CALLS);
HASH(fd_execs,struct ap_invocation_key,struct ap_fd_exec_call,AP_CALLS);
static long (*fd_read_kernel)(void *,u32,const void *)=(void *)BPF_FUNC_probe_read_kernel;
static u64 (*fd_function_ip)(void *)=(void *)BPF_FUNC_get_func_ip;
static struct pt_regs *(*fd_task_regs)(struct task_struct *)=(void *)BPF_FUNC_task_pt_regs;
static __attribute__((noinline)) void stream_copy_commit(struct ap_fd_call *,s64);
#ifdef AP_FTRACE_PROVIDER
static __attribute__((noinline)) int stream_tx_enter(struct pt_regs *);
static __attribute__((noinline)) int stream_tx_exit(struct pt_regs *);
static __attribute__((noinline)) int stream_tx_lock(struct pt_regs *,int);
static __attribute__((noinline)) int stream_tx_unlock(struct pt_regs *,int);
#endif
static __attribute__((noinline)) int stream_tx_syscall_exit(u64 *,struct ap_task_command *);

static __attribute__((noinline)) struct ap_fd_status *fd_stats(void) {
    u32 zero=0;return lookup(&fd_status,&zero);
}
static __attribute__((noinline)) void fd_problem(u64 problem) {
    struct ap_fd_status *s=fd_stats();
    if(s)__sync_fetch_and_or(&s->problem,problem);
}
INLINE struct ap_invocation_key fd_actor(void) { return invocation(0,0); }
static __attribute__((noinline)) u64 fd_table(struct files_struct *files,int create) {
    u64 key=(u64)files;struct ap_fd_table *known=lookup(&fd_tables,&key);
    if(known) {
        int access=ap_fd_table_access(known->enrolled,(u32)create);
        if(!access)return 0;
        if(access==2 && __sync_val_compare_and_swap(&known->enrolled,0,1)!=0) {
            fd_problem(AP_FD_DUPLICATE);return 0;
        }
        return known->identity;
    }
    if(create==AP_TABLE_ENROLLED_LOOKUP || create==AP_TABLE_ANY_LOOKUP ||
       create<AP_TABLE_ENROLLED_LOOKUP || create>AP_TABLE_CENSUS_ENROLL)return 0;
    struct ap_fd_status *s=fd_stats();if(!s)return 0;
    u64 identity=__sync_fetch_and_add(&s->next_table,1)+1;
    if(!identity) { fd_problem(AP_FD_CAPACITY);return 0; }
    struct ap_fd_table fresh={.identity=identity,.enrolled=create!=AP_TABLE_NORMALIZE};
    if(update(&fd_tables,&key,&fresh,BPF_NOEXIST)) {
        known=lookup(&fd_tables,&key);
        if(!known)fd_problem(AP_FD_CAPACITY);
        if(!known || ap_fd_table_access(known->enrolled,(u32)create)!=1)return 0;
        return known->identity;
    }
    return identity;
}
/* Every caller has an actual borrowed kernel file reference here. The mapping
 * is retired at the audited __fput entry or at allocator entry when the kernel
 * inlines final release; it does not own a reference. Both are before reuse.
 * fd_install fexit MUST NOT call this: its argument reference was consumed. */
static __attribute__((noinline)) u64 fd_file(struct file *file) {
    if(!file)return 0;
    u64 key=(u64)file;struct ap_fd_file *known=lookup(&fd_files,&key);
    if(known)return known->identity;
    struct ap_fd_status *s=fd_stats();if(!s)return 0;
    u64 identity=__sync_fetch_and_add(&s->next_file,1)+1;
    if(!identity) { fd_problem(AP_FD_CAPACITY);return 0; }
    struct ap_fd_file fresh={.identity=identity};
    if(update(&fd_files,&key,&fresh,BPF_NOEXIST)) {
        known=lookup(&fd_files,&key);
        if(!known)fd_problem(AP_FD_CAPACITY);
        return known?known->identity:0;
    }
    return identity;
}
/* SO_COOKIE's actual held socket supplies this file pointer. Lookup only:
 * neither an unknown file nor a reused numeric guest slot gets a generation.
 * The consumer joins this observation to its original installation receipt.
 * The held FD keeps socket->file alive for this entire getter invocation. */
static __attribute__((noinline)) int observe_socket_file(
    struct sock *sk,const struct ap_task_command *c,struct ap_command_result *r) {
    if(!ap_socket_file_observation_command(c)) {
        ap_fail(AP_BAD_COMMAND);return 0;
    }
    struct socket *socket=CORE(sk->sk_socket);
    struct file *file=socket?CORE(socket->file):0;
    if(!file) { ap_fail(AP_WRONG_IDENTITY);return 0; }
    u64 pointer=(u64)file;
    struct ap_fd_file *known=lookup(&fd_files,&pointer);
    r->identity=identity(sk,known?known->identity:0);
    r->cookie=CORE(sk->__sk_common.skc_cookie.counter);
    if(!r->cookie || !r->identity.namespace) { ap_fail(AP_WRONG_IDENTITY);return 0; }
    u16 family=CORE(sk->__sk_common.skc_family);
    if((family==AP_AF_INET || family==AP_AF_INET6) && CORE(sk->sk_protocol)==AP_IPPROTO_TCP &&
       raw_tcp_state(sk,&r->state))return 0;
    publish_result(r);return 0;
}

#include "fd-journal.bpf.h"
#include "fd-call-shared.bpf.h"

static __attribute__((noinline)) struct ap_fd_accept *fd_accept(struct ap_fd_call *call) {
    if(!call || call->operation!=AP_ACCEPT_EFFECT)return 0;
    u32 slot=ap_command_slot(call->command);
    struct ap_fd_accept *a=lookup(&fd_accepts,&slot);
    return a && a->command==call->command?a:0;
}

/* Keep the mutually exclusive original-entry initializers in separate BPF
 * frames. Each owns one 424-byte call row; inlining them into the session
 * dispatcher also merges their spills with the audit/security path. */
static __attribute__((noinline)) int fd_accept_enter(struct pt_regs *ctx,u64 entry_cookie,
        struct ap_task_command *c) {
    u32 slot=ap_command_slot(c->command);
    struct ap_fd_accept *a=lookup(&fd_accepts,&slot);
    if(!a || a->command!=c->command || a->phases) { fd_problem(AP_FD_DUPLICATE);return 0; }
    a->task=pid_tgid();a->task_start=CORE(current_task()->start_boottime);
    a->table=fd_table(CORE(current_task()->files),1);
    a->requested_fd=(s32)CORE(ctx->di);a->flags=(s32)CORE(ctx->cx);
    a->phases=AP_FD_ENTERED;
    if(a->requested_fd!=c->expected_level || a->flags!=c->expected_option || !a->table)
        a->problem|=AP_FD_IDENTITY;
    {
    struct ap_invocation_key key=fd_actor();
    struct ap_fd_call call;ap_fd_call_clear(&call);
    call.command=c->command;
    call.raw_table=(u64)CORE(current_task()->files);
    call.function_ip=fd_function_ip(ctx);
    call.operation=AP_ACCEPT_EFFECT;
    __asm__ __volatile__("" : : "r"(&call) : "memory");
    if(!ap_fdget_entry(&call,c,a,key.task,key.start,call.raw_table,a->table,
          entry_cookie,CORE(ctx->sp)))a->problem|=AP_FD_IDENTITY;
    if(update(&fd_calls,&key,&call,BPF_NOEXIST))fd_problem(AP_FD_DUPLICATE);
    }
    return 1;
}

/* Original syscall entries share the existing session program/link with
 * audit/security. Distinct classic post-call probes observe actual fdget RAX
 * before either original syscall can inspect it or begin user-memory copy. */
extern bool bpf_session_is_return(void *ctx) __attribute__((section(".ksyms")));
extern u64 *bpf_session_cookie(void *ctx) __attribute__((section(".ksyms")));
#ifdef AP_GROUPED_PROVIDER
#include "grouped-probes.bpf.h"
#else
static u64 (*fd_attach_cookie)(void *)=(void *)BPF_FUNC_get_attach_cookie;
#endif
/* Keep the final entry validator's private physical snapshot out of the
 * caller's 424-byte unpublished Call frame. The key and both table values
 * are the same already-initialized operands; no map row is exposed here.
 * Publication remains the caller's subsequent BPF_NOEXIST update. */
static __attribute__((noinline)) void fd_connect_validate_entry(
        struct pt_regs *ctx,const struct ap_task_command *c,struct ap_fd_call *fresh,
        const struct ap_invocation_key *key,u64 entry_cookie) {
    if(!ap_fdget_entry(fresh,c,0,key->task,key->start,fresh->raw_table,
          fresh->original.selection.table,entry_cookie,CORE(ctx->sp)))
        fresh->original.problem|=AP_FD_IDENTITY;
}
static __attribute__((noinline)) int fd_connect_enter(struct pt_regs *ctx,u64 entry_cookie,
        struct ap_task_command *c) {
    {
    struct ap_invocation_key key=fd_actor();
    struct files_struct *files=CORE(current_task()->files);
    u64 table=fd_table(files,1),function_ip=fd_function_ip(ctx);
    u64 provider=incarnation();
    /* Compute helper results before the large zeroed row is live, and fill
     * its fields directly: no second selection-sized stack temporary. */
    struct ap_fd_call fresh;ap_fd_call_clear(&fresh);
    fresh.command=c->command;
    fresh.raw_table=(u64)files;
    fresh.function_ip=function_ip;
    fresh.operation=AP_ORIGINAL_CONNECT;
    fresh.original.selection.command=c->command;
    fresh.original.selection.call=c->expected_object;
    fresh.original.selection.owner_mm=c->generation_after;
    fresh.original.selection.provider=provider;
    fresh.original.selection.task=key.task;
    fresh.original.selection.task_start=key.start;
    fresh.original.selection.table=table;
    fresh.original.selection.user_address=CORE(ctx->si);
    fresh.original.selection.requested_fd=(s32)CORE(ctx->di);
    fresh.original.selection.address_length=(s32)CORE(ctx->dx);
    if(!fresh.function_ip || !fresh.original.selection.table || !c->expected_object ||
       fresh.original.selection.requested_fd!=c->expected_level ||
       fresh.original.selection.address_length!=c->expected_option || CORE(ctx->si)!=c->generation_before)
        fresh.original.problem=AP_FD_IDENTITY;
    /* End duplicate scalar lifetimes before the shared validator. This empty
     * compiler barrier materializes the initialized row; it is not a CPU
     * ordering primitive and changes none of the validator predicates. */
    __asm__ __volatile__("" : : "r"(&fresh) : "memory");
    fd_connect_validate_entry(ctx,c,&fresh,&key,entry_cookie);
    if(update(&fd_calls,&key,&fresh,BPF_NOEXIST))fd_problem(AP_FD_DUPLICATE);
    }
    return 1;
}
/* The caller owns the full424-byte private row and its checked operands.
 * Keep the result claim, identity helpers and complete validation in a small
 * callee frame. The row is still unpublished until the final BPF_NOEXIST;
 * no task/map scratch or second callback can observe partial initialization. */
static __attribute__((noinline)) int fd_original_selection_publish_physical(
    const struct ap_task_command *c,struct ap_fd_call *fresh,
    u64 entry_ip,u64 entry_stack,u64 entry_cookie) {
    struct ap_command_result *r=claim_result(c);if(!r)return 0;
    struct ap_invocation_key key=fd_actor();
    struct files_struct *files=CORE(current_task()->files);
    u64 table=fd_table(files,(c->operation==AP_AUXILIARY_FILE || ap_original_recv(c->operation) ||
        ap_stream_tx_operation(c->operation))?AP_TABLE_NORMALIZE:AP_TABLE_ENROLLED_CREATE),provider=incarnation();
    fresh->command=c->command;fresh->raw_table=(u64)files;
    fresh->file_entry_ip=entry_ip;
    fresh->original.selection.command=c->command;fresh->original.selection.call=c->expected_object;
    fresh->original.selection.owner_mm=c->generation_after;fresh->original.selection.provider=provider;
    fresh->original.selection.task=key.task;fresh->original.selection.task_start=key.start;
    fresh->original.selection.table=table;
    __asm__ __volatile__("" : : "r"(fresh), "r"(&key) : "memory");
    if(ap_original_allocator(fresh->operation) || ap_original_recv(fresh->operation) ||
       ap_stream_tx_operation(fresh->operation)) {
        if(ap_original_recv(fresh->operation) || ap_stream_tx_operation(fresh->operation))
            fresh->new_file=(u64)CORE(current_task()->mm);
        if(!provider || provider!=c->provider || !table || !key.task || !key.start ||
           fresh->original.selection.requested_fd!=c->expected_level ||
           fresh->original.selection.user_address!=c->generation_before ||
           fresh->original.selection.address_length!=c->expected_option ||
           fresh->original.selection.original_count!=c->original_count ||
           (c->operation==AP_ORIGINAL_SOCKET_CALL && c->original_count))
            fresh->original.problem|=AP_FD_IDENTITY;
    } else if(ap_original_file_operation(fresh->operation)) {
        if(!ap_file_selection_enter(fresh,c,key.task,key.start,fresh->raw_table,table,
             entry_cookie,entry_stack))fresh->original.problem|=AP_FD_IDENTITY;
    } else if(entry_cookie) {
        if(!ap_read_selection_enter(fresh,c,key.task,key.start,fresh->raw_table,table,
             entry_cookie,entry_stack))fresh->original.problem|=AP_FD_IDENTITY;
    } else if(!ap_read_selection_context(fresh,c,key.task,key.start,fresh->raw_table,table)) {
        /* fentry/__x64_sys_read publishes the exact operands and wrapper
         * identity. The paired fdget_pos session owns selection entry/return. */
        fresh->original.problem|=AP_FD_IDENTITY;
    }
    if(update(&fd_calls,&key,fresh,BPF_NOEXIST))fd_problem(AP_FD_DUPLICATE);
    r->identity.provider=provider;
    return 0;
}
static __attribute__((noinline)) int fd_original_selection_publish(
    struct pt_regs *ctx,const struct ap_task_command *c,struct ap_fd_call *fresh) {
    /* Allocator claims have no kprobe context. Keep every actual context read
     * under its direct nonnull guard, independently of the operation stored in
     * the private row; a nullable scalar is never a pt_regs memory authority. */
    u64 entry_ip=0,entry_stack=0,entry_cookie=0;
    if(ctx) {
        if(ap_original_allocator(fresh->operation) || ap_original_recv(fresh->operation)) {
            fd_problem(AP_FD_IDENTITY);return 0;
        }
        entry_ip=CORE(ctx->ip);entry_stack=CORE(ctx->sp);entry_cookie=fd_attach_cookie(ctx);
#ifdef AP_FTRACE_PROVIDER
        if(entry_cookie==AP_FILE_FDGET_COOKIE) {
            entry_cookie=AP_FILE_ENTRY_COOKIE;
            /* Distinguish this true function-entry session from the retained
             * +5 classic transition probe. ap_file_selection_enter only needs
             * a nonzero physical entry after the exact caller was validated. */
            entry_ip=fd_function_ip(ctx);
        }
#endif
    } else if(!ap_original_allocator(fresh->operation) && !ap_original_recv(fresh->operation) &&
              !ap_stream_tx_operation(fresh->operation)) {
        fd_problem(AP_FD_IDENTITY);return 0;
    }
    return fd_original_selection_publish_physical(
        c,fresh,entry_ip,entry_stack,entry_cookie);
}
#ifndef AP_FTRACE_PROVIDER
static __attribute__((noinline)) int fd_original_file_pre(struct pt_regs *ctx) {
    struct ap_task_command *c=command();if(!c || !ap_original_file_operation(c->operation))return 0;
    int caller=ap_file_lookup_caller(CORE(ctx->ip),CORE(ctx->sp),0,fd_read_kernel);
    if(caller<=0) {if(caller<0)fd_problem(AP_FD_MISSING);return 0;}
    /* The bound caller loads EBP and R12D before its original fdget_raw call.
     * Compare full zero-extended registers, not just plausible low bits. */
    if(CORE(ctx->bp)!=(u64)(u32)c->expected_level ||
       CORE(ctx->r12)!=(u64)(u32)c->expected_option) {fd_problem(AP_FD_IDENTITY);return 0;}
    struct ap_fd_call fresh;ap_fd_call_clear(&fresh);
    {
    struct pt_regs *regs=fd_task_regs(current_task());
    struct ap_file_entry_snapshot operands={0};
    if(!regs || !ap_file_read_entry(c,CORE(ctx->di),CORE(&regs->orig_ax),
          CORE(&regs->di),CORE(&regs->si),fd_read_kernel,&operands)) {
        fd_problem(AP_FD_IDENTITY);return 0;
    }
    fresh.original.selection.requested_fd=operands.fd;
    fresh.original.selection.user_address=operands.original_nr;
    fresh.original.selection.address_length=operands.file_command;
    }
    /* Materialize the actual validated operands in their final row fields
     * before the claim/actor/table helpers become live. This compiler-only
     * boundary ends snapshot temporaries; it does not publish the row. */
    __asm__ __volatile__("" : : "r"(&fresh) : "memory");
    fresh.operation=c->operation;
    return fd_original_selection_publish(ctx,c,&fresh);
}
#endif

/* Operand/profile temporaries use separate frames from the fixed Call row.
 * The actual fd_install entry still owns the file reference for both reads. */
static __attribute__((noinline)) int fd_original_allocator_operands(
    const struct ap_task_command *c,struct ap_original_selection *selection) {
    struct pt_regs *regs=fd_task_regs(current_task());
    struct ap_allocator_entry_snapshot operands={0};
    if(!regs || !ap_allocator_entry_copy(c,CORE(&regs->orig_ax),CORE(&regs->di),
          CORE(&regs->si),CORE(&regs->dx),CORE(&regs->r10),fd_read_kernel,&operands))return 0;
    selection->requested_fd=(s32)operands.arg0;
    selection->user_address=c->operation==AP_ORIGINAL_EPOLL_CALL
        ? operands.nr : c->operation==AP_ORIGINAL_SOCKET_CALL
        ? (u64)(u32)operands.arg1 : operands.arg1;
    selection->address_length=c->operation==AP_ORIGINAL_EPOLL_CALL ? 0 : (s32)operands.arg2;
    selection->original_count=c->operation==AP_ORIGINAL_OPENAT_CALL?operands.arg3:0;
    return 1;
}
static __attribute__((noinline)) int fd_original_openat_profile(
    struct file *file,struct ap_original_openat_installation *opened) {
    struct inode *inode=0;unsigned short mode=0;u32 flags=0,device=0;
    if(fd_read_kernel(&inode,sizeof(inode),CORE(&file->f_inode)) || !inode ||
       fd_read_kernel(&mode,sizeof(mode),CORE(&inode->i_mode)) ||
       fd_read_kernel(&flags,sizeof(flags),CORE(&file->f_flags)) ||
       fd_read_kernel(&device,sizeof(device),CORE(&inode->i_rdev)) || !(mode&0170000U))return 0;
    opened->mode=mode;opened->status_flags=flags;
    opened->device_major=ap_fd_device_major(device);
    opened->device_minor=ap_fd_device_minor(device);
    return 1;
}
static __attribute__((noinline)) int fd_original_epoll_profile(
    struct file *file,s32 fd,struct ap_original_epoll_installation *epoll) {
    struct files_struct *files=CORE(current_task()->files);
    struct fdtable *fdt=files ? CORE(files->fdt) : 0;
    u32 slots=fdt ? CORE(fdt->max_fds) : 0;
    unsigned long *cloexec=fdt ? CORE(fdt->close_on_exec) : 0;
    unsigned long bits=0;u32 flags=0;
    if(fd<0 || (u32)fd>=slots || !cloexec ||
       fd_read_kernel(&flags,sizeof(flags),CORE(&file->f_flags)) ||
       fd_read_kernel(&bits,sizeof(bits),cloexec+(u32)fd/64))return 0;
    epoll->status_flags=flags;
    epoll->descriptor_flags=(bits>>((u32)fd%64))&1;
    epoll->profiled=1;
    return 1;
}
/* Allocators use the existing original command and fd_calls row. No new probe,
 * FD duplicate, registry, user copy or synthetic syscall invocation is added. */
static __attribute__((noinline)) int fd_original_allocator_claim(
    const struct ap_task_command *c,struct file *file,u64 file_id,s32 fd) {
    struct ap_fd_call fresh;ap_fd_call_clear(&fresh);
    if(!fd_original_allocator_operands(c,&fresh.original.selection)) {
        fd_problem(AP_FD_IDENTITY);return 0;
    }
    __asm__ __volatile__("" : : "r"(&fresh) : "memory");
    fresh.operation=c->operation;fresh.new_file=(u64)file;
    fresh.original.selection.file=file_id;
    if(file) {
        fresh.original.installation.fd=fd;
        if(c->operation==AP_ORIGINAL_EPOLL_CALL &&
           !fd_original_epoll_profile(file,fd,&fresh.original.epoll)) {
            fd_problem(AP_FD_IDENTITY);return 0;
        } else if(c->operation==AP_ORIGINAL_OPENAT_CALL &&
           !fd_original_openat_profile(file,&fresh.original.opened)) {
            fd_problem(AP_FD_IDENTITY);return 0;
        }
    } else fresh.original.selection.ready=1;
    return fd_original_selection_publish(0,c,&fresh);
}
/* AUTONOMOUS-BOT-IMPLEMENTED: native helper45/47 use the existing actual
 * sys_enter and Call row, then protocol socket selection (no fdget fiction).
 * TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/3174). */
static __attribute__((noinline)) int fd_original_recv_entered(
    const struct ap_task_command *c) {
    struct ap_fd_call fresh;ap_fd_call_clear(&fresh);
    fresh.operation=c->operation;
    fresh.original.selection.requested_fd=c->expected_level;
    fresh.original.selection.user_address=c->generation_before;
    fresh.original.selection.address_length=c->expected_option;
    fresh.original.selection.original_count=c->original_count;
    return fd_original_selection_publish(0,c,&fresh);
}
static __attribute__((noinline)) int fd_original_recv_syscall_entered(
    u64 *ctx,const struct ap_task_command *c) {
    const s64 nr=(s64)ctx[1];
    if(nr!=(c->operation==AP_ORIGINAL_RECVFROM_CALL?AP_RECVFROM_SYSCALL:AP_RECVMSG_SYSCALL))return 0;
    struct pt_regs *regs=(struct pt_regs *)ctx[0];
    if(!regs || CORE(regs->cs)!=0x33 || (s64)CORE(regs->orig_ax)!=nr ||
       !ap_original_recv_operands(c,nr,CORE(regs->di),CORE(regs->si),CORE(regs->dx),
           CORE(regs->r10),CORE(regs->r8),CORE(regs->r9))) {
        fd_problem(AP_FD_IDENTITY);return 0;
    }
    return fd_original_recv_entered(c);
}
static __attribute__((noinline)) int fd_original_sendto_syscall_entered(
        u64 *ctx,const struct ap_task_command *c) {
    if((s64)ctx[1]!=AP_SENDTO_SYSCALL)return 0;
    struct pt_regs *regs=(struct pt_regs *)ctx[0];
    if(!regs || CORE(regs->cs)!=0x33 || CORE(regs->orig_ax)!=AP_SENDTO_SYSCALL ||
       !ap_stream_tx_any_operands(c,ctx[1],CORE(regs->di),CORE(regs->si),CORE(regs->dx),
           CORE(regs->r10),CORE(regs->r8),CORE(regs->r9))) {
        fd_problem(AP_FD_IDENTITY);return 0;
    }
    return fd_original_recv_entered(c); /* same zeroed original Call initializer */
}
#ifndef AP_FTRACE_PROVIDER
static __attribute__((noinline)) int fd_original_read_pre(struct pt_regs *ctx) {
    struct ap_task_command *c=command();if(!c || c->operation!=AP_ORIGINAL_READ)return 0;
    struct ap_fd_call fresh;ap_fd_call_clear(&fresh);
    {
    struct pt_regs *regs=(struct pt_regs *)CORE(ctx->di);
    struct ap_read_entry_snapshot operands={0};
    if(!regs || !ap_read_entry_copy(c,CORE(&regs->orig_ax),CORE(&regs->di),
          CORE(&regs->si),CORE(&regs->dx),fd_read_kernel,&operands)) {
        fd_problem(AP_FD_IDENTITY);return 0;
    }
    fresh.original.selection.requested_fd=operands.fd;
    fresh.original.selection.user_address=operands.buffer;
    fresh.original.selection.original_count=operands.count;
    }
    /* Same lifetime separation as the compiled predecessor candidate. Actual
     * ABI7 combined stack/compiler/verifier proof remains mandatory. */
    __asm__ __volatile__("" : : "r"(&fresh) : "memory");
    fresh.operation=AP_ORIGINAL_READ;
    return fd_original_selection_publish(ctx,c,&fresh);
}
#else
SEC("fentry/__x64_sys_read") int fd_original_read_entered(u64 *ctx) {
    struct ap_task_command *c=command();if(!c || c->operation!=AP_ORIGINAL_READ)return 0;
    struct pt_regs *regs=(struct pt_regs *)ctx[0];struct ap_read_entry_snapshot operands={0};
    if(!regs || !ap_read_entry_copy(c,CORE(&regs->orig_ax),CORE(&regs->di),
          CORE(&regs->si),CORE(&regs->dx),fd_read_kernel,&operands)) {
        fd_problem(AP_FD_IDENTITY);return 0;
    }
    struct ap_fd_call fresh;ap_fd_call_clear(&fresh);
    fresh.operation=AP_ORIGINAL_READ;
    fresh.original.selection.requested_fd=operands.fd;
    fresh.original.selection.user_address=operands.buffer;
    fresh.original.selection.original_count=operands.count;
    const u64 entry_ip=fd_function_ip(ctx);
    if(!entry_ip) {fd_problem(AP_FD_IDENTITY);return 0;}
    return fd_original_selection_publish_physical(c,&fresh,entry_ip,0,0);
}
#endif

INLINE int fd_connect_problem(struct ap_fd_call *call,u64 problem,int entry) {
    call->original.problem|=problem;
    return entry?1:0;
}
static __attribute__((noinline)) int fd_connect_copy_success(
        struct ap_fd_call *call,const struct ap_task_command *owner,
        const struct ap_invocation_key *key,u64 callee_stack,u64 destination) {
    const struct ap_original_selection *selected=&call->original.selection;
    if(selected->address_length<=0)return selected->address_length==0 &&
        !call->copied_address && !call->original.copy_entered &&
        !call->original.copy_returned && !call->original.copy_remaining;
    if(call->original.copy_returned)return call->original.copy_entered==1 &&
        !call->original.copy_remaining && call->copied_address==destination;
    return ap_original_copy_infer_success(call,owner,key->task,key->start,
        callee_stack,destination,call->selection.word,
        (u32)selected->address_length,selected->user_address);
}
/* Own this path's lookup key here, not in the dispatcher that calls the
 * large entry frames. The kernel still checks the combined BPF call stack. */
static __attribute__((noinline)) int fd_connect_session(struct pt_regs *ctx) {
    struct ap_task_command *c=command();
    struct ap_invocation_key key;struct ap_fd_call *call=fd_actor_call(&key);
    if(!call || call->operation!=AP_ORIGINAL_CONNECT)return 1;
    bool returning=bpf_session_is_return(ctx);
    if(!c || c->operation!=AP_ORIGINAL_CONNECT || call->command!=c->command)
        return fd_connect_problem(call,AP_FD_MISSING,!returning);
    if(call->original.complete)return 1;
    u64 kind=fd_attach_cookie(ctx),*cookie=bpf_session_cookie(ctx);
    if(!cookie)return fd_connect_problem(call,AP_FD_MISSING,!returning);
    struct ap_original_selection *selected=&call->original.selection;
    if(!returning) {
        u64 return_ip=0;
        if(fd_read_kernel(&return_ip,sizeof(return_ip),(const void *)CORE(ctx->sp)))
            return fd_connect_problem(call,AP_FD_MISSING,1);
        if(kind==AP_CONNECT_AUDIT_COOKIE) {
            if(!ap_connect_audit_site(call->function_ip,return_ip))return 1;
            if(!fd_connect_copy_success(call,c,&key,CORE(ctx->sp),CORE(ctx->si)) ||
               (s32)CORE(ctx->di)!=selected->address_length ||
               CORE(ctx->si)!=call->copied_address ||
               call->original.audit_entered || call->original.audit_returned)
                return fd_connect_problem(call,AP_FD_IDENTITY,1);
            call->original.audit_entered=1;
        } else if(kind==AP_CONNECT_SECURITY_COOKIE) {
            if(!ap_connect_security_site(call->function_ip,return_ip))return 1;
            struct socket *socket=(struct socket *)CORE(ctx->di);
            struct file *file=0;
            // Kprobe register arguments are scalar addresses, not verifier
            // trusted BTF pointers. Read through the bounded kernel helper.
            if(fd_read_kernel(&file,sizeof(file),CORE(&socket->file)))
                return fd_connect_problem(call,AP_FD_MISSING,1);
            if(selected->ready!=1 || !ap_fd_selection_complete(&call->selection) ||
               (call->selection.word&~3ULL)!=(u64)file || !selected->file ||
               fd_file(file)!=selected->file || (s32)CORE(ctx->dx)!=selected->address_length ||
               selected->address_length<0 || selected->address_length>128 ||
               (selected->address_length && (!fd_connect_copy_success(call,c,&key,
                 CORE(ctx->sp),CORE(ctx->si)) || CORE(ctx->si)!=call->copied_address)) ||
               call->original.audit_result || call->original.security_entered || call->original.security_returned)
                return fd_connect_problem(call,AP_FD_IDENTITY,1);
            call->security_socket=(u64)socket;
            call->security_address=CORE(ctx->si);
            call->original.security_entered=1;
        } else return fd_connect_problem(call,AP_FD_IDENTITY,1);
        *cookie=call->command;return 0;
    }
    if(!*cookie || *cookie!=call->command)
        return fd_connect_problem(call,AP_FD_IDENTITY,0);
    if(kind==AP_CONNECT_AUDIT_COOKIE) {
        if(call->original.audit_entered!=1 || call->original.audit_returned)
            return fd_connect_problem(call,AP_FD_IDENTITY,0);
        call->original.audit_result=(s32)CORE(ctx->ax);call->original.audit_returned=1;
    } else if(kind==AP_CONNECT_SECURITY_COOKIE) {
        if(call->original.security_entered!=1 || call->original.security_returned ||
           !call->security_socket || selected->address_length<0 || selected->address_length>128)
            return fd_connect_problem(call,AP_FD_IDENTITY,0);
        u32 length=(u32)selected->address_length;
        if(length && fd_read_kernel(call->original.address,length,(const void *)call->security_address))
            return fd_connect_problem(call,AP_FD_MISSING,0);
        call->original.security_result=(s32)CORE(ctx->ax);
        call->original.security_returned=1;
    } else return fd_connect_problem(call,AP_FD_IDENTITY,0);
    return 0;
}
/* The maintained driver forces real PERF_EVENT BPF links at +0x41/+0x46.
 * get_func_ip deliberately returns0 for non-entry classic probes; authority
 * is the inspected exact link offset/cookie plus this retained invocation. */
#ifndef AP_FTRACE_PROVIDER
static __attribute__((noinline)) int fd_connect_copy_boundary(struct pt_regs *ctx,int after) {
    struct ap_task_command *c=command();
    if(!c || c->operation!=AP_ORIGINAL_CONNECT)return 0;
    struct ap_invocation_key key;struct ap_fd_call *call=fd_actor_call(&key);
    struct ap_command_result *r=result(c->command);
    if(!call || !r || r->phase!=AP_COMMAND_RUNNING ||
       fd_attach_cookie(ctx)!=(after?AP_CONNECT_COPY_AFTER_COOKIE:AP_CONNECT_COPY_BEFORE_COOKIE)) {
        fd_problem(AP_FD_MISSING);return 0;
    }
    int ok=after?
        ap_original_copy_end(call,c,key.task,key.start,CORE(ctx->sp),CORE(ctx->ax),
            CORE(ctx->bx),(u32)CORE(ctx->bp),CORE(ctx->r14)):
        ap_original_copy_begin(call,c,key.task,key.start,CORE(ctx->sp),CORE(ctx->di),CORE(ctx->si),
            CORE(ctx->dx),CORE(ctx->bx),(u32)CORE(ctx->bp),CORE(ctx->r14));
    if(!ok)call->original.problem|=AP_FD_IDENTITY;
    return 0;
}
#endif
SEC("fexit/__sys_connect") int fd_connect_returned(u64 *ctx) {
    struct ap_task_command *c=command();
    if(!c || c->operation!=AP_ORIGINAL_CONNECT)return 0;
    struct ap_invocation_key key;struct ap_fd_call *call=fd_actor_call(&key);
    struct ap_command_result *r=result(c->command);
    if(!call || !r || call->command!=c->command || call->operation!=AP_ORIGINAL_CONNECT ||
       r->phase!=AP_COMMAND_RUNNING) { fd_problem(AP_FD_MISSING);return 0; }
    if(!ap_original_selection_matches(c,&call->original.selection) ||
       !ap_fd_selection_complete(&call->selection) || call->original.complete ||
       call->original.security_entered!=call->original.security_returned ||
       (s32)ctx[0]!=c->expected_level || ctx[1]!=c->generation_before ||
       (s32)ctx[2]!=c->expected_option)
        call->original.problem|=AP_FD_IDENTITY;
    r->returned=(s32)ctx[3];call->original.returned=r->returned;
    call->original.complete=1;
    // The bounded command ACK deletes this exact immutable row. Original
    // selection must remain recoverable even if its guest future is canceled.
    publish_result(r);return 0;
}
_Static_assert(__builtin_offsetof(struct pt_regs,ax)==10*8,"reviewed x86_64 AX slot");
_Static_assert(__builtin_offsetof(struct pt_regs,sp)==19*8,"reviewed x86_64 SP slot");
/* For the two original entries, return1 skips only this session's exit
 * callback. Their existing independent fexit programs still own completion.
 * Audit/security retain the original per-invocation entry/return cookie. */
/* The existing session program/link also observes the original f_dupfd
 * interval. No additional map, program, link, file reference or task command.
 * An alloc_fd callback outside this retained original is not an FD fact. */
static __attribute__((noinline)) int fd_fdupfd_session(struct pt_regs *ctx,u64 kind) {
    struct ap_invocation_key key=fd_actor();
    struct files_struct *files=CORE(current_task()->files);
    u64 table=fd_table(files,0),*cookie=bpf_session_cookie(ctx);
    bool returning=bpf_session_is_return(ctx);
    if(!table) {
        if(returning)fd_problem(AP_FD_UNKNOWN_TABLE);
        return !returning;
    }
    if(!cookie) {fd_problem(AP_FD_MISSING);return !returning;}
    if(kind==AP_FDUPFD_COOKIE && !returning) {
        /* fcntl's fdget_raw borrow (or counted reference for shared tables)
         * remains held through do_fcntl/f_dupfd. fd_file only uses its scalar
         * address under that actual kernel custody. */
        struct file *file=(struct file *)CORE(ctx->si);
        struct ap_fd_install_call fresh={.table=table,.file=fd_file(file),
            .raw_file=(u64)file,.fdup={.function_ip=fd_function_ip(ctx),
                .entry_stack=CORE(ctx->sp),.raw_table=(u64)files,.minimum=(u32)CORE(ctx->di),
                .flags=(u32)CORE(ctx->dx)}};
        if(!fresh.file || !fresh.raw_file || !fresh.fdup.function_ip || !fresh.fdup.entry_stack) {
            fd_problem(AP_FD_IDENTITY);return 1;
        }
        if(update(&fd_install_calls,&key,&fresh,BPF_NOEXIST)) {fd_problem(AP_FD_DUPLICATE);return 1;}
        *cookie=fresh.fdup.entry_stack;return 0;
    }
    struct ap_fd_install_call *install=lookup(&fd_install_calls,&key);
    if(!install || !install->fdup.entry_stack) {
        /* An unrelated alloc_fd is normal; a registered return or original
         * f_dupfd completion without its retained row is never a no-effect. */
        if(returning || kind==AP_FDUPFD_COOKIE)fd_problem(AP_FD_MISSING);
        return !returning;
    }
    if(install->table!=table || install->fdup.raw_table!=(u64)files) {
        fd_problem(AP_FD_IDENTITY);return !returning;
    }
    if(kind==AP_FDUPFD_ALLOC_COOKIE) {
        if(!returning) {
            u64 return_ip=0;
            if(fd_read_kernel(&return_ip,sizeof(return_ip),(const void *)CORE(ctx->sp)) ||
               !ap_fdupfd_allocation_enter(&install->fdup,return_ip,CORE(ctx->sp),(u64)files,
                   (u32)CORE(ctx->di),(u32)CORE(ctx->dx))) {
                fd_problem(AP_FD_IDENTITY);return 1;
            }
            *cookie=install->fdup.allocation_stack;return 0;
        }
        s32 returned=(s32)CORE(ctx->ax);
        if(!ap_fdupfd_allocation_return(&install->fdup,*cookie,(u64)files,returned)) {
            fd_problem(AP_FD_IDENTITY);return 0;
        }
        if(returned>=0) {
            install->fd=(u32)returned;
            install->begin=fd_event(AP_FD_INSTALL_BEGIN,table,returned,install->file,0,0,0,0);
            if(!install->begin)fd_problem(AP_FD_MISSING);
        }
        return 0;
    }
    if(kind!=AP_FDUPFD_COOKIE || !returning) {fd_problem(AP_FD_IDENTITY);return !returning;}
    int complete=ap_fdupfd_completion(&install->fdup,*cookie,(u64)files,(s32)CORE(ctx->ax),install->begin);
    if(complete<0) {fd_problem(AP_FD_OUTCOME);return 0;}
    /* The exact kernel paths install before this return. A concurrent Remove
     * may be between BEGIN and END: retain that interval, never read the now
     * reusable slot, dereference the consumed pointer, or reorder its events. */
    if(complete && !fd_event(AP_FD_INSTALL_END,table,install->fd,install->file,0,install->begin,0,0))
        {fd_problem(AP_FD_MISSING);return 0;}
    if(remove_key(&fd_install_calls,&key))fd_problem(AP_FD_MISSING);
    return 0;
}
#include "epoll-ctl-copy.bpf.h"
#ifndef AP_FTRACE_PROVIDER
static __attribute__((noinline)) int fd_shared_fdget_session(struct pt_regs *ctx) {
    const bool returning=bpf_session_is_return(ctx);
    struct ap_task_command *c=command();
    struct ap_invocation_key key;struct ap_fd_call *call=fd_actor_call(&key);
    u64 *session=bpf_session_cookie(ctx);
    if(!c || !call || !session)return !returning;
    struct files_struct *files=CORE(current_task()->files);u64 table=fd_table(files,0);
    struct ap_fd_accept *accept=call->operation==AP_ACCEPT_EFFECT?fd_accept(call):0;
    struct ap_command_result *r=result(c->command);
    if(!returning) {
        u64 return_ip=0,stack=CORE(ctx->sp);
        if(!stack || fd_read_kernel(&return_ip,sizeof(return_ip),(const void *)stack)) {
            fd_problem(AP_FD_MISSING);return 1;
        }
        const u64 role=ap_shared_fdget_role(call->operation,call->function_ip,return_ip);
        if(!role)return 1; /* unrelated fdget in the same task */
        int valid=0;
        if(call->operation==AP_ORIGINAL_EPOLL_CTL)
            valid=fd_original_epoll_context(c,call,r,&key) &&
                ap_original_epoll_fdget_role_ready(call,c,role,0);
        else valid=r && r->phase==AP_COMMAND_RUNNING &&
            ap_fdget_context(call,c,accept,key.task,key.start,(u64)files,table) &&
            call->selection.entered==1 && !call->selection.returned && !call->selection.word;
        if(!valid) {fd_problem(AP_FD_IDENTITY);return 1;}
        *session=role;return 0;
    }
    const u64 role=*session,word=CORE(ctx->ax);
    if(!role) {fd_problem(AP_FD_IDENTITY);return 0;}
    if(call->operation==AP_ORIGINAL_EPOLL_CTL)
        return fd_original_epoll_apply_post(call,c,role,word);
    if(!r || r->phase!=AP_COMMAND_RUNNING ||
       !ap_fdget_session_post(call,c,accept,key.task,key.start,(u64)files,table,role,word)) {
        if(accept)accept->problem|=AP_FD_IDENTITY;else call->original.problem|=AP_FD_IDENTITY;
        return 0;
    }
    fd_selected_file(call,accept,call->operation==AP_ORIGINAL_CONNECT);return 0;
}
#endif
#ifdef AP_FTRACE_PROVIDER
/* fdget can re-enter before a kprobe session returns on this image. Preserve
 * the same exact caller-PC and task-command admission with independent
 * KPROBE_MULTI entry/return programs; the existing per-call row carries one
 * live role, and the positive selection transition remains mandatory. */
static __attribute__((noinline)) int fd_shared_fdget_enter(struct pt_regs *ctx) {
    struct ap_task_command *c=command();struct ap_invocation_key key;
    struct ap_fd_call *call=fd_actor_call(&key);
    if(!c || !call)return 0;
    struct files_struct *files=CORE(current_task()->files);u64 table=fd_table(files,0);
    struct ap_fd_accept *accept=call->operation==AP_ACCEPT_EFFECT?fd_accept(call):0;
    struct ap_command_result *r=result(c->command);u64 return_ip=0,stack=CORE(ctx->sp);
    if(!stack || fd_read_kernel(&return_ip,sizeof(return_ip),(const void *)stack)) {
        fd_problem(AP_FD_MISSING);return 0;
    }
    const u64 role=ap_shared_fdget_role(call->operation,call->function_ip,return_ip);
    if(!role)return 0;
    int valid=0;
    if(call->operation==AP_ORIGINAL_EPOLL_CTL)
        valid=fd_original_epoll_context(c,call,r,&key) &&
            !call->fdget_role && ap_original_epoll_fdget_role_ready(call,c,role,0);
    else valid=r && r->phase==AP_COMMAND_RUNNING &&
        ap_fdget_context(call,c,accept,key.task,key.start,(u64)files,table) &&
        call->selection.entered==1 && !call->selection.returned &&
        !call->selection.word && !call->fdget_role;
    if(!valid) {fd_problem(AP_FD_IDENTITY);return 0;}
    call->fdget_role=role;return 0;
}
static __attribute__((noinline)) int fd_shared_fdget_exit(struct pt_regs *ctx) {
    struct ap_task_command *c=command();struct ap_invocation_key key;
    struct ap_fd_call *call=fd_actor_call(&key);
    if(!c || !call || !call->fdget_role)return 0;
    struct files_struct *files=CORE(current_task()->files);u64 table=fd_table(files,0);
    struct ap_fd_accept *accept=call->operation==AP_ACCEPT_EFFECT?fd_accept(call):0;
    struct ap_command_result *r=result(c->command);
    const u64 role=call->fdget_role,word=CORE(ctx->ax);call->fdget_role=0;
    if(call->operation==AP_ORIGINAL_EPOLL_CTL)
        return fd_original_epoll_apply_post(call,c,role,word);
    if(!r || r->phase!=AP_COMMAND_RUNNING ||
       !ap_fdget_session_post(call,c,accept,key.task,key.start,(u64)files,table,role,word)) {
        if(accept)accept->problem|=AP_FD_IDENTITY;else call->original.problem|=AP_FD_IDENTITY;
        return 0;
    }
    fd_selected_file(call,accept,call->operation==AP_ORIGINAL_CONNECT);return 0;
}
#endif
static __attribute__((noinline)) int fd_file_fdget_session(struct pt_regs *ctx) {
    const bool returning=bpf_session_is_return(ctx);u64 *session=bpf_session_cookie(ctx);
    struct ap_task_command *c=command();
    if(!session || !c || !ap_original_file_operation(c->operation))return !returning;
    if(!returning) {
        u64 return_ip=0,stack=CORE(ctx->sp),function_ip=fd_function_ip(ctx);
        if(!stack || fd_read_kernel(&return_ip,sizeof(return_ip),(const void *)stack)) {
            fd_problem(AP_FD_MISSING);return 1;
        }
        if(!ap_file_fdget_caller_site(function_ip,return_ip))return 1;
        if(!ap_file_fdget_entry_registers(CORE(ctx->bp),CORE(ctx->r12),
              c->expected_level,c->expected_option)) {fd_problem(AP_FD_IDENTITY);return 1;}
        struct pt_regs *regs=fd_task_regs(current_task());struct ap_file_entry_snapshot operands={0};
        if(!regs || !ap_file_read_entry(c,CORE(ctx->di),CORE(&regs->orig_ax),
              CORE(&regs->di),CORE(&regs->si),fd_read_kernel,&operands)) {
            fd_problem(AP_FD_IDENTITY);return 1;
        }
        struct ap_fd_call fresh;ap_fd_call_clear(&fresh);
        fresh.operation=c->operation;
        fresh.original.selection.requested_fd=operands.fd;
        fresh.original.selection.user_address=operands.original_nr;
        fresh.original.selection.address_length=operands.file_command;
        fd_original_selection_publish(ctx,c,&fresh);
        struct ap_invocation_key key;struct ap_fd_call *call=fd_actor_call(&key);
        if(!call || call->operation!=c->operation || call->command!=c->command ||
           call->file_entry_ip!=function_ip || call->selection.entered!=1 ||
           call->selection.returned || call->selection.word || call->original.problem) {
            if(call)call->original.problem|=AP_FD_IDENTITY;else fd_problem(AP_FD_MISSING);
            return 1;
        }
        *session=c->command;return 0;
    }
    struct ap_invocation_key key;struct ap_fd_call *call=fd_actor_call(&key);
    struct ap_command_result *r=result(c->command);struct files_struct *files=CORE(current_task()->files);
    u64 table=fd_table(files,c->operation==AP_AUXILIARY_FILE?AP_TABLE_ANY_LOOKUP:AP_TABLE_ENROLLED_LOOKUP);
    if(*session!=c->command || !r || r->phase!=AP_COMMAND_RUNNING ||
       r->operation!=c->operation || r->task!=key.task || r->start_boottime!=key.start ||
       !ap_file_selection_session_post(call,c,key.task,key.start,(u64)files,table,CORE(ctx->ax))) {
        if(call)call->original.problem|=AP_FD_IDENTITY;else fd_problem(AP_FD_MISSING);return 0;
    }
    fd_selected_file(call,0,1);return 0;
}
static __attribute__((noinline)) int fd_read_fdget_session(struct pt_regs *ctx) {
    const bool returning=bpf_session_is_return(ctx);u64 *session=bpf_session_cookie(ctx);
    struct ap_task_command *c=command();
    if(!session || !c || c->operation!=AP_ORIGINAL_READ)return !returning;
    struct ap_invocation_key key;struct ap_fd_call *call=fd_actor_call(&key);
    if(!returning) {
        u64 return_ip=0,stack=CORE(ctx->sp),function_ip=fd_function_ip(ctx);
        if(!stack || fd_read_kernel(&return_ip,sizeof(return_ip),(const void *)stack)) {
            fd_problem(AP_FD_MISSING);return 1;
        }
        if(!ap_read_fdget_caller_site(function_ip,return_ip))return 1;
        /* During transition a row created by the old wrapper+0x13 site remains
         * owned by its old fdget_pos sites. Only a true fentry row can claim
         * this paired session. */
        if(!call || !ap_read_fentry_fdget_site(call->file_entry_ip,return_ip))return 1;
        struct ap_command_result *r=result(c->command);
        struct files_struct *files=CORE(current_task()->files);u64 table=fd_table(files,0);
        if(CORE(ctx->di)!=(u64)(u32)c->expected_level || !r ||
           r->phase!=AP_COMMAND_RUNNING || r->operation!=AP_ORIGINAL_READ ||
           r->original_count!=c->original_count || r->task!=key.task ||
           r->start_boottime!=key.start || r->identity.provider!=c->provider ||
           !ap_read_selection_enter(call,c,key.task,key.start,(u64)files,table,
               AP_READ_ENTRY_COOKIE,stack)) {
            call->original.problem|=AP_FD_IDENTITY;return 1;
        }
        *session=c->command;return 0;
    }
    struct ap_command_result *r=result(c->command);
    struct files_struct *files=CORE(current_task()->files);u64 table=fd_table(files,0);
    if(!call || *session!=c->command || !r || r->phase!=AP_COMMAND_RUNNING ||
       r->operation!=AP_ORIGINAL_READ || r->original_count!=c->original_count ||
       r->task!=key.task || r->start_boottime!=key.start ||
       r->identity.provider!=c->provider ||
       !ap_read_selection_session_post(call,c,key.task,key.start,(u64)files,table,CORE(ctx->ax))) {
        if(call)call->original.problem|=AP_FD_IDENTITY;else fd_problem(AP_FD_MISSING);return 0;
    }
    fd_selected_file(call,0,1);return 0;
}
#ifndef AP_FTRACE_PROVIDER
static __attribute__((always_inline,nodebug)) inline int fd_session_dispatch(struct pt_regs *ctx) {
    u64 kind=fd_attach_cookie(ctx);
#ifdef AP_GROUPED_PROVIDER
    if(fd_grouped_bootstrap(ctx,kind))return 1;
#endif
    if(kind==AP_EPOLL_CTL_COOKIE)return fd_epoll_ctl_session(ctx);
#ifndef AP_FTRACE_PROVIDER
    if(kind==AP_SHARED_FDGET_COOKIE)return fd_shared_fdget_session(ctx);
#endif
    if(kind==AP_FILE_FDGET_COOKIE)return fd_file_fdget_session(ctx);
    if(kind==AP_READ_FDGET_COOKIE)return fd_read_fdget_session(ctx);
    if(kind==AP_FDUPFD_COOKIE || kind==AP_FDUPFD_ALLOC_COOKIE)return fd_fdupfd_session(ctx,kind);
    if(kind==AP_ACCEPT_ENTRY_COOKIE || kind==AP_CONNECT_ENTRY_COOKIE) {
        if(bpf_session_is_return(ctx)) {fd_problem(AP_FD_IDENTITY);return 1;}
        struct ap_task_command *c=command();
        u64 operation=kind==AP_ACCEPT_ENTRY_COOKIE?AP_ACCEPT_EFFECT:AP_ORIGINAL_CONNECT;
        if(!c || c->operation!=operation)return 1;
        struct ap_command_result *r=claim_result(c);if(!r)return 1;
        int published=kind==AP_ACCEPT_ENTRY_COOKIE?
            fd_accept_enter(ctx,kind,c):fd_connect_enter(ctx,kind,c);
        if(published)r->identity.provider=incarnation();
        return 1;
    }
    return fd_connect_session(ctx);
}
#endif
#ifdef AP_FTRACE_PROVIDER
/* Outer functions and their nested callees need independent recursion state.
 * The five inner sites do not nest each other on the admitted image, so one
 * KPROBE_MULTI session retains their exact cookies with one program/link. */
#include "fd-session-dispatch.inc"
SEC("kprobe.multi") __attribute__((nodebug)) int fd_s20e(struct pt_regs *ctx) {
    if(fd_attach_cookie_raw(ctx)==AP_STREAM_TX_COOKIE)return stream_tx_enter(ctx);
    if(fd_attach_cookie_raw(ctx)==AP_STREAM_TX_LOCK_COOKIE)return stream_tx_lock(ctx,0);
    if(fd_attach_cookie_raw(ctx)==AP_STREAM_TX_UNLOCK_COOKIE)return stream_tx_unlock(ctx,0);
    return fd_shared_fdget_enter(ctx);
}
SEC("kprobe.multi") __attribute__((nodebug)) int fd_s20x(struct pt_regs *ctx) {
    if(fd_attach_cookie_raw(ctx)==AP_STREAM_TX_COOKIE)return stream_tx_exit(ctx);
    if(fd_attach_cookie_raw(ctx)==AP_STREAM_TX_LOCK_COOKIE)return stream_tx_lock(ctx,1);
    if(fd_attach_cookie_raw(ctx)==AP_STREAM_TX_UNLOCK_COOKIE)return stream_tx_unlock(ctx,1);
    return fd_shared_fdget_exit(ctx);
}
#else
SEC("kprobe.session") __attribute__((nodebug)) int fd_accept_selected(struct pt_regs *ctx) {
    return fd_session_dispatch(ctx);
}
#endif
#ifndef AP_FTRACE_PROVIDER
static __attribute__((noinline)) int fd_original_post_fdget(struct pt_regs *ctx,u64 operation) {
    struct ap_task_command *c=command();if(!c || c->operation!=operation)return 0;
    struct ap_invocation_key key;struct ap_fd_call *call=fd_actor_call(&key);
    struct ap_command_result *r=result(c->command);
    struct ap_fd_accept *a=operation==AP_ACCEPT_EFFECT?fd_accept(call):0;
    if(!call || !r || r->phase!=AP_COMMAND_RUNNING ||
       (operation==AP_ACCEPT_EFFECT && !a)) {fd_problem(AP_FD_MISSING);return 0;}
    struct files_struct *files=CORE(current_task()->files);
    const u64 table=fd_table(files,0);
    /* RDI is caller-saved by fdget. Reconstruct the entry frame through the
     * original body's saved trampoline RBP; fexit gives this body a distinct
     * physical stack from the session's outer entry. Actual RAX and the
     * callee-saved option/address still come from this exact post-call stop. */
    const u64 stack=ap_fdget_post_stack(operation,CORE(ctx->sp),fd_read_kernel);
    if(!ap_fdget_post(call,c,a,key.task,key.start,(u64)files,table,fd_attach_cookie(ctx),
          stack,operation==AP_ACCEPT_EFFECT?CORE(ctx->r15):CORE(ctx->bp),
          operation==AP_ACCEPT_EFFECT?0:CORE(ctx->r14),CORE(ctx->ax))) {
        if(a)a->problem|=AP_FD_IDENTITY;else call->original.problem|=AP_FD_IDENTITY;
        return 0;
    }
    fd_selected_file(call,a,operation==AP_ORIGINAL_CONNECT);
    return 0;
}
static __attribute__((noinline)) int fd_original_file_post(struct pt_regs *ctx) {
    struct ap_task_command *c=command();if(!c || !ap_original_file_operation(c->operation))return 0;
    int caller=ap_file_lookup_caller(CORE(ctx->ip),CORE(ctx->sp),1,fd_read_kernel);
    if(caller<=0) {if(caller<0)fd_problem(AP_FD_MISSING);return 0;}
    struct ap_invocation_key key;struct ap_fd_call *call=fd_actor_call(&key);
    struct ap_command_result *r=result(c->command);
    if(!call || !r || r->phase!=AP_COMMAND_RUNNING || r->operation!=c->operation ||
       r->task!=key.task || r->start_boottime!=key.start) {fd_problem(AP_FD_MISSING);return 0;}
    struct files_struct *files=CORE(current_task()->files);u64 table=fd_table(files,c->operation==AP_AUXILIARY_FILE?AP_TABLE_ANY_LOOKUP:AP_TABLE_ENROLLED_LOOKUP);
    if(!ap_file_selection_post(call,c,key.task,key.start,(u64)files,table,fd_attach_cookie(ctx),
         CORE(ctx->sp),CORE(ctx->bp),CORE(ctx->r12),CORE(ctx->ax),CORE(ctx->ip))) {
        call->original.problem|=AP_FD_IDENTITY;return 0;
    }
    fd_selected_file(call,0,1);
    return 0;
}
static __attribute__((noinline)) int fd_original_read_selected(struct pt_regs *ctx) {
    struct ap_task_command *c=command();if(!c || c->operation!=AP_ORIGINAL_READ)return 0;
    struct ap_invocation_key key;struct ap_fd_call *call=fd_actor_call(&key);
    /* Filter the exact original direct caller before interpreting an unrelated
     * fdget_pos callback as duplicate/stale state. No numeric-FD reread. */
    int direct=ap_read_direct_call(call,CORE(ctx->sp),fd_read_kernel);
    if(!direct)return 0;
    if(direct<0) {fd_problem(AP_FD_IDENTITY);return 0;}
    struct ap_command_result *r=result(c->command);
    if(!r || r->phase!=AP_COMMAND_RUNNING || r->operation!=AP_ORIGINAL_READ ||
       r->original_count!=c->original_count || r->task!=key.task || r->start_boottime!=key.start ||
       r->identity.provider!=c->provider) {fd_problem(AP_FD_MISSING);return 0;}
    struct files_struct *files=CORE(current_task()->files);u64 table=fd_table(files,0);
    if(!ap_read_selection_post(call,c,key.task,key.start,(u64)files,table,fd_attach_cookie(ctx),
         CORE(ctx->r12),CORE(ctx->r13),CORE(ctx->ax))) {
        call->original.problem|=AP_FD_IDENTITY;return 0;
    }
    fd_selected_file(call,0,1);
    return 0;
}

/* One loaded classic program retains all original exact-site links. Cookie
 * dispatch calls the unchanged validators; it does not drop any attachment.
 * The ftrace-only artifact omits this program and every classic attachment. */
SEC("kprobe") int fd_connect_post_fdget(struct pt_regs *ctx) {
#ifdef AP_GROUPED_PROVIDER
    u32 zero=0;struct ap_config *config=lookup(&ap_config_map,&zero);
    if(!config || config->anchor_phase<AP_GROUPED_ANCHORED)return 0;
    if(fd_attach_cookie_raw(ctx)!=AP_GROUPED_COOKIE) {fd_problem(AP_FD_IDENTITY);return 0;}
    unsigned role=fd_grouped_role(ctx);
    if(!role) {fd_problem(AP_FD_IDENTITY);return 0;}
    if(role==1 || role==4 || (role>=5 && role<=11)) {
        struct ap_invocation_key key;struct ap_fd_call *call=fd_actor_call(&key);
        const u64 file_entry=ap_grouped_site_ip(6,config->anchor_ip);
        const int file_session=call &&
            ap_file_fdget_session_marker(call->file_entry_ip,file_entry);
        const u64 read_entry=ap_grouped_site_ip(7,config->anchor_ip);
        const int read_session=call &&
            ap_read_fentry_session_marker(call->file_entry_ip,read_entry);
        if(call && ((role==1 || role==4) ? ap_fd_selection_complete(&call->selection) :
           (role==5 || role==6) ? file_session && call->selection.entered==1 :
           role==7 ? read_session :
           (role==8 || role==9) ? read_session && call->selection.entered==1 :
           role==10 ? call->original.epoll_ctl.primary_selected==1 :
                      call->original.epoll_ctl.secondary_selected==1))return 0;
    }
    if(role==12 || role==14)return fd_stream_copy_enter(ctx);
    if(role==13 || role==15)return fd_stream_copy_exit(ctx);
    if(role==16 || role==17)return stream_membership_dispatch(ctx);
#endif
    if(fd_attach_cookie(ctx)==AP_CONNECT_COPY_BEFORE_COOKIE)return fd_connect_copy_boundary(ctx,0);
    if(fd_attach_cookie(ctx)==AP_CONNECT_COPY_AFTER_COOKIE)return fd_connect_copy_boundary(ctx,1);
    if(fd_attach_cookie(ctx)==AP_ACCEPT_POST_COOKIE)return fd_original_post_fdget(ctx,AP_ACCEPT_EFFECT);
    if(fd_attach_cookie(ctx)==AP_EPOLL_PRIMARY_POST_COOKIE || fd_attach_cookie(ctx)==AP_EPOLL_TARGET_POST_COOKIE)
        return fd_original_epoll_post(ctx);
    if(fd_attach_cookie(ctx)==AP_READ_ENTRY_COOKIE)return fd_original_read_pre(ctx);
    if(fd_attach_cookie(ctx)==AP_READ_SELECTED_COOKIE || fd_attach_cookie(ctx)==AP_READ_NULL_COOKIE)
        return fd_original_read_selected(ctx);
    if(fd_attach_cookie(ctx)==AP_FILE_ENTRY_COOKIE)return fd_original_file_pre(ctx);
    if(fd_attach_cookie(ctx)==AP_FILE_POST_COOKIE)return fd_original_file_post(ctx);
    if(fd_attach_cookie(ctx)==AP_CONNECT_POST_COOKIE)
        return fd_original_post_fdget(ctx,AP_ORIGINAL_CONNECT);
    fd_problem(AP_FD_IDENTITY);return 0;
}
#endif

/* inet_accept receives the actual listener socket selected by fdget, after
 * security_socket_accept. It is not a lookup of the numeric FD after return. */
SEC("fentry/inet_accept") int fd_listener_selected(u64 *ctx) {
    struct ap_invocation_key key;struct ap_fd_call *call=fd_actor_call(&key);
    struct ap_fd_accept *a=fd_accept(call);if(!a)return 0;
    struct socket *socket=(struct socket *)ctx[0];struct sock *sk=CORE(socket->sk);
    struct file *selected=CORE(socket->file);
    if(!ap_fd_selection_complete(&call->selection) ||
       (call->selection.word&~3ULL)!=(u64)selected || !call->selected_file ||
       call->selected_file!=fd_file(selected)) { a->problem|=AP_FD_IDENTITY;return 0; }
    struct ap_object *o=object(sk);struct ap_task_command *c=command();
    if(!o || !c || o->creation || o->object!=c->expected_object) { a->problem|=AP_FD_IDENTITY;return 0; }
    a->listener=identity(sk,o->object);a->phases|=AP_FD_LISTENER;
    return 0;
}
SEC("fexit/inet_csk_accept") int fd_child_dequeued(u64 *ctx) {
    struct ap_invocation_key key;struct ap_fd_call *call=fd_actor_call(&key);
    struct ap_fd_accept *a=fd_accept(call);if(!a)return 0;
    struct sock *child=(struct sock *)ctx[2];
    if(!child || (u64)child>=(u64)-4095)return 0;
    struct ap_object *o=object(child);
    if(!o || !o->creation || a->phases&AP_FD_DEQUEUED) { a->problem|=AP_FD_IDENTITY;return 0; }
    a->child=identity(child,o->object);a->creation=o->creation;a->cookie=o->cookie;
    a->phases|=AP_FD_DEQUEUED;
    return 0;
}
SEC("fexit/do_accept") int fd_new_file_returned(u64 *ctx) {
    struct ap_invocation_key key;struct ap_fd_call *call=fd_actor_call(&key);
    struct ap_fd_accept *a=fd_accept(call);if(!a)return 0;
    struct file *file=(struct file *)ctx[5];
    if((u64)file>=(u64)-4095) { a->do_accept_errno=-(s64)(u64)file;return 0; }
    if(!file || !(a->phases&AP_FD_DEQUEUED) || call->new_file) { a->problem|=AP_FD_OUTCOME;return 0; }
    call->new_file=(u64)file;a->file=fd_file(file);a->phases|=AP_FD_FILE_RETURNED;
    return 0;
}
SEC("fentry/fd_install") int fd_install_enter(u64 *ctx) {
    struct files_struct *files=CORE(current_task()->files);u64 table=fd_table(files,0);
    if(!table)return 0;
    struct file *file=(struct file *)ctx[1];u64 file_id=fd_file(file);
    struct ap_task_command *c=command();
    if(c && ap_original_allocator(c->operation)) {
        if(!file || !file_id || ctx[0]>0x7fffffffULL) {fd_problem(AP_FD_IDENTITY);return 0;}
        fd_original_allocator_claim(c,file,file_id,(s32)ctx[0]);
    }
    struct ap_invocation_key key;struct ap_fd_call *call=fd_actor_call(&key);
    struct ap_fd_accept *a=fd_accept(call);
    struct ap_fd_install_call install={.table=table,.file=file_id,.raw_file=(u64)file,.fd=ctx[0]};
    if(call && ap_original_allocator(call->operation)) {
        if(!c || call->command!=c->command || call->new_file!=(u64)file ||
           call->raw_table!=(u64)files || call->original.selection.file!=file_id ||
           call->original.selection.ready || call->original.problem || call->install_begin ||
           call->original.installation.fd!=(s32)ctx[0]) {
            call->original.problem|=AP_FD_IDENTITY;fd_problem(AP_FD_IDENTITY);
        } else install.accept_command=call->command;
    }
    if(a) {
        if(call->new_file!=(u64)file || a->file!=file_id || call->raw_table!=(u64)files || a->phases&AP_FD_INSTALL_ENTERED) {
            a->problem|=AP_FD_IDENTITY;
        } else install.accept_command=a->command;
    }
    install.begin=fd_event(AP_FD_INSTALL_BEGIN,table,ctx[0],file_id,0,0,install.accept_command,0);
    if(update(&fd_install_calls,&key,&install,BPF_NOEXIST)) { fd_problem(AP_FD_DUPLICATE);return 0; }
    if(call && ap_original_allocator(call->operation) && install.accept_command==call->command) {
        call->install_begin=install.begin;call->original.installation.begin=install.begin;
    }
    if(call && call->operation==AP_NATIVE_BIRTH) {
        if(!(call->birth.kernel_flags&AP_CLONE_PIDFD) || call->birth.pidfd_install_begin ||
           call->birth.ready || call->birth.creator_table!=table || !file_id) {
            call->birth.problem|=AP_FD_IDENTITY;fd_problem(AP_FD_IDENTITY);
        } else {
            call->birth.pidfd_install_begin=install.begin;
            call->birth.pidfd_file=file_id;call->birth.pidfd_fd=(s32)ctx[0];
        }
    }
    if(a && install.accept_command) {
        a->returned_fd=(s32)ctx[0];a->phases|=AP_FD_INSTALL_ENTERED;
        a->install_begin=install.begin;call->install_begin=install.begin;
    }
    /* This actual entry precedes fd_install's slot publication. Only the
     * original Socket or accepted fresh file has no earlier guest consumer.
     * Inherited files, dup/SCM imports and census lookup retain unenrolled
     * state; the receive membership path must poison any attempted use. */
    const int fresh_socket=call && c && call->operation==AP_ORIGINAL_SOCKET_CALL &&
        c->operation==AP_ORIGINAL_SOCKET_CALL && call->command==c->command &&
        !call->original.problem && install.accept_command==call->command;
    const int fresh_accept=a && c && c->operation==AP_ACCEPT_EFFECT &&
        c->command==a->command && !a->problem && install.accept_command==a->command &&
        (a->phases&(AP_FD_DEQUEUED|AP_FD_FILE_RETURNED|AP_FD_INSTALL_ENTERED))==
            (AP_FD_DEQUEUED|AP_FD_FILE_RETURNED|AP_FD_INSTALL_ENTERED);
    if(AP_NATIVE_COPY_VERSION==5 && (fresh_socket || fresh_accept)) {
        u64 pointer=(u64)file;struct ap_fd_file *owned=lookup(&fd_files,&pointer);
        if(!ap_stream_frontier_enroll_install(owned,file_id,install.accept_command,install.begin))
            fd_problem(AP_FD_IDENTITY);
    }
    return 0;
}
SEC("fexit/fd_install") int fd_install_returned(u64 *ctx) {
    struct ap_invocation_key key=fd_actor();struct ap_fd_install_call *install=lookup(&fd_install_calls,&key);
    if(!install)return 0;
    if(install->fdup.entry_stack) {fd_problem(AP_FD_IDENTITY);return 0;}
    /* Pointer comparison only. The consumed file may already have been freed. */
    if(install->raw_file!=ctx[1] || install->fd!=(u32)ctx[0] || !install->begin) { fd_problem(AP_FD_IDENTITY);return 0; }
    u64 end=fd_event(AP_FD_INSTALL_END,install->table,install->fd,install->file,0,install->begin,install->accept_command,0);
    if(install->accept_command) {
        struct ap_fd_call *call=lookup(&fd_calls,&key);struct ap_fd_accept *a=fd_accept(call);
        if(call && ap_original_allocator(call->operation)) {
            if(call->command!=install->accept_command || call->install_begin!=install->begin ||
               call->original.installation.begin!=install->begin || call->original.installation.end ||
               call->original.selection.ready || call->original.selection.file!=install->file ||
               call->original.installation.fd!=(s32)install->fd || !end) {
                call->original.problem|=AP_FD_IDENTITY;fd_problem(AP_FD_IDENTITY);
            } else {
                call->original.installation.end=end;
                call->original.selection.ready=1;
            }
        } else {
        if(!a || a->command!=install->accept_command)a=0;
        if(!a)fd_problem(AP_FD_MISSING);
        else { a->install_end=end;a->phases|=AP_FD_INSTALL_RETURNED; }
        }
    }
    struct ap_fd_call *birth=lookup(&fd_calls,&key);
    if(birth && birth->operation==AP_NATIVE_BIRTH) {
        if(birth->birth.pidfd_install_begin!=install->begin || birth->birth.pidfd_install_end || birth->birth.ready) {
            birth->birth.problem|=AP_FD_IDENTITY;fd_problem(AP_FD_IDENTITY);
        } else birth->birth.pidfd_install_end=end;
    }
    remove_key(&fd_install_calls,&key);
    return 0;
}
SEC("fexit/__sys_accept4") int fd_accept_returned(u64 *ctx) {
    struct ap_invocation_key key;struct ap_fd_call *call=fd_actor_call(&key);
    struct ap_fd_accept *a=fd_accept(call);if(!a)return 0;
    struct ap_command_result *r=result(a->command);if(!r)return 0;
    s32 returned=(s32)ctx[4];r->returned=returned;
    /* Do not dereference the fdget pointer here: fdput already ran. These are
     * retained facts from the original invocation, including an EMPTY_FD. */
    if(!ap_fd_selection_complete(&call->selection))a->problem|=AP_FD_MISSING;
    else if(!call->selection.word && returned!=-9)a->problem|=AP_FD_OUTCOME;
    if(returned>=0) {
        if(!(a->phases&AP_FD_INSTALL_RETURNED) || returned!=a->returned_fd || !a->file)
            a->problem|=AP_FD_OUTCOME;
    } else if(a->phases&AP_FD_INSTALL_ENTERED) a->problem|=AP_FD_OUTCOME;
    if(a->phases&AP_FD_DEQUEUED) { r->identity=a->child;r->creation=a->creation;r->cookie=a->cookie; }
    a->phases|=AP_FD_SYSCALL_RETURNED;
    remove_key(&fd_calls,&key);
    /* This is the final access to both accept receipt and command result. */
    publish_result(r);return 0;
}

SEC("fexit/file_close_fd_locked") int fd_removed(u64 *ctx) {
    u64 table=fd_table((struct files_struct *)ctx[0],0);if(!table)return 0;
    struct file *file=(struct file *)ctx[2];if(!file)return 0;
    /* The removed reference is still held by the close caller, under file_lock. */
    fd_event(AP_FD_REMOVE,table,ctx[1],fd_file(file),0,0,0,0);return 0;
}
/* The exact qualified image inlines file_close_fd_locked inside file_close_fd.
 * Keep the old hook above for close_fd/range callers which invoke it. No event
 * is suppressed merely because this task has a removal invocation. */
/* The original close shares fd_calls and the original command owner. No
 * pidfd_getfd or extra kernel file reference is acquired. The removed reference
 * is held by the kernel caller through filp_close; only its normalized identity
 * crosses the observation boundary. */
static __attribute__((noinline)) int fd_close_enter(struct ap_task_command *c,
        struct files_struct *files,u64 table,s32 fd) {
    struct ap_invocation_key key;
    struct ap_fd_call *prior=fd_actor_call(&key);
    if(prior) {
        /* A nested flush may close another descriptor after the original
         * selection. It is journaled normally, never a replacement selection. */
        if(prior->operation==AP_ORIGINAL_CLOSE && prior->command==c->command &&
           prior->original.selection.ready==1)return 0;
        fd_problem(AP_FD_DUPLICATE);return 0;
    }
    if(c->expected_level!=fd || c->expected_option || c->generation_before || !table) {
        fd_problem(AP_FD_IDENTITY);return 0;
    }
    struct ap_command_result *r=claim_result(c);if(!r)return 0;
    r->identity.provider=c->provider;
    struct ap_fd_call fresh;ap_fd_call_clear(&fresh);
    fresh.command=c->command;
    fresh.operation=AP_ORIGINAL_CLOSE;
    fresh.raw_table=(u64)files;
    fresh.original.selection=(struct ap_original_selection){.command=c->command,
        .call=c->expected_object,.owner_mm=c->generation_after,.provider=c->provider,
        .task=key.task,.task_start=key.start,.table=table,.requested_fd=fd};
    __asm__ __volatile__("" : : "r"(&fresh) : "memory");
    if(update(&fd_calls,&key,&fresh,BPF_NOEXIST))fd_problem(AP_FD_DUPLICATE);
    return 0;
}
SEC("fentry/file_close_fd") int fd_remove_enter(u64 *ctx) {
    struct files_struct *files=CORE(current_task()->files);
    u64 table=fd_table(files,0);if(!table)return 0;
    struct ap_invocation_key key=fd_actor();
    struct ap_task_command *c=command();
    if(c && c->operation==AP_ORIGINAL_CLOSE)fd_close_enter(c,files,table,(s32)ctx[0]);
    struct ap_fd_remove_call fresh={.raw_table=(u64)files,.table=table,.fd=(u32)ctx[0]};
    if(update(&fd_removals,&key,&fresh,BPF_NOEXIST)) { fd_problem(AP_FD_DUPLICATE);return 0; }
    struct ap_fd_remove_call *call=lookup(&fd_removals,&key);
    if(!call) { fd_problem(AP_FD_MISSING);return 0; }
    call->begin=fd_event(AP_FD_REMOVE_BEGIN,table,(s32)fresh.fd,0,0,0,0,0);
    return 0;
}
SEC("fexit/file_close_fd") int fd_remove_returned(u64 *ctx) {
    struct ap_invocation_key key=fd_actor();
    struct ap_fd_remove_call *call=lookup(&fd_removals,&key);
    if(!call) {
        if(fd_table(CORE(current_task()->files),0))fd_problem(AP_FD_MISSING);
        return 0;
    }
    if(!call->begin || call->raw_table!=(u64)CORE(current_task()->files) ||
       call->table!=fd_table((struct files_struct *)call->raw_table,0) || call->fd!=(u32)ctx[0]) {
        fd_problem(AP_FD_IDENTITY);return 0; /* Retain unresolved invocation. */
    }
    /* The returned pointer IS the removed reference, still held by the caller.
     * No table lookup or syscall-success inference reconstructs that file. */
    struct file *file=(struct file *)ctx[1];
    u64 identity=file?fd_file(file):0;
    if(file && !identity) { fd_problem(AP_FD_IDENTITY);return 0; }
    if(!fd_event(file?AP_FD_REMOVE:AP_FD_REMOVE_NO_FILE,call->table,(s32)call->fd,
                 identity,0,call->begin,0,0))return 0;
    struct ap_fd_call *original=lookup(&fd_calls,&key);
    if(original && original->operation==AP_ORIGINAL_CLOSE && !original->original.selection.ready) {
        struct ap_task_command *c=command();
        struct ap_original_selection *selected=&original->original.selection;
        if(!c || c->operation!=AP_ORIGINAL_CLOSE || c->command!=original->command ||
           original->raw_table!=call->raw_table || selected->table!=call->table ||
           selected->requested_fd!=(s32)call->fd || selected->task!=key.task ||
           selected->task_start!=key.start || original->original.complete) {
            original->original.problem|=AP_FD_IDENTITY;fd_problem(AP_FD_IDENTITY);return 0;
        }
        selected->file=identity;
        /* Publish only after both paired removal rows and the real held return
         * are durable. Final syscall return may be blocked in flush indefinitely. */
        __sync_val_compare_and_swap(&selected->ready,0,1);
    }
    remove_key(&fd_removals,&key);return 0;
}
SEC("fentry/do_dup2") int fd_replace_enter(u64 *ctx) {
    u64 table=fd_table((struct files_struct *)ctx[0],0);if(!table)return 0;
    struct ap_invocation_key key=fd_actor();
    struct ap_fd_replace replacement={.table=table,.file=fd_file((struct file *)ctx[1]),.fd=ctx[2]};
    replacement.begin=fd_event(AP_FD_REPLACE_BEGIN,table,ctx[2],replacement.file,0,0,0,0);
    if(update(&fd_replacements,&key,&replacement,BPF_NOEXIST))fd_problem(AP_FD_DUPLICATE);
    return 0;
}
SEC("fentry/filp_close") int fd_replaced_old_file(u64 *ctx) {
    struct ap_invocation_key key=fd_actor();struct ap_fd_replace *r=lookup(&fd_replacements,&key);
    if(!r)return 0;
    /* The first direct filp_close in do_dup2 receives its actual `tofree`.
     * Nested flush closes are retained as unresolved rather than replacing it. */
    if(r->old_seen) { fd_problem(AP_FD_UNKNOWN_TABLE);return 0; }
    r->old_seen=1;r->previous_file=fd_file((struct file *)ctx[0]);
    fd_event(AP_FD_REPLACE_OLD_FILE,r->table,r->fd,r->file,r->previous_file,r->begin,0,0);
    return 0;
}
SEC("fexit/do_dup2") int fd_replace_returned(u64 *ctx) {
    struct ap_invocation_key key=fd_actor();struct ap_fd_replace *r=lookup(&fd_replacements,&key);
    if(!r)return 0;
    s32 returned=(s32)ctx[4];
    if((returned<0 && r->old_seen) || (returned>=0 && (u32)returned!=r->fd))fd_problem(AP_FD_OUTCOME);
    fd_event(AP_FD_REPLACE_END,r->table,r->fd,r->file,r->previous_file,r->begin,0,returned);
    remove_key(&fd_replacements,&key);return 0;
}
static __attribute__((noinline)) int fd_retire_file(u64 key) {
    struct ap_fd_file *file=lookup(&fd_files,&key);if(!file)return 0;
    /* Final release cannot erase a poisoned or unmatched native attempt.
     * The existing run-wide sticky problem survives this incarnation row. */
    if(AP_NATIVE_COPY_VERSION==5 &&
       (ap_stream_frontier_state(file)&(AP_STREAM_FRONTIER_ACTIVE|AP_STREAM_FRONTIER_POISON)))
        fd_problem(AP_FD_OUTCOME);
    fd_event(AP_FD_FILE_RETIRED,0,-1,file->identity,0,0,0,0);
    if(remove_key(&fd_files,&key))fd_problem(AP_FD_MISSING);
    return 0;
}
/* This is the SAME audited final-release function, with only its actual LTO
 * symbol selected by the driver. Unlike a trampoline ctx, a kprobe receives
 * x86_64 pt_regs: DI is the actual first struct file* argument. */
_Static_assert(__builtin_offsetof(struct pt_regs,di)==14*8,"reviewed x86_64 DI slot");
SEC("kprobe.multi") int fd_file_retired(struct pt_regs *ctx) {
    return fd_retire_file(CORE(ctx->di));
}
/* The exact BuildID contract binds this instruction boundary. Entry records
 * the actual function IP; the filp_close probe accepts only its direct call
 * return address, original table generation and same task incarnation. */
SEC("fentry/do_close_on_exec") int fd_exec_enter(u64 *ctx) {
    u64 table=fd_table((struct files_struct *)ctx[0],0);if(!table)return 0;
    struct ap_invocation_key key=fd_actor();
    struct ap_fd_exec_call call={.raw_table=ctx[0],.table=table,
        .function_ip=fd_function_ip(ctx)};
    if(!call.function_ip || CORE(current_task()->files)!=(struct files_struct *)ctx[0]) {
        fd_problem(AP_FD_IDENTITY);return 0;
    }
    call.begin=fd_event(AP_FD_EXEC_BEGIN,table,-1,0,0,0,0,0);
    if(!call.begin)return 0;
    if(update(&fd_execs,&key,&call,BPF_NOEXIST))fd_problem(AP_FD_DUPLICATE);
    return 0;
}
_Static_assert(__builtin_offsetof(struct pt_regs,bp)==4*8,"reviewed x86_64 BP slot");
_Static_assert(__builtin_offsetof(struct pt_regs,si)==13*8,"reviewed x86_64 SI slot");
_Static_assert(__builtin_offsetof(struct pt_regs,sp)==19*8,"reviewed x86_64 SP slot");
SEC("kprobe.multi") int fd_exec_closed_file(struct pt_regs *ctx) {
    struct ap_invocation_key key=fd_actor();struct ap_fd_exec_call *call=lookup(&fd_execs,&key);
    if(!call)return 0;
    u64 return_ip=0;
    if(fd_read_kernel(&return_ip,sizeof(return_ip),(const void *)CORE(ctx->sp))) {
        fd_problem(AP_FD_MISSING);return 0;
    }
    /* Nested flush and all other filp_close callers are separate operations.
     * Their frame cannot be decoded using do_close_on_exec's register layout. */
    if(!ap_exec_close_site(call->function_ip,return_ip))return 0;
    if(!call->begin || CORE(ctx->si)!=call->raw_table ||
       CORE(current_task()->files)!=(struct files_struct *)call->raw_table ||
       fd_table((struct files_struct *)call->raw_table,0)!=call->table ||
       CORE(ctx->bp)>0x7fffffffULL || !CORE(ctx->di)) {
        fd_problem(AP_FD_IDENTITY);return 0;
    }
    /* The real removed reference is held in DI; BP is the exact cleared slot,
     * including the case where two slots referred to this same file. */
    u64 file=fd_file((struct file *)CORE(ctx->di));
    if(!file) { fd_problem(AP_FD_IDENTITY);return 0; }
    if(fd_event(AP_FD_EXEC_REMOVE,call->table,(s32)CORE(ctx->bp),file,0,call->begin,0,0))
        call->removed++;
    return 0;
}
SEC("fexit/do_close_on_exec") int fd_exec_returned(u64 *ctx) {
    struct ap_invocation_key key=fd_actor();struct ap_fd_exec_call *call=lookup(&fd_execs,&key);
    if(!call)return 0;
    if(ctx[0]!=call->raw_table || !call->begin ||
       fd_table((struct files_struct *)ctx[0],0)!=call->table ||
       call->removed>0x7fffffffULL) { fd_problem(AP_FD_IDENTITY);return 0; }
    if(!fd_event(AP_FD_EXEC_END,call->table,-1,0,0,call->begin,0,(s32)call->removed))return 0;
    remove_key(&fd_execs,&key);return 0;
}

/* The kernel-owned args pointer is captured only while kernel_clone and its
 * exact paired copy_process are executing. Flags are never fetched from user
 * memory. Task-storage command identity and the existing invocation key scope
 * every subsequent copy/install/birth observation. */
SEC("fentry/kernel_clone") int fd_native_clone_enter(u64 *ctx) {
    struct ap_task_command *c=command();if(!c || c->operation!=AP_NATIVE_BIRTH)return 0;
    if(!ap_native_birth_syscall(c->expected_level) || c->expected_option) {fd_problem(AP_FD_IDENTITY);return 0;}
    struct ap_command_result *r=claim_result(c);if(!r)return 0;
    struct files_struct *files=CORE(current_task()->files);
    u64 table=fd_table(files,0);
    if(!table || table!=c->generation_before || !ctx[0]) { fd_problem(AP_FD_IDENTITY);return 0; }
    struct ap_invocation_key key=fd_actor();
    struct ap_fd_call fresh;ap_fd_call_clear(&fresh);
    fresh.command=c->command;
    fresh.raw_table=(u64)files;
    fresh.function_ip=ctx[0];
    fresh.operation=AP_NATIVE_BIRTH;
    fresh.birth=(struct ap_native_birth){.command=c->command,.call=c->expected_object,
        .provider=c->provider,.owner_mm=c->generation_after,.creator_task=key.task,
        .creator_start=key.start,.creator_table=table,.pidfd_fd=-1};
    if(update(&fd_calls,&key,&fresh,BPF_NOEXIST))fd_problem(AP_FD_DUPLICATE);
    return 0;
}
SEC("fentry/copy_process") int fd_native_copy_process(u64 *ctx) {
    struct ap_invocation_key key;struct ap_fd_call *call=fd_actor_call(&key);
    if(!call || call->operation!=AP_NATIVE_BIRTH)return 0;
    if(ctx[3]!=call->function_ip || call->selected_file || call->birth.ready) {
        call->birth.problem|=AP_FD_DUPLICATE;fd_problem(AP_FD_DUPLICATE);return 0;
    }
    struct kernel_clone_args *args=(struct kernel_clone_args *)ctx[3];
    call->birth.kernel_flags=CORE(args->flags);
    call->birth.requested_exit_signal=CORE(args->exit_signal);
    call->selected_file=1; /* exact copy_process entry observed, not a pointer */
    return 0;
}
/* This target's copy_process calls klp_copy_process after all effective
 * real_parent/exit_signal branches and before task-list publication while
 * tasklist_lock is held. The package binds this exact hook/BuildID contract.
 * A witness is provisional: only the later matched successful fork commits it.
 * No late real_parent reconstruction is permitted. */
SEC("fentry/klp_copy_process") int fd_native_parent_chosen(u64 *ctx) {
    struct ap_invocation_key key;struct ap_fd_call *call=fd_actor_call(&key);
    if(!call || call->operation!=AP_NATIVE_BIRTH)return 0;
    struct task_struct *child=(struct task_struct *)ctx[0];
    struct ap_native_birth *b=&call->birth;
    if(!child || call->selected_file!=1 || call->security_socket || b->ready) {
        b->problem|=AP_FD_DUPLICATE;fd_problem(AP_FD_DUPLICATE);return 0;
    }
    struct task_struct *parent=CORE(child->real_parent);
    if(!parent) { b->problem|=AP_FD_MISSING;fd_problem(AP_FD_MISSING);return 0; }
    call->security_socket=(u64)child; /* private paired-copy witness only */
    b->child_task=((u64)(u32)CORE(child->tgid)<<32)|(u32)CORE(child->pid);
    b->child_start=CORE(child->start_boottime);
    b->parent_task=((u64)(u32)CORE(parent->tgid)<<32)|(u32)CORE(parent->pid);
    b->parent_start=CORE(parent->start_boottime);b->exit_signal=CORE(child->exit_signal);
    b->clear_child_tid=(u64)CORE(child->clear_child_tid);
    return 0;
}
SEC("tp_btf/sched_process_fork") int fd_native_fork(u64 *ctx) {
    struct ap_invocation_key key;struct ap_fd_call *call=fd_actor_call(&key);
    if(!call || call->operation!=AP_NATIVE_BIRTH)return 0;
    struct ap_task_command *c=command();struct ap_native_birth *b=&call->birth;
    struct task_struct *creator=(struct task_struct *)ctx[0],*child=(struct task_struct *)ctx[1];
    if(!c || c->command!=call->command || creator!=current_task() || !child ||
       call->selected_file!=1 || call->security_socket!=(u64)child || b->ready) {
        b->problem|=AP_FD_IDENTITY;fd_problem(AP_FD_IDENTITY);return 0;
    }
    struct files_struct *creator_files=CORE(creator->files),*child_files=CORE(child->files);
    struct mm_struct *creator_mm=CORE(creator->mm),*child_mm=CORE(child->mm);
    if(!creator_files || !child_files || !creator_mm || !child_mm ||
       (u64)creator_files!=call->raw_table) { b->problem|=AP_FD_IDENTITY;fd_problem(AP_FD_IDENTITY);return 0; }
    if(b->child_task!=(((u64)(u32)CORE(child->tgid)<<32)|(u32)CORE(child->pid)) ||
       b->child_start!=CORE(child->start_boottime)) {
        b->problem|=AP_FD_IDENTITY;fd_problem(AP_FD_IDENTITY);return 0;
    }
    b->child_table=fd_table(child_files,0);
    b->shared_mm=creator_mm==child_mm;b->shared_files=creator_files==child_files;
    b->same_thread_group=CORE(creator->tgid)==CORE(child->tgid);
    /* A live child later proves this exact marker through its held pidfd.
     * The same operation's historical birth is still retained if it dies
     * before the backend can collect; absence of this marker proves nothing. */
    struct ap_task_command *child_command=task_storage(&tasks,child,0,BPF_LOCAL_STORAGE_GET_F_CREATE);
    if(!child_command || child_command->provider || child_command->command || child_command->operation) {
        b->problem|=AP_FD_DUPLICATE;fd_problem(AP_FD_DUPLICATE);return 0;
    }
    *child_command=*c;
    if(__sync_val_compare_and_swap(&b->ready,0,1)!=0)fd_problem(AP_FD_DUPLICATE);
    return 0;
}
SEC("fexit/kernel_clone") int fd_native_clone_returned(u64 *ctx) {
    struct ap_invocation_key key;struct ap_fd_call *call=fd_actor_call(&key);
    if(!call || call->operation!=AP_NATIVE_BIRTH)return 0;
    struct ap_command_result *r=result(call->command);if(!r) { fd_problem(AP_FD_MISSING);return 0; }
    s64 returned=(s32)ctx[1];
    if(ctx[0]!=call->function_ip || call->copied_address || returned>0x7fffffffLL || returned< -4095 ||
       (!returned) || (returned>0 && call->birth.ready!=1) ||
       (returned<0 && call->birth.ready)) {
        call->birth.problem|=AP_FD_OUTCOME;fd_problem(AP_FD_OUTCOME);return 0;
    }
    r->identity.provider=call->birth.provider;r->returned=(s32)returned;
    /* Keep RUNNING until actual original syscall exit. This witness is
     * independent of birth-ready; a vfork child can be admitted first. */
    call->copied_address=1; /* private op8 kernel-return witness, never a pointer */
    return 0;
}
static __attribute__((noinline)) int fd_close_returned(u64 *ctx,struct ap_task_command *c) {
    struct pt_regs *regs=(struct pt_regs *)ctx[0];
    if(!regs) {fd_problem(AP_FD_MISSING);return 0;}
    if(CORE(regs->orig_ax)!=3)return 0; /* exact x86-64 SYS_close */
    struct ap_invocation_key key;
    struct ap_fd_call *call=fd_actor_call(&key);
    struct ap_command_result *r=result(c->command);
    s64 returned=(s64)ctx[1];
    if(!call || !r || call->operation!=AP_ORIGINAL_CLOSE || call->command!=c->command ||
       r->operation!=AP_ORIGINAL_CLOSE || r->phase!=AP_COMMAND_RUNNING ||
       r->task!=key.task || r->start_boottime!=key.start ||
       !ap_original_selection_matches(c,&call->original.selection) ||
       call->original.complete || call->original.problem || returned>0 || returned< -4095 ||
       (!call->original.selection.file && returned!=-9)) {
        fd_problem(AP_FD_OUTCOME);return 0;
    }
    call->original.returned=(s32)returned;call->original.complete=1;
    r->returned=(s32)returned;
    /* Final access: provider ACK may retire this exact immutable command. */
    publish_result(r);return 0;
}
static __attribute__((noinline)) int fd_original_file_returned(u64 *ctx,struct ap_task_command *c) {
    struct pt_regs *regs=(struct pt_regs *)ctx[0];
    if(!regs) {fd_problem(AP_FD_MISSING);return 0;}
    if(CORE(regs->orig_ax)!=AP_FILE_SYSCALL_FCNTL)return 0;
    struct ap_invocation_key key;struct ap_fd_call *call=fd_actor_call(&key);
    struct ap_command_result *r=result(c->command);s64 returned=(s64)ctx[1];
    if(!ap_original_file_native_return(c,r,call,key.task,key.start,CORE(regs->orig_ax),
          (s32)CORE(regs->di),(s32)CORE(regs->si),returned)) {
        fd_problem(AP_FD_OUTCOME);return 0;
    }
    call->original.returned=(s32)returned;call->original.complete=1;r->returned=(s32)returned;
    /* F_GETFL has no branch before fdget_raw. A missing positive/zero post
     * witness is unknown even on EBADF, never an early-error shortcut. */
    publish_result(r);return 0;
}
static __attribute__((noinline)) int fd_original_read_returned(u64 *ctx,struct ap_task_command *c) {
    struct pt_regs *regs=(struct pt_regs *)ctx[0];
    if(!regs) {fd_problem(AP_FD_MISSING);return 0;}
    if(CORE(regs->orig_ax)!=AP_READ_SYSCALL)return 0;
    struct ap_invocation_key key;struct ap_fd_call *call=fd_actor_call(&key);
    struct ap_command_result *r=result(c->command);s64 returned=(s64)ctx[1];
    if(!ap_original_read_native_return(c,r,call,key.task,key.start,CORE(regs->orig_ax),
          (s32)CORE(regs->di),CORE(regs->si),CORE(regs->dx),returned)) {
        fd_problem(AP_FD_OUTCOME);return 0;
    }
    call->entry_stack=0;
    call->original.returned=(s32)returned;call->original.complete=1;r->returned=(s32)returned;
    /* The raw exit commits only observed kernel-copy records for this Call. */
    stream_copy_commit(call,returned);
    publish_result(r);return 0;
}

static __attribute__((noinline)) int fd_original_recv_returned(u64 *ctx,struct ap_task_command *c) {
    struct pt_regs *regs=(struct pt_regs *)ctx[0];
    if(!regs) {fd_problem(AP_FD_MISSING);return 0;}
    s64 nr=CORE(regs->orig_ax);
    if(nr!=(c->operation==AP_ORIGINAL_RECVFROM_CALL?AP_RECVFROM_SYSCALL:AP_RECVMSG_SYSCALL))return 0;
    struct ap_invocation_key key;struct ap_fd_call *call=fd_actor_call(&key);
    struct ap_command_result *r=result(c->command);s64 returned=(s64)ctx[1];
    if(CORE(regs->cs)!=0x33 ||
       !ap_original_recv_operands(c,nr,CORE(regs->di),CORE(regs->si),CORE(regs->dx),
           CORE(regs->r10),CORE(regs->r8),CORE(regs->r9)) ||
       !ap_original_recv_context(c,r,call,key.task,key.start) ||
       call->raw_table!=(u64)CORE(current_task()->files) || call->new_file!=(u64)CORE(current_task()->mm) ||
       call->copied_address || call->security_socket ||
       !ap_original_read_return_value(c->original_count,returned) ||
       !ap_original_recv_copy_version(call)) {
        fd_problem(AP_FD_OUTCOME);return 0;
    }
    call->original.returned=(s32)returned;call->original.complete=1;r->returned=(s32)returned;
    stream_copy_commit(call,returned);publish_result(r);return 0;
}

static __attribute__((noinline)) int fd_original_allocator_returned(u64 *ctx,struct ap_task_command *c) {
    struct pt_regs *regs=(struct pt_regs *)ctx[0];
    if(!regs) {fd_problem(AP_FD_MISSING);return 0;}
    if(CORE(regs->orig_ax)!=(c->operation==AP_ORIGINAL_EPOLL_CALL
        ? c->generation_before : c->operation==AP_ORIGINAL_SOCKET_CALL
        ? AP_SOCKET_SYSCALL : AP_OPENAT_SYSCALL))return 0;
    s64 returned=(s64)ctx[1];
    if(!ap_original_allocator_operands(c,CORE(regs->orig_ax),CORE(regs->di),
          CORE(regs->si),CORE(regs->dx),CORE(regs->r10)) || returned< -4095 || returned>0x7fffffffLL) {
        fd_problem(AP_FD_OUTCOME);return 0;
    }
    struct ap_invocation_key key;
    struct ap_fd_call *call=fd_actor_call(&key);
    struct ap_command_result *r=result(c->command);
    if(!call) {
        /* Positive exact sys_exit is required. Absence of fd_install, READY,
         * cancellation, or a missing callback cannot issue this error fact.
         * The bound __sys_socket has no normal error after fd_install. */
        if(returned>=0 || !r || r->phase!=AP_COMMAND_READY || r->task || r->start_boottime ||
           r->command!=c->command || r->operation!=c->operation) {
            fd_problem(AP_FD_OUTCOME);return 0;
        }
        fd_original_allocator_claim(c,0,0,0);
        call=lookup(&fd_calls,&key);r=result(c->command);
    }
    if(!ap_original_allocator_native_return(c,r,call,key.task,key.start,CORE(regs->orig_ax),
          CORE(regs->di),CORE(regs->si),CORE(regs->dx),CORE(regs->r10),returned)) {
        fd_problem(AP_FD_OUTCOME);return 0;
    }
    call->original.returned=(s32)returned;call->original.complete=1;r->returned=(s32)returned;
    publish_result(r);return 0;
}

/* Only the authenticated creator's validated native early-error branch
 * enters here. Keep its full424-byte private row out of the common exit frame. */
static __attribute__((noinline)) struct ap_command_result *fd_native_birth_early_error(
    const struct ap_task_command *c,const struct ap_invocation_key *key,s64 returned) {
    /* No kernel_clone entry was recorded. Absence is not the result:
     * the actual native negative return, exact armed command and current
     * creator/table supply this receipt. Journal cleanup stays required. */
    struct files_struct *files=CORE(current_task()->files);u64 table=fd_table(files,0);
    if(!table || table!=c->generation_before) {fd_problem(AP_FD_IDENTITY);return 0;}
    struct ap_command_result *r=claim_result(c);if(!r)return 0;
    struct ap_fd_call fresh;ap_fd_call_clear(&fresh);
    fresh.command=c->command;
    fresh.operation=AP_NATIVE_BIRTH;
    fresh.birth=(struct ap_native_birth){.command=c->command,.call=c->expected_object,
        .provider=c->provider,.owner_mm=c->generation_after,.creator_task=key->task,
        .creator_start=key->start,.creator_table=table,.pidfd_fd=-1};
    if(update(&fd_calls,key,&fresh,BPF_NOEXIST)) {fd_problem(AP_FD_DUPLICATE);return 0;}
    r->identity.provider=c->provider;r->returned=(s32)returned;
    return r;
}

SEC("tp_btf/sys_exit") int fd_native_syscall_returned(u64 *ctx) {
    struct ap_task_command *c=command();if(!c)return 0;
    if(c->operation==AP_EPOLL_CTL_COPY)return fd_epoll_ctl_returned(ctx,c);
    if(c->operation==AP_ORIGINAL_EPOLL_CTL)return fd_original_epoll_returned(ctx,c);
    if(c->operation==AP_ORIGINAL_CLOSE)return fd_close_returned(ctx,c);
    if(ap_original_file_operation(c->operation))return fd_original_file_returned(ctx,c);
    if(c->operation==AP_ORIGINAL_READ)return fd_original_read_returned(ctx,c);
    if(ap_original_recv(c->operation))return fd_original_recv_returned(ctx,c);
    if(ap_stream_tx_operation(c->operation))return stream_tx_syscall_exit(ctx,c);
    if(ap_original_allocator(c->operation))return fd_original_allocator_returned(ctx,c);
    if(c->operation!=AP_NATIVE_BIRTH)return 0;
    struct ap_command_result *r=result(c->command);
    struct ap_invocation_key key=fd_actor(),creator={0};
    if(r) {creator.task=r->task;creator.start=r->start_boottime;}
    struct ap_fd_call *origin=creator.task?lookup(&fd_calls,&creator):0;
    enum ap_native_actor actor=ap_native_birth_actor(c,r,origin,key.task,key.start);
    if(actor==AP_NATIVE_ACTOR_CHILD)return 0; /* exact newborn admission only */
    if(actor!=AP_NATIVE_ACTOR_CREATOR) {fd_problem(AP_FD_IDENTITY);return 0;}
    struct pt_regs *regs=(struct pt_regs *)ctx[0];
    if(!regs) {fd_problem(AP_FD_MISSING);return 0;}
    s64 syscall=CORE(regs->orig_ax);
    if(syscall!=c->expected_level)return 0; /* unrelated backend/guest syscall */
    struct ap_fd_call *call=lookup(&fd_calls,&key);
    s64 returned=(s64)ctx[1];
    enum ap_native_return kind=ap_native_birth_return(c,r,call,syscall,returned);
    if(kind==AP_NATIVE_RETURN_INVALID) {fd_problem(AP_FD_OUTCOME);return 0;}
    if(kind==AP_NATIVE_RETURN_EARLY_ERROR) {
        r=fd_native_birth_early_error(c,&key,returned);if(!r)return 0;
    }
    /* Final access: exact native exit is now durable; ACK may retire the slot. */
    publish_result(r);return 0;
}

SEC("fentry/dup_fd") int fd_copy_enter(u64 *ctx) {
    u64 table=fd_table((struct files_struct *)ctx[0],0);if(!table)return 0;
    struct ap_invocation_key key=invocation(0,ctx[0]);
    struct ap_fd_copy_call call={.table=table};
    struct ap_invocation_key actor;struct ap_fd_call *birth=fd_actor_call(&actor);
    if(birth && birth->operation==AP_NATIVE_BIRTH) {
        if(birth->birth.copy_begin || birth->birth.ready || birth->birth.creator_table!=table) {
            birth->birth.problem|=AP_FD_DUPLICATE;fd_problem(AP_FD_DUPLICATE);return 0;
        }
        call.birth_command=birth->command;
    }
    call.begin=fd_event(AP_FD_COPY_BEGIN,table,-1,0,0,0,0,0);
    if(!call.begin)return 0;
    if(call.birth_command)birth->birth.copy_begin=call.begin;
    if(update(&fd_copies,&key,&call,BPF_NOEXIST))fd_problem(AP_FD_DUPLICATE);
    return 0;
}
SEC("fexit/dup_fd") int fd_copy_returned(u64 *ctx) {
    struct ap_invocation_key key=invocation(0,ctx[0]);struct ap_fd_copy_call *call=lookup(&fd_copies,&key);
    if(!call)return 0;
    u64 begin=call->begin;struct files_struct *files=(struct files_struct *)ctx[2];
    if(!begin) { fd_problem(AP_FD_MISSING);return 0; }
    if((u64)files>=(u64)-4095) {
        /* The tracing return is a typed file-table pointer to the verifier.
         * Read its full saved representation into a scalar before narrowing
         * the native error; a partial pointer spill is not a scalar copy. */
        s64 returned=0;
        if(fd_read_kernel(&returned,sizeof(returned),&files)) {
            fd_problem(AP_FD_MISSING);return 0;
        }
        if(!fd_event(AP_FD_COPY_END,0,-1,0,0,begin,0,(s32)returned))return 0;
        remove_key(&fd_copies,&key);return 0;
    }
    /* Before return the new table is private, and every nonnull entry owns a
     * real file reference. We read the actual child table, never infer a copy
     * from the parent's possibly changing current slots. */
    if(!files || (u64)files==ctx[0] || CORE(files->count.counter)!=1) {
        fd_problem(AP_FD_IDENTITY);return 0;
    }
    struct fdtable *fdt=CORE(files->fdt);
    u32 slots=CORE(fdt->max_fds);
    if(!slots || slots>AP_FD_FILES) { fd_problem(AP_FD_CAPACITY|AP_FD_UNKNOWN_TABLE);return 0; }
    u64 table=fd_table(files,1);if(!table)return 0;
    struct file **array=CORE(fdt->fd);unsigned long *cloexec=CORE(fdt->close_on_exec);
    if(!array || !cloexec) { fd_problem(AP_FD_MISSING);return 0; }
    s32 copied=0;
    for(u32 fd=0;fd<AP_FD_FILES;fd++) {
        if(fd>=slots)break;
        struct file *file=0;unsigned long flags=0;
        if(fd_read_kernel(&file,sizeof(file),array+fd) ||
           fd_read_kernel(&flags,sizeof(flags),cloexec+fd/64)) {
            fd_problem(AP_FD_MISSING);return 0;
        }
        if(!file)continue;
        u64 identity=fd_file(file);if(!identity) { fd_problem(AP_FD_IDENTITY);return 0; }
        if(!fd_event(AP_FD_COPY_SLOT,table,(s32)fd,identity,0,begin,0,(flags>>(fd%64))&1))return 0;
        copied++;
    }
    /* END attests every slot through max_fds was read successfully, including
     * all holes. A missing END never supplies a partial-success census. */
    u64 end=fd_event(AP_FD_COPY_END,table,(s32)slots,0,0,begin,0,copied);
    if(!end)return 0;
    if(call->birth_command) {
        struct ap_invocation_key actor;struct ap_fd_call *birth=fd_actor_call(&actor);
        if(!birth || birth->operation!=AP_NATIVE_BIRTH || birth->command!=call->birth_command ||
           birth->birth.copy_begin!=begin || birth->birth.copy_end || birth->birth.ready) {
            fd_problem(AP_FD_IDENTITY);return 0;
        }
        birth->birth.copy_end=end;
    }
    remove_key(&fd_copies,&key);return 0;
}

SEC("fentry/put_files_struct") int fd_table_put_enter(u64 *ctx) {
    u64 table=fd_table((struct files_struct *)ctx[0],0);if(!table)return 0;
    struct ap_invocation_key key=invocation(0,ctx[0]);
    struct ap_fd_put_call call={.table=table};
    call.begin=fd_event(AP_FD_TABLE_PUT_BEGIN,table,-1,0,0,0,0,0);
    if(!call.begin)return 0;
    if(update(&fd_puts,&key,&call,BPF_NOEXIST))fd_problem(AP_FD_DUPLICATE);
    return 0;
}
SEC("fexit/put_files_struct") int fd_table_put_returned(u64 *ctx) {
    struct ap_invocation_key key=invocation(0,ctx[0]);struct ap_fd_put_call *call=lookup(&fd_puts,&key);
    if(!call)return 0;
    /* No table dereference or current pointer-map lookup here: another task
     * can retire and reuse a nonfinal put's allocation before this return. */
    if(!call->begin) { fd_problem(AP_FD_MISSING);return 0; }
    if(!fd_event(AP_FD_TABLE_PUT_END,call->table,-1,0,0,
                 call->retired?call->retired:call->begin,0,call->retired?1:0))return 0;
    remove_key(&fd_puts,&key);return 0;
}
/* The exact kernel's zero-reference path calls every filp_close, frees any
 * separate arrays, then tail-jumps here. The allocator entry is before reuse;
 * the saved invocation and generation identify the completed table drain. */
SEC("fentry/kmem_cache_free") int fd_table_retired(u64 *ctx) {
    u64 raw=ctx[1];struct ap_fd_table *table=lookup(&fd_tables,&raw);
    struct ap_fd_file *file=lookup(&fd_files,&raw);
    /* fput_close_sync inlines both normal and backing-file final release on
     * the qualified kernel. This is its actual allocation being freed, joined
     * to the typed generation minted from a held struct file, not a guess from
     * a close result, cache name, or numeric descriptor. No key means no fact.
     * The existing __fput hook may already have retired this exact generation. */
    if(file) {
        if(!table)return fd_retire_file(raw);
        /* One live allocation cannot be both a file and a files_struct.
         * Refuse the contradiction, discard the stale file key before reuse,
         * and preserve the original table drain/retirement path below. */
        fd_problem(AP_FD_IDENTITY);
        if(remove_key(&fd_files,&raw))fd_problem(AP_FD_MISSING);
    }
    if(!table)return 0;
    if(!table->enrolled) {
        /* A normalized-only auxiliary table has no guest journal interval.
         * Retire its pointer generation before actual allocator reuse. */
        if(remove_key(&fd_tables,&raw))fd_problem(AP_FD_MISSING);
        return 0;
    }
    struct ap_invocation_key key=invocation(0,raw);struct ap_fd_put_call *call=lookup(&fd_puts,&key);
    u64 dependency=0;
    if(!call || !call->begin || call->table!=table->identity || call->retired)
        fd_problem(AP_FD_UNKNOWN_TABLE);
    else dependency=call->begin;
    u64 retired=fd_event(AP_FD_TABLE_RETIRED,table->identity,-1,0,0,dependency,0,0);
    if(call && dependency)call->retired=retired;
    /* Even a failed observation retires this pointer key before actual reuse;
     * the sticky error and incomplete interval cannot authorize the consumer. */
    remove_key(&fd_tables,&raw);return 0;
}

/* Both hooks are inside the real kernel's ptrace-freeze interval. The child
 * pointer is selected by ptrace and must also carry the exact pidfd-installed
 * TASK_STORAGE command. No numeric/proc lookup creates identity here. */
INLINE u64 fd_target_task(struct task_struct *task) {
    return ((u64)(u32)CORE(task->tgid)<<32)|(u32)CORE(task->pid);
}
INLINE u64 fd_enrollment_event(struct ap_fd_enrollment *e,u64 kind,s32 fd,u64 file,s32 returned) {
    return fd_event_for(e->task,e->task_start,kind,e->table,fd,file,0,
                        kind==AP_FD_ENROLL_BEGIN?0:e->begin,e->command,returned);
}
/* No new observation owner: borrow the same held census file. Unknown
 * dispatch is conservative; a failed required kernel read invalidates census.
 * Classic images have no authenticated anchor and cannot issue this proof. */
static __attribute__((noinline)) int fd_enrollment_source_ioctl(
        struct file *file,struct inode *inode,u32 mode,u32 device,u64 *dispatch) {
    *dispatch=AP_SOURCE_IOCTL_DISPATCH_UNKNOWN;
#ifdef AP_GROUPED_PROVIDER
    u32 zero=0;struct ap_config *config=lookup(&ap_config_map,&zero);
    if(!config || config->anchor_phase!=AP_GROUPED_ANCHOR_ACTIVE ||
       !config->anchor_task || !config->anchor_start || !config->anchor_ip)return 0;
    u32 kind=mode&0170000;
    if(kind!=0100000 && !(kind==0020000 &&
       ap_fd_device_major(device)==1 && ap_fd_device_minor(device)==3))return 0;
    const struct file_operations *fops=0,*inode_fops=0;u64 unlocked_ioctl=0,filesystem=0;
    if(fd_read_kernel(&fops,sizeof(fops),CORE(&file->f_op)) || !fops ||
       fd_read_kernel(&unlocked_ioctl,sizeof(unlocked_ioctl),CORE(&fops->unlocked_ioctl)))return -1;
    if(kind==0100000) {
        struct super_block *sb=0;
        if(fd_read_kernel(&inode_fops,sizeof(inode_fops),CORE(&inode->i_fop)) ||
           fd_read_kernel(&sb,sizeof(sb),CORE(&inode->i_sb)) || !sb ||
           fd_read_kernel(&filesystem,sizeof(filesystem),CORE(&sb->s_magic)))return -1;
    }
    *dispatch=ap_fd_source_ioctl_dispatch(config->anchor_ip,(u64)fops,unlocked_ioctl,
        (u64)inode_fops,filesystem,mode,ap_fd_device_major(device),ap_fd_device_minor(device));
#endif
    return 0;
}
SEC("fentry/ptrace_request") int fd_enrollment_enter(u64 *ctx) {
    struct task_struct *task=(struct task_struct *)ctx[0];
    struct ap_task_command *c=authenticated_command(task_storage(&tasks,task,0,0));
    if(!c || c->operation!=AP_TABLE_ENROLLMENT || ctx[1]!=AP_PTRACE_GETREGSET || ctx[2]!=AP_NT_PRSTATUS)return 0;
    struct ap_command_result *r=result(c->command);
    u32 slot=ap_command_slot(c->command);
    struct ap_fd_enrollment *e=lookup(&fd_enrollments,&slot);
    if(!ap_command_reservation_matches(c,r,slot) || !e || e->command!=c->command || e->phases ||
       __sync_val_compare_and_swap(&r->phase,AP_COMMAND_READY,AP_COMMAND_RUNNING)!=AP_COMMAND_READY) {
        ap_fail(AP_DUPLICATE);return 0;
    }
    e->task=fd_target_task(task);e->task_start=CORE(task->start_boottime);
    e->phases=AP_ENROLL_ENTERED;
    r->task=e->task;r->start_boottime=e->task_start;r->identity.provider=incarnation();
    struct files_struct *files=CORE(task->files);
    e->references=files?CORE(files->count.counter):0;
    if(!files || !ap_fd_enrollment_context(CORE(task->__state),CORE(task->jobctl),CORE(task->ptrace),
          CORE(task->parent)==current_task(),e->references,!e->expected_table) ||
       c->expected_level!=AP_PTRACE_GETREGSET || c->expected_option!=AP_NT_PRSTATUS) {
        e->problem|=AP_FD_IDENTITY;return 0;
    }
    u64 known=fd_table(files,0);
    if(e->expected_table) {
        if(known!=e->expected_table) { e->problem|=AP_FD_IDENTITY;return 0; }
        e->mode=AP_ENROLL_KNOWN_TABLE;e->table=known;
    } else {
        if(known) { e->problem|=AP_FD_DUPLICATE;return 0; }
        e->mode=AP_ENROLL_NEW_TABLE;e->table=fd_table(files,AP_TABLE_CENSUS_ENROLL);
    }
    if(!e->table) { e->problem|=AP_FD_MISSING;return 0; }
    e->begin=fd_enrollment_event(e,AP_FD_ENROLL_BEGIN,-1,0,0);
    if(!e->begin) { e->problem|=AP_FD_MISSING;return 0; }
    if(e->mode==AP_ENROLL_NEW_TABLE) {
        /* count1 plus the actual non-wakeable ptrace freeze excludes another
         * table owner and target execution throughout the full scan. Every
         * nonnull slot owns its file reference; holes are read, not omitted. */
        struct fdtable *fdt=CORE(files->fdt);
        if(!fdt) { e->problem|=AP_FD_MISSING;return 0; }
        u32 slots=CORE(fdt->max_fds);
        if(!slots || slots>AP_FD_FILES) { e->problem|=AP_FD_CAPACITY;return 0; }
        struct file **array=CORE(fdt->fd);unsigned long *cloexec=CORE(fdt->close_on_exec);
        if(!array || !cloexec) { e->problem|=AP_FD_MISSING;return 0; }
        e->slots=slots;
        for(u32 fd=0;fd<AP_FD_FILES;fd++) {
            if(fd>=slots)break;
            struct file *file=0;unsigned long flags=0;
            if(fd_read_kernel(&file,sizeof(file),array+fd) ||
               fd_read_kernel(&flags,sizeof(flags),cloexec+fd/64)) { e->problem|=AP_FD_MISSING;return 0; }
            if(!file)continue;
            struct inode *inode=0;unsigned short mode=0;u32 status_flags=0;
            if(fd_read_kernel(&inode,sizeof(inode),CORE(&file->f_inode)) || !inode ||
               fd_read_kernel(&mode,sizeof(mode),CORE(&inode->i_mode)) ||
               fd_read_kernel(&status_flags,sizeof(status_flags),CORE(&file->f_flags)) ||
               !ap_fd_profile_valid(mode)) { e->problem|=AP_FD_MISSING;return 0; }
            u32 device=0,kind=mode & 0170000;
            if((kind==0020000 || kind==0060000) &&
               fd_read_kernel(&device,sizeof(device),CORE(&inode->i_rdev))) {
                e->problem|=AP_FD_MISSING;return 0;
            }
            u64 dispatch=0;
            if(fd_enrollment_source_ioctl(file,inode,mode,device,&dispatch)) {
                e->problem|=AP_FD_MISSING;return 0;
            }
            u64 identity=fd_file(file);
            if(!identity || !fd_event_for_profile(e->task,e->task_start,AP_FD_ENROLL_SLOT,e->table,
                 (s32)fd,identity,0,e->begin,e->command,(flags>>(fd%64))&1,mode,status_flags,ap_fd_device_major(device),ap_fd_device_minor(device),dispatch)) {
                e->problem|=AP_FD_MISSING;return 0;
            }
            e->files++;
        }
    }
    e->end=fd_enrollment_event(e,AP_FD_ENROLL_END,e->mode==AP_ENROLL_NEW_TABLE?(s32)e->slots:-1,0,(s32)e->files);
    if(!e->end) { e->problem|=AP_FD_MISSING;return 0; }
    e->phases|=AP_ENROLL_CENSUS;return 0;
}
SEC("fexit/ptrace_request") int fd_enrollment_returned(u64 *ctx) {
    struct task_struct *task=(struct task_struct *)ctx[0];
    struct ap_task_command *c=authenticated_command(task_storage(&tasks,task,0,0));
    if(!c || c->operation!=AP_TABLE_ENROLLMENT || ctx[1]!=AP_PTRACE_GETREGSET || ctx[2]!=AP_NT_PRSTATUS)return 0;
    struct ap_command_result *r=result(c->command);u32 slot=ap_command_slot(c->command);
    struct ap_fd_enrollment *e=lookup(&fd_enrollments,&slot);
    if(!e || !r || e->command!=c->command || r->phase!=AP_COMMAND_RUNNING ||
       e->task!=fd_target_task(task) || e->task_start!=CORE(task->start_boottime) ||
       !(e->phases&AP_ENROLL_ENTERED) || (e->phases&AP_ENROLL_RETURNED)) { ap_fail(AP_BAD_COMMAND);return 0; }
    if(e->table && (fd_table(CORE(task->files),0)!=e->table ||
       !ap_fd_enrollment_context(CORE(task->__state),CORE(task->jobctl),CORE(task->ptrace),
          CORE(task->parent)==current_task(),CORE(task->files->count.counter),!e->expected_table)))
        e->problem|=AP_FD_IDENTITY;
    e->ptrace_return=(s32)ctx[4];r->returned=e->ptrace_return;
    e->phases|=AP_ENROLL_RETURNED;
    /* The native GETREGSET result is independent of successful census. Errors
     * remain exact negative receipts, never admission. Final producer access. */
    publish_result(r);return 0;
}
