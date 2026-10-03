/* SPDX-License-Identifier: GPL-2.0 */
#ifndef HERMIT_EXECUTABLE_SOURCE_BPF_H
#define HERMIT_EXECUTABLE_SOURCE_BPF_H
#include "executable-source.h"
ARRAY(executable_sources,struct ap_executable_source,AP_COMMANDS);
#if defined(AP_GROUPED_PROVIDER) && defined(AP_FTRACE_PROVIDER)
static long (*exe_find_vma)(struct task_struct *,u64,void *,void *,u64)=(void *)BPF_FUNC_find_vma;
static long (*exe_read_user)(void *,u32,const void *)=(void *)BPF_FUNC_probe_read_user;
struct ap_exe_vma_context {
    struct ap_executable_source *receipt;
    struct ap_executable_observation *observation;
    u64 anchor,called;
};
static long exe_observe_vma(struct task_struct *task,struct vm_area_struct *vma,
                            struct ap_exe_vma_context *ctx) {
    struct ap_executable_source *e=ctx->receipt;
    struct ap_executable_observation *o=ctx->observation;
    if(ctx->called++) {e->problem|=AP_EXE_CONTEXT;return 0;}
    struct mm_struct *mm=CORE(vma->vm_mm);
    struct file *file=CORE(vma->vm_file),*exe=mm?CORE(mm->exe_file):0;
    struct inode *inode=file?CORE(file->f_inode):0;
    struct address_space *mapping=file?CORE(file->f_mapping):0;
    struct super_block *sb=inode?CORE(inode->i_sb):0;
    if(!mm || CORE(task->mm)!=mm || !file || file!=exe || !inode || !mapping ||
       CORE(mapping->host)!=inode || !sb || CORE(vma->vm_userfaultfd_ctx.ctx)) {
        e->problem|=AP_EXE_BACKING;return 0;
    }
    o->mm=(u64)mm;o->file=(u64)file;o->exe_file=(u64)exe;o->inode=(u64)inode;o->mapping=(u64)mapping;
    o->fops=(u64)CORE(file->f_op);o->vm_ops=(u64)CORE(vma->vm_ops);
    o->filesystem=CORE(sb->s_magic);o->device=CORE(sb->s_dev);
    o->inode_number=CORE(inode->i_ino);o->file_size=CORE(inode->i_size);
    o->vm_start=CORE(vma->vm_start);o->vm_end=CORE(vma->vm_end);
    o->vm_pgoff=CORE(vma->vm_pgoff);o->vm_flags=CORE(vma->vm_flags);
    o->file_mode=CORE(file->f_mode);o->inode_mode=CORE(inode->i_mode);
    o->writecount=CORE(inode->i_writecount.counter);
    if(!ap_executable_observation_valid(&e->intent,o,ctx->anchor))e->problem|=AP_EXE_GEOMETRY;
    return 0;
}
static __attribute__((noinline)) s64 exe_observe(struct task_struct *task,
        struct ap_executable_source *e,struct ap_executable_observation *o) {
    u32 zero=0;struct ap_config *config=lookup(&ap_config_map,&zero);
    if(!config || config->provider!=incarnation() || config->anchor_phase!=AP_GROUPED_ANCHOR_ACTIVE) {
        e->problem|=AP_EXE_CONTEXT;return -1;
    }
    o->task=fd_target_task(task);o->start=CORE(task->start_boottime);
    o->tracer=pid_tgid();o->tracer_start=CORE(current_task()->start_boottime);
    struct iovec iov={0};
    if(exe_read_user(&iov,sizeof(iov),(void *)e->intent.iovec)) {
        e->problem|=AP_EXE_READ;return -1;
    }
    o->iovec_base=(u64)iov.iov_base;o->iovec_length=iov.iov_len;
    if(!ap_fd_enrollment_context(CORE(task->__state),CORE(task->jobctl),CORE(task->ptrace),
        CORE(task->parent)==current_task(),1,0)) {
        e->problem|=AP_EXE_CONTEXT;return -1;
    }
    struct ap_exe_vma_context context={e,o,config->anchor_ip,0};
    s64 result=exe_find_vma(task,e->intent.address,exe_observe_vma,&context,0);
    if(result || context.called!=1)e->problem|=AP_EXE_HELPER;
    return result;
}
#endif
/* Called only by the existing actual ptrace_request entry/return observers.
 * No opcode from a helper return or userspace callback synthesizes this event. */
static __attribute__((noinline)) int executable_source_enter(u64 *ctx,struct ap_task_command *c) {
    /* Only the armed PRSTATUS GET belongs to this command. The backend's
     * separate XSTATE capture may run before collection of this receipt. */
    if(ctx[1]!=AP_PTRACE_GETREGSET || ctx[2]!=AP_NT_PRSTATUS)return 0;
    struct task_struct *task=(struct task_struct *)ctx[0];
    u32 slot=ap_command_slot(c->command);
    struct ap_executable_source *e=lookup(&executable_sources,&slot);
    struct ap_command_result *r=result(c->command);
    if(!e || !ap_executable_intent_matches(c,&e->intent) || e->phases || e->problem ||
       !ap_command_reservation_matches(c,r,slot) ||
       __sync_val_compare_and_swap(&r->phase,AP_COMMAND_READY,AP_COMMAND_RUNNING)!=AP_COMMAND_READY) {
        ap_fail(AP_BAD_COMMAND);return 0;
    }
    e->phases=AP_EXE_ENTERED;
    r->task=fd_target_task(task);r->start_boottime=CORE(task->start_boottime);
    r->identity.provider=incarnation();
    if(ctx[1]!=AP_PTRACE_GETREGSET || ctx[2]!=AP_NT_PRSTATUS || ctx[3]!=e->intent.iovec) {
        e->problem|=AP_EXE_CONTEXT;return 0;
    }
#if defined(AP_GROUPED_PROVIDER) && defined(AP_FTRACE_PROVIDER)
    e->find_enter_return=exe_observe(task,e,&e->entered);
    if(!e->problem)e->phases|=AP_EXE_OBSERVED;
#else
    e->problem|=AP_EXE_BACKING;
#endif
    return 0;
}
static __attribute__((noinline)) int executable_source_returned(u64 *ctx,struct ap_task_command *c) {
    if(ctx[1]!=AP_PTRACE_GETREGSET || ctx[2]!=AP_NT_PRSTATUS)return 0;
    struct task_struct *task=(struct task_struct *)ctx[0];u32 slot=ap_command_slot(c->command);
    struct ap_executable_source *e=lookup(&executable_sources,&slot);
    struct ap_command_result *r=result(c->command);
    if(!e || !r || !ap_executable_intent_matches(c,&e->intent) ||
       r->phase!=AP_COMMAND_RUNNING || !(e->phases&AP_EXE_ENTERED) || (e->phases&AP_EXE_RETURNED) ||
       r->task!=fd_target_task(task) || r->start_boottime!=CORE(task->start_boottime)) {
        ap_fail(AP_BAD_COMMAND);return 0;
    }
    if(ctx[1]!=AP_PTRACE_GETREGSET || ctx[2]!=AP_NT_PRSTATUS || ctx[3]!=e->intent.iovec)
        e->problem|=AP_EXE_CONTEXT;
#if defined(AP_GROUPED_PROVIDER) && defined(AP_FTRACE_PROVIDER)
    e->find_exit_return=exe_observe(task,e,&e->returned);
    if(!ap_executable_observations_same(&e->entered,&e->returned))e->problem|=AP_EXE_CONTEXT;
#else
    e->problem|=AP_EXE_BACKING;
#endif
    e->ptrace_return=(s64)ctx[4];r->returned=(s32)e->ptrace_return;
    if((s64)r->returned!=e->ptrace_return)e->problem|=AP_EXE_CONTEXT;
    e->phases|=AP_EXE_RETURNED;
    publish_result(r);return 0;
}
#endif
