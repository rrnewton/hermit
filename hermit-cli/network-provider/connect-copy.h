/* SPDX-License-Identifier: MIT */
#ifndef HERMIT_ORIGINAL_CONNECT_COPY_H
#define HERMIT_ORIGINAL_CONNECT_COPY_H
#include "fd-effects.h"
#include "retirement-target.h"
static __attribute__((always_inline)) inline u64 ap_shared_fdget_role(
        u64 operation,u64 function_ip,u64 return_ip) {
    if(!function_ip || !return_ip)return 0;
    if(operation==AP_ORIGINAL_CONNECT) {
        const u64 role=function_ip<=~0ULL-AP_CONNECT_FDGET_RETURN_OFFSET &&
            return_ip==function_ip+AP_CONNECT_FDGET_RETURN_OFFSET?AP_SHARED_FDGET_CONNECT_ROLE:0;
        return ap_ftrace_role_enabled(role)?role:0;
    }
    if(operation==AP_ACCEPT_EFFECT) {
        const u64 role=function_ip<=~0ULL-AP_ACCEPT_FDGET_RETURN_OFFSET &&
            return_ip==function_ip+AP_ACCEPT_FDGET_RETURN_OFFSET?AP_SHARED_FDGET_ACCEPT_ROLE:0;
        return ap_ftrace_role_enabled(role)?role:0;
    }
    if(operation==AP_ORIGINAL_EPOLL_CTL) {
        if(function_ip<=~0ULL-AP_EPOLL_PRIMARY_POST_OFFSET &&
           return_ip==function_ip+AP_EPOLL_PRIMARY_POST_OFFSET)
            return ap_ftrace_role_enabled(AP_SHARED_FDGET_EPOLL_PRIMARY_ROLE)?
                AP_SHARED_FDGET_EPOLL_PRIMARY_ROLE:0;
        if(function_ip<=~0ULL-AP_EPOLL_TARGET_POST_OFFSET &&
           return_ip==function_ip+AP_EPOLL_TARGET_POST_OFFSET)
            return ap_ftrace_role_enabled(AP_SHARED_FDGET_EPOLL_TARGET_ROLE)?
                AP_SHARED_FDGET_EPOLL_TARGET_ROLE:0;
    }
    return 0;
}
static __attribute__((always_inline)) inline int ap_original_epoll_fdget_role_ready(
    const struct ap_fd_call *call,const struct ap_task_command *c,u64 role,u64 word) {
    if(!call || !ap_original_epoll_ctl_command(c) || !call->function_ip || !call->entry_stack ||
       call->original.epoll_ctl.ctl_entered!=1 || call->original.epoll_ctl.ctl_returned ||
       call->original.selection.ready || (word&2) || ((word&3) && !(word&~3ULL)))return 0;
    if(role==AP_SHARED_FDGET_EPOLL_PRIMARY_ROLE)
        return !call->original.epoll_ctl.primary_selected &&
            !call->original.epoll_ctl.secondary_selected && !call->original.selection.file &&
            !call->original.epoll_ctl.target_file;
    return role==AP_SHARED_FDGET_EPOLL_TARGET_ROLE &&
        call->original.epoll_ctl.primary_selected==1 &&
        !call->original.epoll_ctl.secondary_selected && call->original.selection.file &&
        !call->original.epoll_ctl.target_file;
}
/* Pure predicate shared by actual post-fdget callbacks and controls. Saved
 * RBP is the actual copied-event pointer; no guessed trampoline SP delta. */
static __attribute__((always_inline)) inline int ap_original_epoll_post_site(
    const struct ap_fd_call *call,const struct ap_task_command *c,u64 cookie,
    u64 ip,u64 event,u64 op,u64 fd,u64 nonblock,u64 word) {
    if(!call || !ap_original_epoll_ctl_command(c) || !call->function_ip || !call->entry_stack ||
       call->entry_stack>~0ULL-8 || event!=call->entry_stack+8 ||
       op!=(u64)(u32)c->expected_option || fd!=c->original_count || nonblock ||
       call->original.epoll_ctl.ctl_entered!=1 || call->original.epoll_ctl.ctl_returned)return 0;
    u64 offset,role;
    if(cookie==AP_EPOLL_PRIMARY_POST_COOKIE) {
        offset=AP_EPOLL_PRIMARY_POST_OFFSET;role=AP_SHARED_FDGET_EPOLL_PRIMARY_ROLE;
    } else if(cookie==AP_EPOLL_TARGET_POST_COOKIE) {
        offset=AP_EPOLL_TARGET_POST_OFFSET;role=AP_SHARED_FDGET_EPOLL_TARGET_ROLE;
    } else return 0;
    return ap_original_epoll_fdget_role_ready(call,c,role,word) &&
        call->function_ip<=~0ULL-offset-1 && ip==call->function_ip+offset+1;
}
/* Exact original-entry and post-fdget receipts, shared by both wrappers.
 * The selected word cannot be invented at entry, and the post callback cannot
 * create a missing entry. Caller-saved RDI is deliberately not a post input. */
struct ap_selection_physical {u64 task,start,raw_table,table;};
#ifdef __BPF__
static __attribute__((noinline)) int
#else
static __attribute__((always_inline)) inline int
#endif
ap_fdget_context_shared(const struct ap_fd_call *call,const struct ap_task_command *owner,
    const struct ap_fd_accept *accept,const struct ap_selection_physical *p) {
    if(!p)return 0;
    const u64 task=p->task,start=p->start,raw_table=p->raw_table,table=p->table;
    if(!call || !owner || owner->original_count || !owner->provider || !owner->command || !task || !start ||
       !raw_table || !table || !call->function_ip || call->command!=owner->command ||
       call->operation!=owner->operation || call->raw_table!=raw_table)return 0;
    if(call->operation==AP_ACCEPT_EFFECT) {
        return accept && accept->command==owner->command && !accept->problem &&
            accept->task==task && accept->task_start==start && accept->table==table &&
            accept->phases==AP_FD_ENTERED && accept->requested_fd==owner->expected_level &&
            accept->flags==owner->expected_option;
    }
    if(call->operation!=AP_ORIGINAL_CONNECT || accept)return 0;
    const struct ap_original_selection *s=&call->original.selection;
    return !call->original.problem && !call->original.complete && !s->ready &&
        s->command==owner->command && s->call==owner->expected_object && s->call &&
        s->owner_mm==owner->generation_after && s->provider==owner->provider &&
        s->task==task && s->task_start==start && s->table==table &&
        s->requested_fd==owner->expected_level && s->address_length==owner->expected_option &&
        s->user_address==owner->generation_before;

}
static __attribute__((always_inline)) inline int ap_fdget_context(
    const struct ap_fd_call *call,const struct ap_task_command *owner,
    const struct ap_fd_accept *accept,u64 task,u64 start,u64 raw_table,u64 table) {
    const struct ap_selection_physical p={task,start,raw_table,table};
    return ap_fdget_context_shared(call,owner,accept,&p);
}
static __attribute__((always_inline)) inline int ap_fdget_entry(
    struct ap_fd_call *call,const struct ap_task_command *owner,
    const struct ap_fd_accept *accept,u64 task,u64 start,u64 raw_table,u64 table,
    u64 cookie,u64 stack) {
    if(!ap_fdget_context(call,owner,accept,task,start,raw_table,table) ||
       !stack || call->entry_stack ||
       cookie!=(call->operation==AP_ACCEPT_EFFECT?AP_ACCEPT_ENTRY_COOKIE:AP_CONNECT_ENTRY_COOKIE) ||
       !ap_fd_selection_enter(&call->selection))return 0;
    call->entry_stack=stack;return 1;
}
/* The exact supported x86 image co-attaches an fexit trampoline to each
 * original syscall. Its normal JMP entry saves RBP at outer-entry SP - 8,
 * establishes RBP there, and CALLs the original body after its fentry NOP.
 * Both body prologues first push that RBP. Thus the body's saved RBP slot,
 * unlike a guessed trampoline-local byte count, links the physical post
 * stack to the independently retained outer entry. CALL/IPMODIFY layouts
 * reconstruct a different entry and are refused by ap_fdget_post below.
 *
 * Return a stack coordinate in the outer entry frame for that unchanged
 * equality. This helper never reads call->entry_stack. Only the lower kernel
 * read is substitutable by the host control; no receipt is manufactured. */
static __attribute__((always_inline)) inline u64 ap_fdget_post_stack(
    u64 operation,u64 stack,long (*read_kernel)(void *,u32,const void *)) {
    const u64 frame=operation==AP_ACCEPT_EFFECT?AP_ACCEPT_ENTRY_FRAME_BYTES:
        operation==AP_ORIGINAL_CONNECT?AP_CONNECT_ENTRY_FRAME_BYTES:0;
    if(!frame || !stack || (stack&7) || stack>~0ULL-frame)return 0;
    const u64 body_entry=stack+frame;
    u64 saved_bp=0;
    if(read_kernel(&saved_bp,sizeof(saved_bp),(const void *)(body_entry-sizeof(saved_bp))) ||
       !saved_bp || (saved_bp&7) || saved_bp<=body_entry ||
       saved_bp>~0ULL-sizeof(saved_bp))return 0;
    return saved_bp+sizeof(saved_bp)-frame;
}
static __attribute__((always_inline)) inline int ap_fdget_post(
    struct ap_fd_call *call,const struct ap_task_command *owner,
    const struct ap_fd_accept *accept,u64 task,u64 start,u64 raw_table,u64 table,
    u64 cookie,u64 stack,u64 saved_option,u64 saved_address,u64 word) {
    if(!ap_fdget_context(call,owner,accept,task,start,raw_table,table))return 0;
    const int is_accept=call->operation==AP_ACCEPT_EFFECT;
    const u64 frame=is_accept?AP_ACCEPT_ENTRY_FRAME_BYTES:AP_CONNECT_ENTRY_FRAME_BYTES;
    if(cookie!=(is_accept?AP_ACCEPT_POST_COOKIE:AP_CONNECT_POST_COOKIE) ||
       !stack || call->entry_stack<frame || stack!=call->entry_stack-frame ||
       (u32)saved_option!=(u32)owner->expected_option ||
       (!is_accept && saved_address!=owner->generation_before) ||
       !ap_fd_selection_return(&call->selection,word))return 0;
    call->entry_stack=0;return 1;
}
static __attribute__((always_inline)) inline int ap_fdget_session_post(
    struct ap_fd_call *call,const struct ap_task_command *owner,
    const struct ap_fd_accept *accept,u64 task,u64 start,u64 raw_table,u64 table,
    u64 role,u64 word) {
    if(!ap_fdget_context(call,owner,accept,task,start,raw_table,table) ||
       !call->entry_stack ||
       role!=(call->operation==AP_ACCEPT_EFFECT?AP_SHARED_FDGET_ACCEPT_ROLE:
              AP_SHARED_FDGET_CONNECT_ROLE) ||
       !ap_fd_selection_return(&call->selection,word))return 0;
    call->entry_stack=0;return 1;
}
/* The original fdget_raw entry supplies its actual u32 FD and the
 * current task's original syscall operands before claim. The exact direct
 * caller/frame guard separately authenticates the inlined syscall72 path. */
static __attribute__((always_inline)) inline int ap_file_entry_operands(
    const struct ap_task_command *owner,u64 lookup_fd,s64 original_nr,s32 fd,s32 file_command) {
    return owner && !owner->original_count && ap_original_file_operation(owner->operation) &&
        ap_original_file_shape(owner->generation_before,owner->expected_option) &&
        lookup_fd==(u64)(u32)fd && original_nr==AP_FILE_SYSCALL_FCNTL &&
        fd==owner->expected_level && file_command==owner->expected_option;
}
struct ap_file_entry_snapshot { s64 original_nr; s32 fd, file_command; };
/* Copy all original kernel operands before claiming the command. The actual
 * x86 caller loads fd/command with 32-bit MOVs; orig_ax must match in full.
 * A failed copy cannot publish even a plausible partially written snapshot. */
static __attribute__((always_inline)) inline int ap_file_read_entry(
    const struct ap_task_command *owner,u64 lookup_fd,const void *original_nr,
    const void *fd,const void *file_command,long (*read_kernel)(void *,u32,const void *),
    struct ap_file_entry_snapshot *out) {
    if(!owner || !original_nr || !fd || !file_command || !read_kernel || !out)return 0;
    struct ap_file_entry_snapshot observed={0};
    if(read_kernel(&observed.original_nr,sizeof(observed.original_nr),original_nr) ||
       read_kernel(&observed.fd,sizeof(observed.fd),fd) ||
       read_kernel(&observed.file_command,sizeof(observed.file_command),file_command) ||
       !ap_file_entry_operands(owner,lookup_fd,observed.original_nr,
          observed.fd,observed.file_command))return 0;
    *out=observed;return 1;
}
/* -1 is an unreadable/invalid physical frame; 0 is an unrelated caller.
 * Both callbacks read the same original call return word, before RET consumes
 * it. No kretprobe, stack walk, syscall errno or numeric-FD lookup substitutes
 * for this direct-call witness. The caller is checked before command claim. */
static __attribute__((always_inline)) inline int ap_file_lookup_caller(
    u64 observed_ip,u64 stack,int post,long (*read_kernel)(void *,u32,const void *)) {
    const u64 frame=post?AP_FILE_LOOKUP_FRAME_BYTES:0;
    if(!stack || (stack&7) || stack>~0ULL-frame || !read_kernel)return -1;
    u64 return_ip=0;
    if(read_kernel(&return_ip,sizeof(return_ip),(const void *)(stack+frame)))return -1;
    return ap_file_caller_site(observed_ip,return_ip,post);
}
/* Common immutable identity checks before either physical selection. File
 * and Read still own their distinct operation, shape, count and flag guards.
 * No phase is advanced and no selection is created by this predicate. */
#ifdef __BPF__
static __attribute__((noinline)) int
#else
static __attribute__((always_inline)) inline int
#endif
ap_unpublished_selection_context(const struct ap_fd_call *call,const struct ap_task_command *owner,
    const struct ap_selection_physical *p) {
    if(!p || !call || !owner || !owner->provider || !owner->command || !owner->expected_object ||
       !p->task || !p->start || !p->raw_table || !p->table || !call->file_entry_ip ||
       call->operation!=owner->operation || call->command!=owner->command ||
       call->raw_table!=p->raw_table || call->original.problem || call->original.complete)return 0;
    const struct ap_original_selection *s=&call->original.selection;
    return !s->ready && s->provider==owner->provider && s->command==owner->command &&
        s->call==owner->expected_object && s->owner_mm==owner->generation_after &&
        s->task==p->task && s->task_start==p->start && s->table==p->table &&
        s->requested_fd==owner->expected_level && s->user_address==owner->generation_before &&
        !s->file && !s->fdput_flags;
}
#ifdef __BPF__
static __attribute__((noinline)) int
#else
static __attribute__((always_inline)) inline int
#endif
ap_file_selection_context_shared(const struct ap_fd_call *call,const struct ap_task_command *owner,
    const struct ap_selection_physical *p) {
    return owner && !owner->original_count && ap_original_file_operation(owner->operation) &&
        ap_original_file_shape(owner->generation_before,owner->expected_option) &&
        ap_unpublished_selection_context(call,owner,p) &&
        call->original.selection.address_length==owner->expected_option &&
        !call->original.selection.original_count;
}
static __attribute__((always_inline)) inline int ap_file_selection_context(
    const struct ap_fd_call *call,const struct ap_task_command *owner,
    u64 task,u64 start,u64 raw_table,u64 table) {
    const struct ap_selection_physical p={task,start,raw_table,table};
    return ap_file_selection_context_shared(call,owner,&p);
}
static __attribute__((always_inline)) inline int ap_file_selection_enter(
    struct ap_fd_call *call,const struct ap_task_command *owner,
    u64 task,u64 start,u64 raw_table,u64 table,u64 cookie,u64 stack) {
    if(!ap_file_selection_context(call,owner,task,start,raw_table,table) ||
       cookie!=AP_FILE_ENTRY_COOKIE || !stack || (stack&7) || call->entry_stack ||
       !ap_fd_selection_enter(&call->selection))return 0;
    call->entry_stack=stack;return 1;
}
static __attribute__((always_inline)) inline int ap_file_selection_post(
    struct ap_fd_call *call,const struct ap_task_command *owner,
    u64 task,u64 start,u64 raw_table,u64 table,u64 cookie,u64 stack,
    u64 saved_fd,u64 saved_command,u64 word,u64 post_ip) {
    if(!ap_file_selection_context(call,owner,task,start,raw_table,table) ||
       cookie!=AP_FILE_POST_COOKIE || !ap_file_post_site(call->file_entry_ip,post_ip) ||
       !stack || (stack&7) ||
       call->entry_stack<AP_FILE_LOOKUP_FRAME_BYTES ||
       stack!=call->entry_stack-AP_FILE_LOOKUP_FRAME_BYTES ||
       saved_fd!=(u64)(u32)owner->expected_level ||
       saved_command!=(u64)(u32)owner->expected_option ||
       !ap_fd_selection_return(&call->selection,word))return 0;
    call->entry_stack=0;return 1;
}
static __attribute__((always_inline)) inline int ap_file_selection_session_post(
    struct ap_fd_call *call,const struct ap_task_command *owner,
    u64 task,u64 start,u64 raw_table,u64 table,u64 word) {
    if(!ap_ftrace_role_enabled(5) ||
       !ap_file_selection_context(call,owner,task,start,raw_table,table) ||
       !call->entry_stack || !ap_fd_selection_return(&call->selection,word))return 0;
    call->entry_stack=0;return 1;
}
/* AUTONOMOUS-BOT-IMPLEMENTED: exact scalar Read selection observation.
 * TODO-HUMAN-REVIEW(PR-id): component only; capability remains disabled. */
struct ap_read_entry_snapshot { s64 original_nr; u64 buffer,count; s32 fd; };
static __attribute__((always_inline)) inline int ap_read_entry_operands(
    const struct ap_task_command *owner,s64 nr,s32 fd,u64 buffer,u64 count) {
    return owner && owner->operation==AP_ORIGINAL_READ && !owner->expected_option &&
        nr==AP_READ_SYSCALL && fd==owner->expected_level &&
        buffer==owner->generation_before && count==owner->original_count;
}
static __attribute__((always_inline)) inline int ap_read_entry_copy(
    const struct ap_task_command *owner,const void *nr,const void *fd,const void *buffer,
    const void *count,long (*read_kernel)(void *,u32,const void *),
    struct ap_read_entry_snapshot *out) {
    if(!ap_ftrace_role_enabled(7) || !owner || !nr || !fd || !buffer ||
       !count || !read_kernel || !out)return 0;
    struct ap_read_entry_snapshot observed={0};
    if(read_kernel(&observed.original_nr,sizeof(observed.original_nr),nr) ||
       read_kernel(&observed.fd,sizeof(observed.fd),fd) ||
       read_kernel(&observed.buffer,sizeof(observed.buffer),buffer) ||
       read_kernel(&observed.count,sizeof(observed.count),count) ||
       !ap_read_entry_operands(owner,observed.original_nr,observed.fd,
           observed.buffer,observed.count))return 0;
    *out=observed;return 1;
}
#ifdef __BPF__
static __attribute__((noinline)) int
#else
static __attribute__((always_inline)) inline int
#endif
ap_read_selection_context_shared(const struct ap_fd_call *call,const struct ap_task_command *owner,
    const struct ap_selection_physical *p) {
    return owner && owner->operation==AP_ORIGINAL_READ && !owner->expected_option &&
        ap_unpublished_selection_context(call,owner,p) &&
        call->original.selection.original_count==owner->original_count &&
        !call->original.selection.address_length;
}
static __attribute__((always_inline)) inline int ap_read_selection_context(
    const struct ap_fd_call *call,const struct ap_task_command *owner,
    u64 task,u64 start,u64 raw_table,u64 table) {
    const struct ap_selection_physical p={task,start,raw_table,table};
    return ap_read_selection_context_shared(call,owner,&p);
}
static __attribute__((always_inline)) inline int ap_read_selection_enter(
    struct ap_fd_call *call,const struct ap_task_command *owner,
    u64 task,u64 start,u64 raw_table,u64 table,u64 cookie,u64 stack) {
    if(!ap_read_selection_context(call,owner,task,start,raw_table,table) ||
       cookie!=AP_READ_ENTRY_COOKIE || !stack || (stack&7) || call->entry_stack ||
       !ap_fd_selection_enter(&call->selection))return 0;
    call->entry_stack=stack;return 1;
}
/* Only this original wrapper's direct fdget_pos call is eligible. A nested or
 * unrelated fdget_pos is inert, including after ready. A failed kernel read
 * at the retained direct-call coordinate is an error, not absence. The
 * original wrapper is not attached by fentry/fexit; its saved PC is the
 * classic kprobe's actual instruction address + 1 on this bound image. */
static __attribute__((always_inline)) inline int ap_read_direct_call(
    const struct ap_fd_call *call,u64 stack,long (*read_kernel)(void *,u32,const void *)) {
    if(!call || call->operation!=AP_ORIGINAL_READ || !call->file_entry_ip ||
       !stack || (stack&7) || call->entry_stack<AP_READ_FDGET_STACK_BYTES ||
       stack!=call->entry_stack-AP_READ_FDGET_STACK_BYTES)return 0;
    if(!read_kernel || stack>~0ULL-AP_READ_FDGET_RETURN_SLOT ||
       call->file_entry_ip>~0ULL-AP_READ_CALLER_PC_DELTA)return -1;
    u64 actual_return=0;
    if(read_kernel(&actual_return,sizeof(actual_return),
        (const void *)(stack+AP_READ_FDGET_RETURN_SLOT)))return -1;
    return actual_return==call->file_entry_ip+AP_READ_CALLER_PC_DELTA;
}
static __attribute__((always_inline)) inline int ap_read_selection_post(
    struct ap_fd_call *call,const struct ap_task_command *owner,
    u64 task,u64 start,u64 raw_table,u64 table,u64 cookie,u64 buffer,u64 count,u64 word) {
    if(!ap_read_selection_context(call,owner,task,start,raw_table,table) ||
       (cookie!=AP_READ_SELECTED_COOKIE && cookie!=AP_READ_NULL_COOKIE) ||
       (cookie==AP_READ_NULL_COOKIE && word) || buffer!=owner->generation_before ||
       count!=owner->original_count || !ap_fd_selection_return(&call->selection,word))return 0;
    /* Retain entry_stack until original sys_exit. It rejects another actual
     * direct callback after ready while unrelated nested callers stay inert. */
    return 1;
}
static __attribute__((always_inline)) inline int ap_read_selection_session_post(
    struct ap_fd_call *call,const struct ap_task_command *owner,
    u64 task,u64 start,u64 raw_table,u64 table,u64 word) {
    if(!ap_ftrace_role_enabled(word?8:9) ||
       !ap_read_selection_context(call,owner,task,start,raw_table,table) ||
       !call->entry_stack || !ap_fd_selection_return(&call->selection,word))return 0;
    /* Keep the real fdget_pos entry witness through original sys_exit, just
     * as the classic wrapper-entry stack remained live until that boundary. */
    return 1;
}

/* Both callbacks are instruction-boundary probes inside the same original
 * __sys_connect frame. The kernel stack destination and callee-saved values
 * survive _copy_from_user, including a UFFD sleep. No return-probe pairing,
 * nested-copy depth, guest reread, or errno inference supplies the witness. */
struct ap_copy_frame_snapshot {u64 task,start,stack,file_word,length,address;};
#ifdef __BPF__
static __attribute__((noinline)) int
#else
static __attribute__((always_inline)) inline int
#endif
ap_original_copy_frame_shared(const struct ap_fd_call *call,const struct ap_task_command *owner,
        const struct ap_copy_frame_snapshot *p) {
    if(!p)return 0;
    const u64 task=p->task,start=p->start,stack=p->stack,file_word=p->file_word,
        length=p->length,address=p->address;
    if(!call || !owner || !stack || call->operation!=AP_ORIGINAL_CONNECT ||
       call->command!=owner->command || !call->function_ip || call->original.problem ||
       call->original.complete || !ap_original_selection_matches(owner,&call->original.selection) ||
       call->original.selection.ready!=1 || !call->original.selection.file ||
       call->original.selection.task!=task || call->original.selection.task_start!=start ||
       !ap_fd_selection_complete(&call->selection) || call->selection.word!=file_word ||
       call->original.selection.address_length<=0 || call->original.selection.address_length>128 ||
       length!=(u64)call->original.selection.address_length ||
       address!=call->original.selection.user_address ||
       call->original.audit_entered || call->original.security_entered)return 0;
    return 1;
}
static __attribute__((always_inline)) inline int ap_original_copy_frame(
    const struct ap_fd_call *call,const struct ap_task_command *owner,
    u64 task,u64 start,u64 stack,u64 file_word,u64 length,u64 address) {
    const struct ap_copy_frame_snapshot p={task,start,stack,file_word,length,address};
    return ap_original_copy_frame_shared(call,owner,&p);
}
static __attribute__((always_inline)) inline int ap_original_copy_begin(
    struct ap_fd_call *call,const struct ap_task_command *owner,u64 task,u64 start,
    u64 stack,u64 destination,u64 source,u64 count,u64 file_word,u64 length,u64 address) {
    if(!ap_original_copy_frame(call,owner,task,start,stack,file_word,length,address) ||
       destination!=stack || source!=address || count!=length ||
       call->copied_address || call->original.copy_entered || call->original.copy_returned)return 0;
    call->copied_address=destination;call->original.copy_entered=1;return 1;
}
static __attribute__((always_inline)) inline int ap_original_copy_end(
    struct ap_fd_call *call,const struct ap_task_command *owner,u64 task,u64 start,
    u64 stack,u64 remaining,u64 file_word,u64 length,u64 address) {
    if(!ap_original_copy_frame(call,owner,task,start,stack,file_word,length,address) ||
       call->copied_address!=stack || call->original.copy_entered!=1 ||
       call->original.copy_returned || remaining>length)return 0;
    call->original.copy_remaining=remaining;call->original.copy_returned=1;return 1;
}
/* The exact admitted __sys_connect image reaches audit/security only after
 * its inlined _copy_from_user returned zero. At either direct callee entry,
 * the copied sockaddr is the caller's stack pointer, one return slot above
 * the observed callee SP. This replaces the two notrace mid-body probes
 * without treating a later errno or a guest-memory reread as copy evidence. */
static __attribute__((always_inline)) inline int ap_original_copy_infer_success(
    struct ap_fd_call *call,const struct ap_task_command *owner,u64 task,u64 start,
    u64 callee_stack,u64 destination,u64 file_word,u64 length,u64 address) {
    if(!ap_ftrace_role_enabled(2) ||
       !callee_stack || callee_stack>~0ULL-8 || destination!=callee_stack+8 ||
       !ap_original_copy_frame(call,owner,task,start,destination,file_word,length,address) ||
       call->copied_address || call->original.copy_entered || call->original.copy_returned ||
       call->original.copy_remaining)return 0;
    call->copied_address=destination;call->original.copy_entered=1;
    if(!ap_ftrace_role_enabled(3))return 0;
    call->original.copy_returned=1;call->original.copy_remaining=0;return 1;
}
#ifndef __BPF__
#include <linux/bpf.h>
#include <stddef.h>
#include <string.h>
#include "retirement-target.h"
/* A masked addr is not guessed. Kernel-issued exact function name, offset,
 * program/link identity and cookie establish which owned site was attached. */
static inline int ap_classic_link_matches(const struct bpf_prog_info *program,
    unsigned ps,const struct bpf_link_info *link,unsigned ls,const char *symbol,
    const char *expected,size_t name_len,u64 offset,u64 cookie) {
    return ps>=offsetof(struct bpf_prog_info,recursion_misses)+sizeof(program->recursion_misses) &&
        ls>=offsetof(struct bpf_link_info,perf_event.kprobe.cookie)+sizeof(link->perf_event.kprobe.cookie) &&
        program->type==BPF_PROG_TYPE_KPROBE && program->id && link->id &&
        link->type==BPF_LINK_TYPE_PERF_EVENT && link->prog_id==program->id &&
        link->perf_event.type==BPF_PERF_EVENT_KPROBE &&
        link->perf_event.kprobe.name_len==name_len && !memcmp(symbol,expected,name_len) &&
        link->perf_event.kprobe.offset==offset && link->perf_event.kprobe.cookie==cookie &&
        !program->recursion_misses && !link->perf_event.kprobe.missed;
}
static inline int ap_copy_link_matches(unsigned which,const struct bpf_prog_info *program,
    unsigned ps,const struct bpf_link_info *link,unsigned ls,const char *symbol) {
    return which<2 && ap_classic_link_matches(program,ps,link,ls,symbol,
        AP_CONNECT_COPY_SYMBOL_OWNER,sizeof(AP_CONNECT_COPY_SYMBOL_OWNER),
        which?AP_CONNECT_COPY_AFTER_OFFSET:AP_CONNECT_COPY_BEFORE_OFFSET,
        which?AP_CONNECT_COPY_AFTER_COOKIE:AP_CONNECT_COPY_BEFORE_COOKIE);
}
static inline int ap_fdget_link_matches(unsigned which,const struct bpf_prog_info *program,
    unsigned ps,const struct bpf_link_info *link,unsigned ls,const char *symbol) {
    if(which==7 || which==8)return ap_classic_link_matches(program,ps,link,ls,symbol,
        AP_EPOLL_CTL_SYMBOL,sizeof(AP_EPOLL_CTL_SYMBOL),
        which==7?AP_EPOLL_PRIMARY_POST_OFFSET:AP_EPOLL_TARGET_POST_OFFSET,
        which==7?AP_EPOLL_PRIMARY_POST_COOKIE:AP_EPOLL_TARGET_POST_COOKIE);
    if(which>=4 && which<=6)return ap_classic_link_matches(program,ps,link,ls,symbol,
        which==4?AP_READ_ENTRY_SYMBOL:AP_READ_SELECTED_SYMBOL,
        which==4?sizeof(AP_READ_ENTRY_SYMBOL):sizeof(AP_READ_SELECTED_SYMBOL),
        which==4?AP_READ_ENTRY_OFFSET:which==5?AP_READ_SELECTED_OFFSET:AP_READ_NULL_OFFSET,
        which==4?AP_READ_ENTRY_COOKIE:which==5?AP_READ_SELECTED_COOKIE:AP_READ_NULL_COOKIE);
    if(which==2 || which==3)return ap_classic_link_matches(program,ps,link,ls,symbol,
        AP_FILE_SYMBOL,sizeof(AP_FILE_SYMBOL),
        which==2?AP_FILE_FDGET_RETURN_OFFSET:AP_FILE_ENTRY_OFFSET,
        which==2?AP_FILE_POST_COOKIE:AP_FILE_ENTRY_COOKIE);
    return which<2 && ap_classic_link_matches(program,ps,link,ls,symbol,
        which?AP_CONNECT_SYMBOL:AP_ACCEPT_SYMBOL,
        which?sizeof(AP_CONNECT_SYMBOL):sizeof(AP_ACCEPT_SYMBOL),
        which?AP_CONNECT_FDGET_RETURN_OFFSET:AP_ACCEPT_FDGET_RETURN_OFFSET,
        which?AP_CONNECT_POST_COOKIE:AP_ACCEPT_POST_COOKIE);
}
#endif
#endif
