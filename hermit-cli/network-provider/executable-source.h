/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
#ifndef HERMIT_EXECUTABLE_SOURCE_H
#define HERMIT_EXECUTABLE_SOURCE_H
#include "fd-enrollment.h"

/* ABI11 observes one closed executable-backed source under an existing
 * command owner. It is not anonymous backing or permission to read a task. */
#define AP_EXECUTABLE_SOURCE 26ULL
#define AP_EXE_REGISTER_BYTES 216ULL
#define AP_EXE_ENTERED 1ULL
#define AP_EXE_OBSERVED 2ULL
#define AP_EXE_RETURNED 4ULL
#define AP_EXE_CONTEXT 1ULL
#define AP_EXE_GEOMETRY 2ULL
#define AP_EXE_BACKING 4ULL
#define AP_EXE_READ 8ULL
#define AP_EXE_HELPER 16ULL
#define AP_EXE_VMOPS_IMAGE 0xffffffff82a8bb20ULL
#define AP_EXE_NONOTIFY 0x02000000ULL
#define AP_EXE_NONOTIFY_PERM 0x04000000ULL
#define AP_EXE_FMODE_READ 0x1U
#define AP_EXE_FMODE_WRITE 0x2U
#define AP_EXE_FMODE_CAN_READ 0x00020000U
#define AP_EXE_FMODE_OPENED 0x00080000U
#define AP_EXE_FMODE_REQUIRED (AP_EXE_FMODE_READ|AP_EXE_FMODE_OPENED|AP_EXE_FMODE_CAN_READ)
#define AP_EXE_VM_READ 0x1ULL
#define AP_EXE_VM_FORBIDDEN (0x2ULL|0x8ULL|0x400ULL|0x4000ULL|0x40000ULL|0x400000ULL|0x10000000ULL)

struct ap_executable_intent {
    u64 command,registration,owner_mm,call,address,length,iovec,registers;
};
struct ap_executable_observation {
    u64 task,start,tracer,tracer_start,mm,file,exe_file,inode,mapping,fops,vm_ops;
    u64 filesystem,device,inode_number,file_size,vm_start,vm_end,vm_pgoff,vm_flags;
    u32 file_mode,inode_mode;
    s64 writecount;
    u64 iovec_base,iovec_length;
};
struct ap_executable_source {
    struct ap_executable_intent intent;
    struct ap_executable_observation entered,returned;
    u64 phases,problem;
    s64 ptrace_return,find_enter_return,find_exit_return;
};
_Static_assert(sizeof(struct ap_executable_intent)==64,"executable intent ABI11");
_Static_assert(sizeof(struct ap_executable_observation)==184,"executable observation ABI11");
_Static_assert(sizeof(struct ap_executable_source)==472,"executable receipt ABI11");

static __attribute__((always_inline)) inline int ap_executable_range(u64 address,u64 length) {
    return address && length && length<=512 && address<0x0000800000000000ULL &&
        length<=0x0000800000000000ULL-address &&
        (address>>12)==((address+length-1)>>12);
}
static __attribute__((always_inline)) inline int ap_executable_intent_matches(
        const struct ap_task_command *c,const struct ap_executable_intent *i) {
    return c && i && c->provider && c->command && c->operation==AP_EXECUTABLE_SOURCE &&
        c->expected_object && c->generation_before &&
        c->expected_level==AP_PTRACE_GETREGSET && c->expected_option==AP_NT_PRSTATUS &&
        !c->expected_timeout_ticks && i->command==c->command &&
        i->registration==c->generation_before && i->owner_mm==c->generation_after &&
        i->call==c->expected_object && i->length==c->original_count &&
        ap_executable_range(i->address,i->length) && i->iovec && i->registers &&
        i->iovec!=i->registers;
}
static __attribute__((always_inline)) inline int ap_executable_observation_valid(
        const struct ap_executable_intent *i,const struct ap_executable_observation *o,u64 anchor) {
    if(!i || !o || !o->task || !o->start || !o->tracer || !o->tracer_start ||
       !o->mm || !o->fops || !o->vm_ops || !o->file || o->file!=o->exe_file || !o->inode || !o->mapping ||
       !o->inode_number || !o->file_size || o->file_size>0x7fffffffffffffffULL ||
       (o->inode_mode&0170000)!=0100000 || o->inode_mode>0177777 ||
       o->filesystem!=AP_SOURCE_BTRFS_MAGIC || o->writecount>=0 ||
       /* Linux OPEN_FMODE initializes access mode, not the __FMODE_EXEC
        * open flag. Executable-file association and write denial are checked
        * above; require persistent readable-open state. */
       o->writecount<(-2147483647LL-1) ||
       (o->file_mode&AP_EXE_FMODE_REQUIRED)!=AP_EXE_FMODE_REQUIRED ||
       (o->file_mode&AP_EXE_FMODE_WRITE) ||
       ((o->file_mode&(AP_EXE_NONOTIFY|AP_EXE_NONOTIFY_PERM))!=AP_EXE_NONOTIFY &&
        (o->file_mode&(AP_EXE_NONOTIFY|AP_EXE_NONOTIFY_PERM))!=AP_EXE_NONOTIFY_PERM) ||
       !anchor || o->fops!=ap_grouped_image_address(anchor,AP_SOURCE_BTRFS_FOPS_IMAGE) ||
       o->vm_ops!=ap_grouped_image_address(anchor,AP_EXE_VMOPS_IMAGE) ||
       !o->vm_start || o->vm_start>=o->vm_end || (o->vm_start&4095) || (o->vm_end&4095) ||
       !(o->vm_flags&AP_EXE_VM_READ) || (o->vm_flags&AP_EXE_VM_FORBIDDEN) ||
       !ap_executable_range(i->address,i->length) || i->address<o->vm_start ||
       i->address>o->vm_end || i->length>o->vm_end-i->address ||
       o->vm_pgoff>(~0ULL>>12) || o->iovec_base!=i->registers ||
       o->iovec_length!=AP_EXE_REGISTER_BYTES)return 0;
    u64 offset=o->vm_pgoff<<12,delta=i->address-o->vm_start;
    return delta<=~0ULL-offset && offset+delta<=o->file_size &&
        i->length<=o->file_size-(offset+delta);
}
static __attribute__((always_inline)) inline int ap_executable_observations_same(
        const struct ap_executable_observation *a,const struct ap_executable_observation *b) {
    /* Other executables may acquire/release a denial reference concurrently;
     * strict negativity at both cuts, not equal counts, proves this premise. */
    return a->task==b->task && a->start==b->start && a->tracer==b->tracer &&
        a->tracer_start==b->tracer_start && a->mm==b->mm && a->file==b->file &&
        a->exe_file==b->exe_file && a->inode==b->inode && a->mapping==b->mapping &&
        a->fops==b->fops && a->vm_ops==b->vm_ops && a->filesystem==b->filesystem &&
        a->device==b->device && a->inode_number==b->inode_number && a->file_size==b->file_size &&
        a->vm_start==b->vm_start && a->vm_end==b->vm_end && a->vm_pgoff==b->vm_pgoff &&
        a->vm_flags==b->vm_flags && a->file_mode==b->file_mode && a->inode_mode==b->inode_mode &&
        a->iovec_base==b->iovec_base && a->iovec_length==b->iovec_length;
}
static __attribute__((always_inline)) inline int ap_executable_source_matches(
        const struct ap_task_command *c,const struct ap_command_result *r,
        const struct ap_executable_source *e,u64 anchor) {
    return c && r && e && ap_executable_intent_matches(c,&e->intent) &&
        r->command==c->command && r->operation==AP_EXECUTABLE_SOURCE &&
        r->phase==AP_COMMAND_DONE && r->original_count==c->original_count &&
        r->identity.provider==c->provider && !r->identity.object && !r->identity.namespace &&
        !r->creation && !r->cookie && !r->returned && !r->reserved &&
        !r->state.receive_timeout_ticks && !r->state.send_timeout_ticks && !r->state.lowat &&
        !r->state.receive_buffer && !r->state.peek_offset && !r->state.socket_option_memory &&
        !r->state.window_clamp && !r->state.userlocks && !r->state.scaling_ratio &&
        !r->state.tcp_state && !r->state.child_spin_locked &&
        r->task==e->entered.task && r->start_boottime==e->entered.start &&
        e->phases==(AP_EXE_ENTERED|AP_EXE_OBSERVED|AP_EXE_RETURNED) && !e->problem &&
        !e->ptrace_return && !e->find_enter_return && !e->find_exit_return &&
        ap_executable_observation_valid(&e->intent,&e->entered,anchor) &&
        ap_executable_observation_valid(&e->intent,&e->returned,anchor) &&
        ap_executable_observations_same(&e->entered,&e->returned);
}
#ifndef __BPF__
int ap_prepare_executable_source(struct ap_session *,int,u64,u64,u64,u64,u64,u64,u64,u64 *);
int ap_collect_executable_source(struct ap_session *,int,u64,struct ap_command_result *,struct ap_executable_source *);
#endif
#endif
