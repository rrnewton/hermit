/* SPDX-License-Identifier: GPL-2.0 */
#include "fd-call-shared.bpf.h"
/* Included after the shared task/command and fd_calls helpers. No new map,
 * file reference, guest-memory read, or scheduling operation is introduced. */
/* AUTONOMOUS-BOT-IMPLEMENTED: original epoll_ctl observation, same Call.
 * TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/3174).
 * No pre-copy FD exclusion: the actual copied event precedes both native
 * selections. These callbacks retain observations, never issue fdget or ctl. */
static __attribute__((noinline)) int fd_original_epoll_publish(
    const struct ap_task_command *c,struct ap_fd_call *fresh) {
    struct ap_command_result *r=claim_result(c);if(!r)return 0;
    struct ap_invocation_key key=fd_actor();struct task_struct *task=current_task();
    struct ap_fd_status *status=fd_stats();
    fresh->raw_table=(u64)CORE(task->files);fresh->new_file=(u64)CORE(task->mm);
    struct ap_original_selection *s=&fresh->original.selection;
    s->provider=incarnation();s->task=key.task;s->task_start=key.start;
    s->table=fd_table((struct files_struct *)fresh->raw_table,1);
    if(!status || status->problem || s->provider!=c->provider || !s->provider ||
       !s->task || !s->task_start || !s->table || !fresh->raw_table || !fresh->new_file) {
        fd_problem(AP_FD_IDENTITY);return 0;
    }
    fresh->original.epoll_ctl.before=status->next_event;
    if(update(&fd_calls,&key,fresh,BPF_NOEXIST)) {fd_problem(AP_FD_DUPLICATE);return 0;}
    r->identity.provider=s->provider;return 0;
}
static __attribute__((noinline)) int fd_original_epoll_entered(
    const struct ap_task_command *c) {
    struct ap_fd_call fresh;ap_fd_call_clear(&fresh);
    fresh.command=c->command;
    fresh.operation=AP_ORIGINAL_EPOLL_CTL;
    fresh.original.selection.command=c->command;fresh.original.selection.call=c->expected_object;
    fresh.original.selection.owner_mm=c->generation_after;
    fresh.original.selection.requested_fd=c->expected_level;
    fresh.original.selection.address_length=c->expected_option;
    fresh.original.selection.user_address=c->generation_before;
    fresh.original.selection.original_count=c->original_count;
    fresh.original.epoll_ctl.entered=1;
    fresh.original.epoll_ctl.image_wakeup_policy=AP_EPOLL_IMAGE_PM_SLEEP_DISABLED;
    return fd_original_epoll_publish(c,&fresh);
}
static __attribute__((noinline)) int fd_original_epoll_context(
    const struct ap_task_command *c,struct ap_fd_call *call,
    const struct ap_command_result *r,const struct ap_invocation_key *key) {
    struct task_struct *task=current_task();
    return c && call && r && call->operation==AP_ORIGINAL_EPOLL_CTL && call->command==c->command &&
        ap_original_epoll_ctl_command(c) && r->command==c->command && r->operation==c->operation &&
        r->phase==AP_COMMAND_RUNNING && r->identity.provider==c->provider &&
        r->original_count==c->original_count && r->task==key->task && r->start_boottime==key->start &&
        call->raw_table==(u64)CORE(task->files) && call->new_file==(u64)CORE(task->mm) &&
        call->original.selection.command==c->command && call->original.selection.call==c->expected_object &&
        call->original.selection.owner_mm==c->generation_after && call->original.selection.provider==c->provider &&
        call->original.selection.task==key->task && call->original.selection.task_start==key->start &&
        call->original.selection.table && call->original.selection.requested_fd==c->expected_level &&
        call->original.selection.address_length==c->expected_option &&
        call->original.selection.user_address==c->generation_before &&
        call->original.selection.original_count==c->original_count &&
        call->original.epoll_ctl.entered==1 && !call->original.problem && !call->original.complete;
}
static __attribute__((noinline)) int fd_original_epoll_session(struct pt_regs *ctx,
    const struct ap_task_command *c) {
    bool returning=bpf_session_is_return(ctx);struct ap_invocation_key key;
    struct ap_fd_call *call=fd_actor_call(&key);struct ap_command_result *r=result(c->command);
    u64 *cookie=bpf_session_cookie(ctx);
    if(!cookie || !fd_original_epoll_context(c,call,r,&key)) {fd_problem(AP_FD_IDENTITY);return !returning;}
    struct ap_original_epoll_ctl *e=&call->original.epoll_ctl;
    if(!returning) {
        u64 caller=0,stack=CORE(ctx->sp),ip=fd_function_ip(ctx);
        if(e->ctl_entered || e->ctl_returned || call->original.selection.ready || !stack ||
           fd_read_kernel(&caller,sizeof(caller),(void *)stack) || !ap_epoll_ctl_caller_site(ip,caller) ||
           stack>~0ULL-8 || CORE(ctx->cx)!=stack+8 || CORE(ctx->r8) ||
           (s32)CORE(ctx->di)!=c->expected_level || (s32)CORE(ctx->si)!=c->expected_option ||
           (u64)(u32)CORE(ctx->dx)!=c->original_count) {
            call->original.problem|=AP_FD_IDENTITY;return 1;
        }
        /* DEL has no input event access. For every other op this exact direct
         * successor proves Linux completed all twelve input bytes first. */
        if(c->expected_option!=AP_EPOLL_CTL_DEL && fd_read_kernel(e->event,sizeof(e->event),(void *)(stack+8))) {
            call->original.problem|=AP_FD_MISSING;return 1;
        }
        call->function_ip=ip;call->entry_stack=stack;e->ctl_entered=1;*cookie=c->command;return 0;
    }
    s64 returned=(s32)CORE(ctx->ax);
    if(*cookie!=c->command || e->ctl_entered!=1 || e->ctl_returned ||
       !call->function_ip || !call->entry_stack || !ap_original_epoll_ctl_selected(c,&call->original) ||
       returned< -4095 || returned>0 ||
       ((!call->original.selection.file || !e->target_file) && returned!=-9)) {
        call->original.problem|=AP_FD_OUTCOME;return 0;
    }
    e->ctl_result=(s32)returned;e->ctl_returned=1;return 0;
}
static __attribute__((noinline)) int fd_original_epoll_apply_post(
        struct ap_fd_call *,const struct ap_task_command *,u64,u64);
#ifndef AP_FTRACE_PROVIDER
static __attribute__((noinline)) int fd_original_epoll_post(struct pt_regs *ctx) {
    struct ap_task_command *c=command();if(!c || c->operation!=AP_ORIGINAL_EPOLL_CTL)return 0;
    struct ap_invocation_key key;struct ap_fd_call *call=fd_actor_call(&key);
    struct ap_command_result *r=result(c->command);
    if(!fd_original_epoll_context(c,call,r,&key)) {fd_problem(AP_FD_IDENTITY);return 0;}
    u64 cookie=fd_attach_cookie(ctx),word=CORE(ctx->ax);
    if(!ap_original_epoll_post_site(call,c,cookie,CORE(ctx->ip),CORE(ctx->bp),
          CORE(ctx->bx),CORE(ctx->r13),CORE(ctx->r12),word)) {
        call->original.problem|=AP_FD_IDENTITY;return 0;
    }
    const u64 role=cookie==AP_EPOLL_PRIMARY_POST_COOKIE?
        AP_SHARED_FDGET_EPOLL_PRIMARY_ROLE:AP_SHARED_FDGET_EPOLL_TARGET_ROLE;
    return fd_original_epoll_apply_post(call,c,role,word);
}
#endif
static __attribute__((noinline)) int fd_original_epoll_apply_post(
        struct ap_fd_call *call,const struct ap_task_command *c,u64 role,u64 word) {
    if(!ap_original_epoll_fdget_role_ready(call,c,role,word)) {
        call->original.problem|=AP_FD_IDENTITY;return 0;
    }
    struct ap_fd_status *status=fd_stats();
    if(!status || status->problem) {call->original.problem|=AP_FD_MISSING;return 0;}
    struct file *file=(struct file *)(word&~3ULL);u64 identity=file?fd_file(file):0;
    if(file && !identity) {call->original.problem|=AP_FD_MISSING;return 0;}
    struct ap_original_epoll_ctl *e=&call->original.epoll_ctl;
    if(role==AP_SHARED_FDGET_EPOLL_PRIMARY_ROLE) {
        call->original.selection.file=identity;call->original.selection.fdput_flags=word&1;
        e->primary_cut=status->next_event;e->primary_selected=1;
        if(file)return 0; /* Actual second selection has not happened yet. */
    } else {
        e->target_file=identity;e->target_flags=word&1;e->target_cut=status->next_event;e->secondary_selected=1;
    }
    /* Original fdget references are still borrowed. This is the publication
     * of the immutable pair (or positive primary-empty, second-not-reached). */
    if(__sync_val_compare_and_swap(&call->original.selection.ready,0,1)!=0)
        call->original.problem|=AP_FD_DUPLICATE;
    return 0;
}
static __attribute__((noinline)) int fd_original_epoll_returned(u64 *ctx,const struct ap_task_command *c) {
    struct pt_regs *regs=(struct pt_regs *)ctx[0];
    if(!regs || (s64)CORE(regs->orig_ax)!=AP_EPOLL_CTL_SYSCALL)return 0;
    struct ap_invocation_key key;struct ap_fd_call *call=fd_actor_call(&key);
    struct ap_command_result *r=result(c->command);
    /* A suppressed sys_enter supplies no original invocation. READY remains
     * owned; actual sys_exit errno alone is never descriptor-selection proof. */
    if(!call && ap_command_reservation_matches(c,r,ap_command_slot(c->command)) &&
       ap_original_epoll_ctl_operands(c,CORE(regs->orig_ax),CORE(regs->cs),CORE(regs->di),
           CORE(regs->si),CORE(regs->dx),CORE(regs->r10))) {
        if(!ap_command_ready_payload_empty(r))fd_problem(AP_FD_OUTCOME);
        return 0;
    }
    s64 returned=(s64)ctx[1];
    if(!ap_original_epoll_ctl_operands(c,CORE(regs->orig_ax),CORE(regs->cs),CORE(regs->di),
          CORE(regs->si),CORE(regs->dx),CORE(regs->r10)) ||
       !fd_original_epoll_context(c,call,r,&key) || returned< -4095 || returned>0) {
        fd_problem(AP_FD_OUTCOME);return 0;
    }
    struct ap_original_epoll_ctl *e=&call->original.epoll_ctl;
    if(!e->ctl_entered) {
        /* The authenticated entered nr233 dispatch on this exact image has
         * only copy_from_user failure before do_epoll_ctl. This actual -EFAULT
         * result reports no descriptor selection, including no empty fdget. */
        if(c->expected_option==AP_EPOLL_CTL_DEL || returned!=-14 || e->ctl_returned ||
           e->primary_selected || e->secondary_selected || call->original.selection.ready ||
           call->original.selection.file || e->target_file || !ap_original_epoll_event_empty(e)) {
            call->original.problem|=AP_FD_OUTCOME;return 0;
        }
        if(__sync_val_compare_and_swap(&call->original.selection.ready,0,1)!=0) {
            call->original.problem|=AP_FD_DUPLICATE;return 0;
        }
    } else if(e->ctl_returned!=1 || e->ctl_result!=returned || !ap_original_epoll_ctl_selected(c,&call->original)) {
        call->original.problem|=AP_FD_OUTCOME;return 0;
    }
    call->original.returned=(s32)returned;call->original.complete=1;r->returned=(s32)returned;
    publish_result(r);return 0;
}

static __attribute__((noinline)) int fd_epoll_ctl_entered(struct pt_regs *regs,
    const struct ap_task_command *c) {
    struct ap_command_result *r=claim_result(c);if(!r)return 0;
    r->identity.provider=c->provider;
    struct ap_invocation_key key=fd_actor();
    struct task_struct *task=current_task();
    u64 table=fd_table(CORE(task->files),1);
    struct ap_fd_call fresh;ap_fd_call_clear(&fresh);
    fresh.command=c->command;
    fresh.operation=AP_EPOLL_CTL_COPY;
    fresh.raw_table=(u64)CORE(task->files);
    fresh.new_file=(u64)CORE(task->mm);
    fresh.epoll_copy.command=c->command;fresh.epoll_copy.call=c->expected_object;
    fresh.epoll_copy.owner_mm=c->generation_after;fresh.epoll_copy.provider=c->provider;
    fresh.epoll_copy.task=key.task;fresh.epoll_copy.task_start=key.start;fresh.epoll_copy.table=table;
    fresh.epoll_copy.user_address=CORE(regs->r10);fresh.epoll_copy.op=(s32)CORE(regs->si);
    fresh.epoll_copy.entered=1;fresh.epoll_copy.image_wakeup_policy=AP_EPOLL_IMAGE_PM_SLEEP_DISABLED;
    if(!table || !fresh.raw_table || !fresh.new_file || !ap_epoll_ctl_identity(c,&fresh.epoll_copy))
        fresh.epoll_copy.problem=AP_FD_IDENTITY;
    if(update(&fd_calls,&key,&fresh,BPF_NOEXIST))fd_problem(AP_FD_DUPLICATE);
    return 0;
}
SEC("tp_btf/sys_enter") int fd_epoll_ctl_syscall_entered(u64 *ctx) {
    struct ap_task_command *c=command();if(!c)return 0;
    if(ap_original_recv(c->operation))return fd_original_recv_syscall_entered(ctx,c);
    if(ap_stream_tx_operation(c->operation))return fd_original_sendto_syscall_entered(ctx,c);
    if(c->operation!=AP_EPOLL_CTL_COPY && c->operation!=AP_ORIGINAL_EPOLL_CTL)return 0;
    struct pt_regs *regs=(struct pt_regs *)ctx[0];
    if((s64)ctx[1]!=AP_EPOLL_CTL_SYSCALL)return 0; /* unrelated private syscall */
    if(c->operation==AP_ORIGINAL_EPOLL_CTL) {
        if(!regs || (s64)CORE(regs->orig_ax)!=(s64)ctx[1] ||
           !ap_original_epoll_ctl_operands(c,(s64)ctx[1],CORE(regs->cs),CORE(regs->di),CORE(regs->si),CORE(regs->dx),CORE(regs->r10))) {
            fd_problem(AP_FD_IDENTITY);return 0;
        }
        return fd_original_epoll_entered(c);
    }
    if(!regs || (s64)CORE(regs->orig_ax)!=(s64)ctx[1] ||
       !ap_epoll_ctl_operands(c,(s64)ctx[1],CORE(regs->cs),CORE(regs->di),
                             CORE(regs->si),CORE(regs->dx),CORE(regs->r10))) {
        fd_problem(AP_FD_IDENTITY);return 0;
    }
    return fd_epoll_ctl_entered(regs,c);
}
static __attribute__((noinline)) int fd_epoll_ctl_session(struct pt_regs *ctx) {
    bool returning=bpf_session_is_return(ctx);
    struct ap_task_command *c=command();
    if(c && c->operation==AP_ORIGINAL_EPOLL_CTL)return fd_original_epoll_session(ctx,c);
    if(!c || c->operation!=AP_EPOLL_CTL_COPY)return !returning;
    struct ap_invocation_key key;struct ap_fd_call *call=fd_actor_call(&key);
    struct ap_command_result *r=result(c->command);
    u64 *cookie=bpf_session_cookie(ctx);
    if(!call || call->operation!=AP_EPOLL_CTL_COPY || call->command!=c->command || !cookie ||
       !r || r->phase!=AP_COMMAND_RUNNING || r->task!=key.task || r->start_boottime!=key.start ||
       call->raw_table!=(u64)CORE(current_task()->files) || call->new_file!=(u64)CORE(current_task()->mm) ||
       !ap_epoll_ctl_identity(c,&call->epoll_copy) || call->epoll_copy.problem || call->epoll_copy.complete) {
        fd_problem(AP_FD_IDENTITY);return !returning;
    }
    struct ap_epoll_ctl_copy *copy=&call->epoll_copy;
    if(!returning) {
        u64 caller=0,stack=CORE(ctx->sp),ip=fd_function_ip(ctx);
        /* Session entry_data is uninitialized scratch. Authenticate this
         * invocation independently, then initialize its return cookie below. */
        if(copy->ctl_entered || copy->ctl_returned || !stack ||
           fd_read_kernel(&caller,sizeof(caller),(void *)stack) ||
           !ap_epoll_ctl_caller_site(ip,caller) || stack>~0ULL-8 || CORE(ctx->cx)!=stack+8 ||
           (s32)CORE(ctx->di)!=-1 || (s32)CORE(ctx->si)!=copy->op || (s32)CORE(ctx->dx)!=-1 ||
           CORE(ctx->r8)) {copy->problem|=AP_FD_IDENTITY;return 1;}
        /* This exact caller reaches do_epoll_ctl only after all12 native
         * uaccess bytes succeeded, or DEL skipped uaccess altogether. */
        if(copy->op!=AP_EPOLL_CTL_DEL && fd_read_kernel(copy->event,sizeof(copy->event),(void *)(stack+8))) {
            copy->problem|=AP_FD_MISSING;return 1;
        }
        call->function_ip=ip;call->entry_stack=stack;
        copy->ctl_entered=1;*cookie=c->command;return 0;
    }
    if(*cookie!=c->command || copy->ctl_entered!=1 || copy->ctl_returned ||
       !call->function_ip || !call->entry_stack || (s32)CORE(ctx->ax)!=-9) {
        copy->problem|=AP_FD_OUTCOME;return 0;
    }
    copy->ctl_returned=1;return 0;
}
static __attribute__((noinline)) int fd_epoll_ctl_returned(u64 *ctx,const struct ap_task_command *c) {
    struct pt_regs *regs=(struct pt_regs *)ctx[0];
    if(!regs || (s64)CORE(regs->orig_ax)!=AP_EPOLL_CTL_SYSCALL)return 0;
    struct ap_invocation_key key;struct ap_fd_call *call=fd_actor_call(&key);
    struct ap_command_result *r=result(c->command);
    /* sys_exit also runs when seccomp suppressed sys_enter. An untouched
     * reservation with no entered row remains uncompleted: neither this exit
     * nor its raw return value proves a copy or authorizes cancellation. */
    if(!call && ap_epoll_ctl_operands(c,CORE(regs->orig_ax),CORE(regs->cs),CORE(regs->di),
                                     CORE(regs->si),CORE(regs->dx),CORE(regs->r10)) &&
       ap_command_reservation_matches(c,r,ap_command_slot(c->command))) {
        /* Require the whole canonical READY payload, including unused bytes.
         * Claimed, stale, or malformed state must still poison/refuse below. */
        if(!ap_command_ready_payload_empty(r))fd_problem(AP_FD_OUTCOME);
        return 0;
    }
    if(!ap_epoll_ctl_operands(c,CORE(regs->orig_ax),CORE(regs->cs),CORE(regs->di),
                             CORE(regs->si),CORE(regs->dx),CORE(regs->r10)) ||
       !call || call->operation!=AP_EPOLL_CTL_COPY || call->command!=c->command ||
       call->raw_table!=(u64)CORE(current_task()->files) || call->new_file!=(u64)CORE(current_task()->mm) ||
       !r || r->phase!=AP_COMMAND_RUNNING || r->task!=key.task || r->start_boottime!=key.start ||
       !ap_epoll_ctl_identity(c,&call->epoll_copy) || !ap_epoll_ctl_outcome(&call->epoll_copy,(s64)ctx[1])) {
        fd_problem(AP_FD_OUTCOME);return 0;
    }
    call->epoll_copy.returned=(s32)ctx[1];call->epoll_copy.complete=1;
    r->returned=(s32)ctx[1];
    /* All copied bytes and session publications precede the existing final
     * command release. No callback accesses either row after DONE. */
    publish_result(r);return 0;
}
