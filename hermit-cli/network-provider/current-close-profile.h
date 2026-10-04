/* SPDX-License-Identifier: BSD-3-Clause */
#ifndef HERMIT_CURRENT_CLOSE_PROFILE_H
#define HERMIT_CURRENT_CLOSE_PROFILE_H
#include "fd-enrollment.h"
#include "current-close-target.h"
#define AP_CURRENT_CLOSE_PROFILE 27ULL
#define AP_CLOSE_PROFILE_VERSION 1ULL
#define AP_CLOSE_ENTERED 1ULL
#define AP_CLOSE_OBSERVED 2ULL
#define AP_CLOSE_RETURNED 4ULL
#define AP_CLOSE_CONTEXT 1ULL
#define AP_CLOSE_READ 2ULL
#define AP_CLOSE_UNSUPPORTED 4ULL
#define AP_CLOSE_REGISTER_BYTES 216ULL
#define AP_CLOSE_SYSCALL 3ULL
struct ap_close_profile_intent {
    u64 command,registration,owner_mm,normal_epoch,expected_table,expected_file;
    s32 fd;u32 reserved;u64 syscall_nr;
};
struct ap_close_profile_observation {
    u64 task,start,tracer,tracer_start,mm,table,file,raw_table,raw_file;
    u64 socket,sk,inode,file_ops,file_flush,file_release;
    u64 socket_ops,socket_release,protocol_ops,protocol_close;
    u64 ulp_ops,ulp_data,file_ref_raw,file_refs,linger_ticks;
    u32 files_refs,max_fds,aliases,family,type,protocol,repair,linger;
};
struct ap_close_profile {
    struct ap_close_profile_intent intent;
    struct ap_close_profile_observation entered,returned;
    u64 phases,problem;s64 ptrace_return;
    u64 iovec,registers,register_bytes,original_nr,original_fd;
};
_Static_assert(sizeof(struct ap_close_profile_intent)==64,"close intent ABI12");
_Static_assert(sizeof(struct ap_close_profile_observation)==224,"close observation ABI12");
_Static_assert(sizeof(struct ap_close_profile)==576,"close receipt ABI12");
static __attribute__((always_inline)) inline int ap_close_intent_matches(
        const struct ap_task_command *c,const struct ap_close_profile_intent *i) {
    return c && i && c->provider && c->command && c->operation==AP_CURRENT_CLOSE_PROFILE &&
        c->expected_object && c->generation_before && !c->original_count && !c->expected_timeout_ticks &&
        c->expected_level==AP_PTRACE_GETREGSET && c->expected_option==AP_NT_PRSTATUS &&
        i->command==c->command && i->registration==c->generation_before && i->owner_mm==c->generation_after &&
        i->expected_table==c->expected_object && i->expected_file &&
        i->fd>=0 && (u32)i->fd<AP_FD_FILES && !i->reserved && i->syscall_nr==AP_CLOSE_SYSCALL;
}
/* Linux295ad uses references-minus-one. Saturation/dead values are not counts. */
static __attribute__((always_inline)) inline int ap_close_refs(u64 raw,u32 aliases,u64 *decoded) {
    if(!decoded || !aliases || aliases>AP_FD_FILES || raw>0x7fffffffffffffffULL)return 0;
    *decoded=raw+1;return *decoded==aliases;
}
static __attribute__((always_inline)) inline int ap_close_observation_owned(
        const struct ap_close_profile_intent *i,const struct ap_close_profile_observation *o) {
    u64 count=0;
    return i && o && o->task && o->start && o->tracer && o->tracer_start && o->mm &&
        o->table==i->expected_table && o->file==i->expected_file && o->raw_table && o->raw_file &&
        o->inode && o->file_ops && o->files_refs==1 && o->max_fds && o->max_fds<=AP_FD_FILES &&
        (u32)i->fd<o->max_fds && o->aliases<=o->max_fds &&
        ap_close_refs(o->file_ref_raw,o->aliases,&count) && o->file_refs==count;
}
static __attribute__((always_inline)) inline int ap_close_owned_same(
        const struct ap_close_profile_observation *a,const struct ap_close_profile_observation *b) {
    return a->task==b->task && a->start==b->start && a->tracer==b->tracer &&
        a->tracer_start==b->tracer_start && a->mm==b->mm && a->table==b->table && a->file==b->file &&
        a->raw_table==b->raw_table && a->raw_file==b->raw_file && a->inode==b->inode &&
        a->files_refs==b->files_refs && a->max_fds==b->max_fds && a->aliases==b->aliases &&
        a->file_ref_raw==b->file_ref_raw && a->file_refs==b->file_refs;
}
/* Physical safety only: never a Record/Replay foreground/background selector. */
static __attribute__((always_inline)) inline int ap_close_observation_finite(
        const struct ap_close_profile_observation *o,u64 anchor) {
    return o && anchor && o->socket && o->sk && o->family==AP_AF_INET && o->type==1 &&
        o->protocol==AP_IPPROTO_TCP && !o->repair && !o->linger && !o->ulp_ops && !o->ulp_data &&
        o->file_ops==ap_grouped_image_address(anchor,AP_CLOSE_SOCKET_FILE_OPS_IMAGE) && !o->file_flush &&
        o->file_release==ap_grouped_image_address(anchor,AP_CLOSE_SOCK_CLOSE_IMAGE) &&
        o->socket_ops==ap_grouped_image_address(anchor,AP_CLOSE_INET_STREAM_OPS_IMAGE) &&
        o->socket_release==ap_grouped_image_address(anchor,AP_CLOSE_INET_RELEASE_IMAGE) &&
        o->protocol_ops==ap_grouped_image_address(anchor,AP_CLOSE_TCP_PROT_IMAGE) &&
        o->protocol_close==ap_grouped_image_address(anchor,AP_CLOSE_TCP_CLOSE_IMAGE);
}
/* Complete unsupported physical observations are collectible/ACKable. They
 * still fail ap_close_profile_finite and never authorize generic fallback. */
static __attribute__((always_inline)) inline int ap_close_profile_matches(
        const struct ap_task_command *c,const struct ap_command_result *r,const struct ap_close_profile *e) {
    return c && r && e && ap_close_intent_matches(c,&e->intent) &&
        r->command==c->command && r->operation==AP_CURRENT_CLOSE_PROFILE && r->phase==AP_COMMAND_DONE &&
        r->identity.provider==c->provider && !r->identity.object && !r->identity.namespace &&
        !r->creation && !r->cookie && !r->returned && !r->reserved && !r->original_count &&
        !r->state.receive_timeout_ticks && !r->state.send_timeout_ticks && !r->state.lowat &&
        !r->state.receive_buffer && !r->state.peek_offset && !r->state.socket_option_memory &&
        !r->state.window_clamp && !r->state.userlocks && !r->state.scaling_ratio &&
        !r->state.tcp_state && !r->state.child_spin_locked &&
        r->task==e->entered.task && r->start_boottime==e->entered.start &&
        e->phases==(AP_CLOSE_ENTERED|AP_CLOSE_OBSERVED|AP_CLOSE_RETURNED) &&
        !(e->problem&~AP_CLOSE_UNSUPPORTED) && !e->ptrace_return && e->iovec && e->registers &&
        e->register_bytes==AP_CLOSE_REGISTER_BYTES && e->original_nr==AP_CLOSE_SYSCALL &&
        e->original_fd==(u64)(u32)e->intent.fd &&
        ap_close_observation_owned(&e->intent,&e->entered) &&
        ap_close_observation_owned(&e->intent,&e->returned) && ap_close_owned_same(&e->entered,&e->returned);
}
static __attribute__((always_inline)) inline int ap_close_profile_finite(
        const struct ap_task_command *c,const struct ap_command_result *r,const struct ap_close_profile *e,u64 anchor) {
    return ap_close_profile_matches(c,r,e) && !e->problem &&
        ap_close_observation_finite(&e->entered,anchor) && ap_close_observation_finite(&e->returned,anchor) &&
        e->entered.socket==e->returned.socket && e->entered.sk==e->returned.sk;
}
#ifndef __BPF__
int ap_prepare_current_close_profile(struct ap_session *,int,const struct ap_close_profile_intent *,u64 *);
int ap_collect_current_close_profile(struct ap_session *,int,u64,struct ap_command_result *,struct ap_close_profile *);
int ap_validate_current_close_profile(struct ap_session *,const struct ap_command_result *,const struct ap_close_profile *);
#endif
#endif
