/* SPDX-License-Identifier: MIT */
#ifndef HERMIT_EPOLL_CTL_COPY_H
#define HERMIT_EPOLL_CTL_COPY_H
#include "provider.h"

/* A copy-only native epoll_ctl(-1, op, -1, original_pointer) in the original
 * task. This is NOT an epoll registration, fd selection, or readiness result.
 * TASK_STORAGE and the existing command ticket own the entire invocation. */
#define AP_EPOLL_CTL_COPY 13
#define AP_EPOLL_CTL_SYSCALL 233
#define AP_EPOLL_CTL_DEL 2
#define AP_EPOLL_IMAGE_PM_SLEEP_DISABLED 1ULL
struct ap_epoll_ctl_copy {
    u64 command, call, owner_mm, provider, task, task_start;
    u64 user_address, table;
    u64 entered, ctl_entered, ctl_returned, complete, problem;
    /* Image-bound CONFIG_PM_SLEEP=n policy, NOT a controller credential test.
     * The invalid epfd returns before stripping; event keeps all input bits. */
    u64 image_wakeup_policy;
    s32 op, returned;
    u8 event[12];
    u32 reserved;
};
struct ap_epoll_ctl_terminal {
    struct ap_command_result command;
    struct ap_epoll_ctl_copy copy;
    u64 call, fd_call_present, task_absent;
};
#ifdef __BPF__
static __attribute__((noinline)) int ap_epoll_ctl_command(
#else
static __attribute__((always_inline)) inline int ap_epoll_ctl_command(
#endif

    const struct ap_task_command *c) {
    return c && c->operation==AP_EPOLL_CTL_COPY && c->provider && c->command &&
        c->expected_object && c->expected_option==AP_EPOLL_CTL_SYSCALL && !c->original_count;
}
/* The two FDs and op are Linux int operands; pointer comparison is full64.
 * Check native x86-64 CS as well as the post-seccomp syscall number. */
static __attribute__((always_inline)) inline int ap_epoll_ctl_operands(
    const struct ap_task_command *c,s64 nr,u64 cs,u64 epfd,u64 op,u64 fd,u64 pointer) {
    return ap_epoll_ctl_command(c) && nr==AP_EPOLL_CTL_SYSCALL && cs==0x33 &&
        (s32)epfd==-1 && (s32)fd==-1 && (s32)op==c->expected_level && pointer==c->generation_before;
}
#ifdef __BPF__
static __attribute__((noinline)) int
#else
static __attribute__((always_inline)) inline int
#endif
ap_epoll_ctl_binding(
    const struct ap_task_command *c,const struct ap_epoll_ctl_copy *r) {
    return ap_epoll_ctl_command(c) && r && r->command==c->command &&
        r->call==c->expected_object && r->owner_mm==c->generation_after &&
        r->provider==c->provider && r->task && r->task_start &&
        r->user_address==c->generation_before && r->op==c->expected_level;
}
#ifdef __BPF__
static __attribute__((noinline)) int
#else
static __attribute__((always_inline)) inline int
#endif
ap_epoll_ctl_identity(
    const struct ap_task_command *c,const struct ap_epoll_ctl_copy *r) {
    return ap_epoll_ctl_binding(c,r) && r->table && r->entered==1 &&
        r->image_wakeup_policy==AP_EPOLL_IMAGE_PM_SLEEP_DISABLED && !r->reserved;
}
#ifdef __BPF__
static __attribute__((noinline)) int ap_epoll_ctl_event_empty(
#else
static __attribute__((always_inline)) inline int ap_epoll_ctl_event_empty(
#endif
    const struct ap_epoll_ctl_copy *r) {
    /* Fixed native packed epoll_event size. DEL must never read its stack slot. */
    u32 words[3];
    _Static_assert(sizeof(words)==sizeof(r->event),"all twelve event bytes");
    __builtin_memcpy(words,r->event,sizeof(words));
    return !(words[0] | words[1] | words[2]);
}
static __attribute__((always_inline)) inline int ap_epoll_ctl_outcome(
    const struct ap_epoll_ctl_copy *r,s64 returned) {
    if(!r || r->entered!=1 || r->problem || r->complete || r->reserved)return 0;
    if(returned==-9)
        return r->ctl_entered==1 && r->ctl_returned==1 &&
            (r->op!=AP_EPOLL_CTL_DEL || ap_epoll_ctl_event_empty(r));
    /* The exact image dispatches nr233 after this positive sys_enter. Its
     * sole pre-do_epoll_ctl failure is native copy_from_user returning bytes
     * remaining. Missing callbacks alone never authorize this branch. */
    return returned==-14 && r->op!=AP_EPOLL_CTL_DEL &&
        !r->ctl_entered && !r->ctl_returned && ap_epoll_ctl_event_empty(r);
}
static __attribute__((always_inline)) inline int ap_epoll_ctl_copy_matches(
    const struct ap_task_command *c,const struct ap_command_result *command,
    const struct ap_epoll_ctl_copy *r) {
    if(!ap_epoll_ctl_identity(c,r) || !command || command->command!=c->command ||
       command->operation!=AP_EPOLL_CTL_COPY || command->phase!=AP_COMMAND_DONE ||
       command->identity.provider!=c->provider || command->original_count ||
       command->task!=r->task || command->start_boottime!=r->task_start ||
       command->returned!=r->returned || r->complete!=1 || r->problem)return 0;
    if(r->returned==-9)
        return r->ctl_entered==1 && r->ctl_returned==1 &&
            (r->op!=AP_EPOLL_CTL_DEL || ap_epoll_ctl_event_empty(r));
    return r->returned==-14 && r->op!=AP_EPOLL_CTL_DEL &&
        !r->ctl_entered && !r->ctl_returned && ap_epoll_ctl_event_empty(r);
}
/* Actual original epoll_ctl. Keep op13's -1/-1 copy probe above unchanged. */
#ifdef __BPF__
static __attribute__((noinline)) int ap_original_epoll_ctl_command(
#else
static __attribute__((always_inline)) inline int ap_original_epoll_ctl_command(
#endif

    const struct ap_task_command *c) {
    return c && c->operation==AP_ORIGINAL_EPOLL_CTL && c->provider && c->command &&
        c->expected_object && c->original_count<=0xffffffffULL;
}
struct ap_epoll_operand_snapshot {s64 nr;u64 cs,epfd,op,fd,pointer;};
#ifdef __BPF__
static __attribute__((noinline)) int
#else
static __attribute__((always_inline)) inline int
#endif
ap_original_epoll_ctl_operands_shared(const struct ap_task_command *c,
    const struct ap_epoll_operand_snapshot *p) {
    if(!p)return 0;
    const s64 nr=p->nr;const u64 cs=p->cs,epfd=p->epfd,op=p->op,fd=p->fd,pointer=p->pointer;
    return ap_original_epoll_ctl_command(c) && nr==AP_EPOLL_CTL_SYSCALL && cs==0x33 &&
        (s32)epfd==c->expected_level && (s32)op==c->expected_option &&
        (u64)(u32)fd==c->original_count && pointer==c->generation_before;

}
static __attribute__((always_inline)) inline int ap_original_epoll_ctl_operands(
    const struct ap_task_command *c,s64 nr,u64 cs,u64 epfd,u64 op,u64 fd,u64 pointer) {
    const struct ap_epoll_operand_snapshot p={nr,cs,epfd,op,fd,pointer};
    return ap_original_epoll_ctl_operands_shared(c,&p);
}
#ifdef __BPF__
static __attribute__((noinline)) int ap_original_epoll_event_empty(
#else
static __attribute__((always_inline)) inline int ap_original_epoll_event_empty(
#endif
    const struct ap_original_epoll_ctl *e) {
    u32 words[3];
    _Static_assert(sizeof(words)==sizeof(e->event),"all twelve event bytes");
    __builtin_memcpy(words,e->event,sizeof(words));
    return !(words[0] | words[1] | words[2]);
}
#ifdef __BPF__
static __attribute__((noinline)) int ap_original_epoll_ctl_selected(
#else
static __attribute__((always_inline)) inline int ap_original_epoll_ctl_selected(
#endif
    const struct ap_task_command *c,const struct ap_original_result *o) {
    if(!ap_original_epoll_ctl_command(c) || !o || o->problem ||
       o->selection.ready!=1 || o->selection.command!=c->command ||
       o->selection.call!=c->expected_object || o->selection.owner_mm!=c->generation_after ||
       o->selection.provider!=c->provider || !o->selection.task || !o->selection.task_start ||
       !o->selection.table || o->selection.requested_fd!=c->expected_level ||
       o->selection.user_address!=c->generation_before || o->selection.address_length!=c->expected_option ||
       o->selection.original_count!=c->original_count || o->selection.fdput_flags>1 ||
       (!o->selection.file && o->selection.fdput_flags))return 0;
    const struct ap_original_epoll_ctl *e=&o->epoll_ctl;
    if(e->entered!=1 || e->reserved || e->result_reserved ||
       e->image_wakeup_policy!=AP_EPOLL_IMAGE_PM_SLEEP_DISABLED ||
       e->ctl_entered>1 || e->primary_selected>1 || e->secondary_selected>1 || e->target_flags>1 ||
       (!e->target_file && e->target_flags))return 0;
    /* A pre-do_epoll_ctl failure supplies no descriptor selection. Its actual
     * entered original return remains necessary below; absence is not EBADF. */
    if(!e->ctl_entered)
        return o->complete==1 && o->returned==-14 && c->expected_option!=AP_EPOLL_CTL_DEL &&
            !e->primary_selected && !e->secondary_selected && !o->selection.file &&
            !e->target_file && !e->target_flags && !e->primary_cut && !e->target_cut &&
            ap_original_epoll_event_empty(e);
    if(e->primary_selected!=1 || e->primary_cut<e->before ||
       (c->expected_option==AP_EPOLL_CTL_DEL && !ap_original_epoll_event_empty(e)))return 0;
    if(!o->selection.file)
        return !e->secondary_selected && !e->target_file && !e->target_flags && !e->target_cut;
    return e->secondary_selected==1 && e->target_cut>=e->primary_cut;
}
static __attribute__((always_inline)) inline int ap_original_epoll_ctl_result_matches(
    const struct ap_task_command *c,const struct ap_command_result *r,const struct ap_original_result *o) {
    if(!ap_original_epoll_ctl_selected(c,o) || !r || r->command!=c->command ||
       r->operation!=AP_ORIGINAL_EPOLL_CTL || r->phase!=AP_COMMAND_DONE || r->reserved ||
       r->identity.provider!=c->provider || r->identity.object || r->identity.namespace ||
       r->creation || r->cookie || r->task!=o->selection.task || r->start_boottime!=o->selection.task_start ||
       r->original_count!=c->original_count || r->returned!=o->returned || o->complete!=1 || o->reserved ||
       o->returned< -4095 || o->returned>0 || o->copy_entered || o->copy_returned || o->copy_remaining ||
       o->audit_entered || o->audit_returned || o->audit_result ||
       o->security_entered || o->security_returned || o->security_result)return 0;
    for(unsigned n=sizeof(struct ap_original_epoll_ctl);n<sizeof(o->address);n++)if(o->address[n])return 0;
    const u8 *state=(const u8 *)&r->state;
    for(unsigned n=0;n<sizeof(r->state);n++)if(state[n])return 0;
    const struct ap_original_epoll_ctl *e=&o->epoll_ctl;
    if(!e->ctl_entered)
        return c->expected_option!=AP_EPOLL_CTL_DEL && o->returned==-14 &&
            !e->ctl_returned && !e->ctl_result;
    if(e->ctl_returned!=1 || e->ctl_result!=o->returned)return 0;
    return (o->selection.file && e->target_file) || o->returned==-9;
}
#ifndef __BPF__
struct ap_session;
int ap_prepare_epoll_ctl_copy(struct ap_session *,int exact_pidfd,u64 call,u64 mm,
                              int op,u64 original_pointer,u64 *command);
int ap_collect_epoll_ctl_copy(struct ap_session *,int exact_pidfd,u64 command,
                              struct ap_command_result *,struct ap_epoll_ctl_copy *);
/* Requires the backend's positive never-invoked marker, not READY alone. */
int ap_cancel_uninvoked_epoll_ctl_copy(struct ap_session *,int exact_pidfd,u64 command);
/* Requires the retained exact PIDFD_THREAD and backend final-wait evidence. */
int ap_retire_dead_epoll_ctl_copy(struct ap_session *,int exact_pidfd,u64 command,
                                 struct ap_epoll_ctl_terminal *);
#endif
#endif
