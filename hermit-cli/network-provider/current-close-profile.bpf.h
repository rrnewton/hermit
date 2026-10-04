/* SPDX-License-Identifier: GPL-2.0 */
#ifndef HERMIT_CURRENT_CLOSE_PROFILE_BPF_H
#define HERMIT_CURRENT_CLOSE_PROFILE_BPF_H
#ifdef AP_CURRENT_CLOSE_PROFILE_ENABLED
#include "current-close-profile.h"
ARRAY(current_close_profiles,struct ap_close_profile,AP_COMMANDS);
static long (*close_read_user)(void *,u32,const void *)=(void *)BPF_FUNC_probe_read_user;
/* All pointers below are borrowed under the actual ptrace freeze. No get_file,
 * fdget, socket helper or artificial reference changes the final fput. */
static __attribute__((noinline)) int close_observe(struct task_struct *task,
        struct ap_close_profile *e,struct ap_close_profile_observation *o) {
    u32 zero=0;struct ap_config *config=lookup(&ap_config_map,&zero);
    if(!config || config->provider!=incarnation() || config->anchor_phase!=AP_GROUPED_ANCHOR_ACTIVE) {
        e->problem|=AP_CLOSE_CONTEXT;return -1;
    }
    struct files_struct *files=0;struct fdtable *fdt=0;struct file **array=0,*file=0;
    struct mm_struct *mm=0;struct task_struct *parent=0;
    u32 state=0,ptrace=0;unsigned long jobctl=0;
#define CLOSE_READ(dst,src) do { if(fd_read_kernel(&(dst),sizeof(dst),CORE(&(src)))) { \
        e->problem|=AP_CLOSE_READ;return -1; } } while(0)
    o->task=fd_target_task(task);o->tracer=pid_tgid();
    CLOSE_READ(o->start,task->start_boottime);CLOSE_READ(o->tracer_start,current_task()->start_boottime);
    CLOSE_READ(files,task->files);CLOSE_READ(mm,task->mm);CLOSE_READ(parent,task->parent);
    CLOSE_READ(state,task->__state);CLOSE_READ(jobctl,task->jobctl);CLOSE_READ(ptrace,task->ptrace);
    if(!files || !mm) {e->problem|=AP_CLOSE_CONTEXT;return -1;}
    CLOSE_READ(o->files_refs,files->count.counter);
    if(!ap_fd_enrollment_context(state,jobctl,ptrace,parent==current_task(),o->files_refs,1)) {
        e->problem|=AP_CLOSE_CONTEXT;return -1;
    }
    o->mm=(u64)mm;o->raw_table=(u64)files;o->table=fd_table(files,0);
    if(o->table!=e->intent.expected_table) {e->problem|=AP_CLOSE_CONTEXT;return -1;}
    CLOSE_READ(fdt,files->fdt);
    if(!fdt) {e->problem|=AP_CLOSE_CONTEXT;return -1;}
    CLOSE_READ(o->max_fds,fdt->max_fds);CLOSE_READ(array,fdt->fd);
    if(!array || !o->max_fds || o->max_fds>AP_FD_FILES || (u32)e->intent.fd>=o->max_fds) {
        e->problem|=AP_CLOSE_CONTEXT;return -1;
    }
    if(fd_read_kernel(&file,sizeof(file),array+(u32)e->intent.fd) || !file) {
        e->problem|=AP_CLOSE_READ;return -1;
    }
    o->raw_file=(u64)file;
    struct ap_fd_file *known=lookup(&fd_files,&o->raw_file);
    if(!known || known->identity!=e->intent.expected_file) {e->problem|=AP_CLOSE_CONTEXT;return -1;}
    o->file=known->identity;
    /* Count every slot, including holes. The selected file reference count
     * must be exactly these genuine guest aliases, not an observer-held FD. */
    for(u32 fd=0;fd<AP_FD_FILES;fd++) {
        if(fd>=o->max_fds)break;
        struct file *other=0;
        if(fd_read_kernel(&other,sizeof(other),array+fd)) {e->problem|=AP_CLOSE_READ;return -1;}
        if(other==file)o->aliases++;
    }
    CLOSE_READ(o->file_ref_raw,file->f_ref.refcnt.counter);
    if(!ap_close_refs(o->file_ref_raw,o->aliases,&o->file_refs))e->problem|=AP_CLOSE_UNSUPPORTED;
    struct inode *inode=0;const struct file_operations *fops=0;
    CLOSE_READ(inode,file->f_inode);CLOSE_READ(fops,file->f_op);
    o->inode=(u64)inode;o->file_ops=(u64)fops;
    if(!inode || !fops) {e->problem|=AP_CLOSE_CONTEXT;return -1;}
    CLOSE_READ(o->file_flush,fops->flush);CLOSE_READ(o->file_release,fops->release);
    if(o->file_ops!=ap_grouped_image_address(config->anchor_ip,AP_CLOSE_SOCKET_FILE_OPS_IMAGE)) {
        e->problem|=AP_CLOSE_UNSUPPORTED;return 0;
    }
    struct socket *socket=0;struct sock *sk=0;struct file *back_file=0;
    const struct proto_ops *ops=0;struct proto *prot=0;struct socket *back_socket=0;
    CLOSE_READ(socket,file->private_data);
    if(!socket) {e->problem|=AP_CLOSE_CONTEXT;return -1;}
    CLOSE_READ(back_file,socket->file);CLOSE_READ(sk,socket->sk);CLOSE_READ(ops,socket->ops);
    if(back_file!=file || !sk || !ops) {e->problem|=AP_CLOSE_CONTEXT;return -1;}
    CLOSE_READ(back_socket,sk->sk_socket);
    if(back_socket!=socket) {e->problem|=AP_CLOSE_CONTEXT;return -1;}
    o->socket=(u64)socket;o->sk=(u64)sk;o->socket_ops=(u64)ops;
    CLOSE_READ(o->socket_release,ops->release);
    u16 family=0,type=0,protocol=0;
    CLOSE_READ(family,sk->__sk_common.skc_family);CLOSE_READ(type,sk->sk_type);CLOSE_READ(protocol,sk->sk_protocol);
    o->family=family;o->type=type;o->protocol=protocol;
    if(family!=AP_AF_INET || type!=1 || protocol!=AP_IPPROTO_TCP) {
        e->problem|=AP_CLOSE_UNSUPPORTED;return 0;
    }
    CLOSE_READ(prot,sk->__sk_common.skc_prot);o->protocol_ops=(u64)prot;
    if(!prot) {e->problem|=AP_CLOSE_CONTEXT;return -1;}
    CLOSE_READ(o->protocol_close,prot->close);
    unsigned long flags=0;CLOSE_READ(flags,sk->__sk_common.skc_flags);
    o->linger=(flags>>SOCK_LINGER)&1;CLOSE_READ(o->linger_ticks,sk->sk_lingertime);
    struct inet_connection_sock *icsk=(struct inet_connection_sock *)sk;
    CLOSE_READ(o->ulp_ops,icsk->icsk_ulp_ops);CLOSE_READ(o->ulp_data,icsk->icsk_ulp_data);
    /* Checked CO-RE bitfield read: failed reads cannot become repair=false. */
    struct tcp_sock *tcp=(struct tcp_sock *)sk;u64 raw=0;
    u32 size=__builtin_preserve_field_info(tcp->repair,1);
    u32 offset=__builtin_preserve_field_info(tcp->repair,0);
    if(!size || size>sizeof(raw) || fd_read_kernel(&raw,size,(void *)tcp+offset)) {
        e->problem|=AP_CLOSE_READ;return -1;
    }
    o->repair=(raw<<__builtin_preserve_field_info(tcp->repair,4))>>__builtin_preserve_field_info(tcp->repair,5);
    if(!ap_close_observation_finite(o,config->anchor_ip))e->problem|=AP_CLOSE_UNSUPPORTED;
#undef CLOSE_READ
    return 0;
}
static __attribute__((noinline)) int current_close_profile_enter(u64 *ctx,struct ap_task_command *c) {
    if(ctx[1]!=AP_PTRACE_GETREGSET || ctx[2]!=AP_NT_PRSTATUS)return 0;
    struct task_struct *task=(struct task_struct *)ctx[0];u32 slot=ap_command_slot(c->command);
    struct ap_close_profile *e=lookup(&current_close_profiles,&slot);
    struct ap_command_result *r=result(c->command);
    if(!e || !ap_close_intent_matches(c,&e->intent) || e->phases || e->problem ||
       !ap_command_reservation_matches(c,r,slot) ||
       __sync_val_compare_and_swap(&r->phase,AP_COMMAND_READY,AP_COMMAND_RUNNING)!=AP_COMMAND_READY) {
        ap_fail(AP_BAD_COMMAND);return 0;
    }
    e->phases=AP_CLOSE_ENTERED;e->iovec=ctx[3];
    r->task=fd_target_task(task);r->start_boottime=CORE(task->start_boottime);r->identity.provider=incarnation();
    struct iovec iov={0};
    if(!e->iovec || close_read_user(&iov,sizeof(iov),(void *)e->iovec) ||
       !iov.iov_base || iov.iov_len!=AP_CLOSE_REGISTER_BYTES) {e->problem|=AP_CLOSE_READ;return 0;}
    e->registers=(u64)iov.iov_base;e->register_bytes=iov.iov_len;
    if(!close_observe(task,e,&e->entered))e->phases|=AP_CLOSE_OBSERVED;
    return 0;
}
static __attribute__((noinline)) int current_close_profile_returned(u64 *ctx,struct ap_task_command *c) {
    if(ctx[1]!=AP_PTRACE_GETREGSET || ctx[2]!=AP_NT_PRSTATUS)return 0;
    struct task_struct *task=(struct task_struct *)ctx[0];u32 slot=ap_command_slot(c->command);
    struct ap_close_profile *e=lookup(&current_close_profiles,&slot);
    struct ap_command_result *r=result(c->command);
    if(!e || !r || !ap_close_intent_matches(c,&e->intent) || r->phase!=AP_COMMAND_RUNNING ||
       !(e->phases&AP_CLOSE_ENTERED) || (e->phases&AP_CLOSE_RETURNED) ||
       r->task!=fd_target_task(task) || r->start_boottime!=CORE(task->start_boottime)) {
        ap_fail(AP_BAD_COMMAND);return 0;
    }
    struct iovec iov={0};
    if(ctx[3]!=e->iovec || close_read_user(&iov,sizeof(iov),(void *)e->iovec) ||
       (u64)iov.iov_base!=e->registers || iov.iov_len!=AP_CLOSE_REGISTER_BYTES) e->problem|=AP_CLOSE_READ;
    else if(close_read_user(&e->original_nr,8,(void *)(e->registers+15*8)) ||
            close_read_user(&e->original_fd,8,(void *)(e->registers+14*8)))e->problem|=AP_CLOSE_READ;
    if(e->original_nr!=AP_CLOSE_SYSCALL || e->original_fd!=(u64)(u32)e->intent.fd)e->problem|=AP_CLOSE_CONTEXT;
    close_observe(task,e,&e->returned);
    if(!ap_close_owned_same(&e->entered,&e->returned))e->problem|=AP_CLOSE_CONTEXT;
    e->ptrace_return=(s64)ctx[4];r->returned=(s32)e->ptrace_return;
    if((s64)r->returned!=e->ptrace_return)e->problem|=AP_CLOSE_CONTEXT;
    e->phases|=AP_CLOSE_RETURNED;
    publish_result(r);return 0;
}
#endif
#endif
