/* SPDX-License-Identifier: GPL-2.0 */
/* File-first refusal is deliberately outside command/task lookup. Another
 * process with an alias cannot hide consumption by lacking a command row. */
#include "retirement-target.h"
/* Native copy5 uses the SAME fd_files row. Unsupported consumers may
 * only atomically OR poison, including when no command/Call exists. Preserve
 * the original provider-wide sticky failure as a separate mandatory signal. */
static inline void stream_membership_poison(struct ap_fd_file *known) {
#if defined(AP_NATIVE_COPY_VERSION) && AP_NATIVE_COPY_VERSION == 5
    ap_stream_frontier_poison(known);
#else
    (void)known;
#endif
}
static inline int stream_membership_frontier(struct ap_fd_file *known) {
#if defined(AP_NATIVE_COPY_VERSION) && AP_NATIVE_COPY_VERSION == 5
    return ap_stream_frontier_member(known,known->identity);
#else
    (void)known;return 1;
#endif
}
static __attribute__((noinline)) struct file *stream_membership_file(struct pt_regs *ctx,u64 cookie) {
    struct socket *socket=0;struct file *file=0;
    if(cookie==1 || cookie==2 || cookie==3 || cookie==8 || cookie==17)
        socket=(struct socket *)CORE(ctx->di);
    else if(cookie==7) {
        struct unix_stream_read_state *state=(void *)CORE(ctx->di);
        if(!state || fd_read_kernel(&socket,sizeof(socket),CORE(&state->socket)))goto failed;
    } else if(cookie>=5 && cookie<=20) {
        struct sock *sk=(struct sock *)CORE(ctx->di);
        if(!sk || fd_read_kernel(&socket,sizeof(socket),CORE(&sk->sk_socket)))goto failed;
    } else goto failed;
    if(!socket)return 0;
    if(fd_read_kernel(&file,sizeof(file),CORE(&socket->file)))goto failed;
    return file;
failed:
    fd_problem(AP_FD_IDENTITY);return 0;
}
static __attribute__((noinline)) int stream_membership_message(
        struct ap_fd_call *call,struct msghdr *msg,u64 length,s32 flags) {
    if(!msg || call->original.stream_copy.iterator!=(u64)CORE(&msg->msg_iter) ||
       length!=call->original.stream_copy.summary.initial_count)return 0;
    struct ap_task_command *c=command();
    return c && (ap_original_recv(c->operation)?flags==c->expected_option:((u32)flags&~0x40U)==0);
}
static __attribute__((noinline)) int stream_membership_entry(struct pt_regs *ctx,u64 cookie) {
    struct file *file=stream_membership_file(ctx,cookie);
    u64 raw=(u64)file;struct ap_fd_file *known=file?lookup(&fd_files,&raw):0;
    if(!known)return cookie>=1 && cookie<=3;
    if(!known->identity || !stream_membership_frontier(known)) {
        stream_membership_poison(known);fd_problem(AP_FD_IDENTITY);return 0;}
    struct ap_task_command *c=command();
    if(!c || !ap_original_receive(c->operation)) {
        stream_membership_poison(known);fd_problem(AP_FD_OUTCOME);return 0;}
    struct ap_fd_call *call=stream_copy_call();
    if(!call && cookie<=3 && ap_original_recv(c->operation))call=stream_copy_recv_pending(c);
    if(!call || (call->selected_file && (call->selected_file!=known->identity ||
       (call->selection.word&~3ULL)!=raw))) {
        stream_membership_poison(known);fd_problem(AP_FD_OUTCOME);return 0;}
    if(cookie<=3) {
        if(call->security_socket) {
            stream_membership_poison(known);stream_copy_problem(call,AP_FD_DUPLICATE);return 0;}
        return 1;
    }
    if(!call->selected_file || !call->original.selection.ready)goto failed;
    struct ap_stream_copy_state *copy=&call->original.stream_copy;
    const u64 word=call->security_socket,protocol=ap_stream_frame_protocol(word),stack=CORE(ctx->sp);
    if(cookie==5) {
        if((protocol!=1 && protocol!=2) || ap_stream_frame_stage(word)!=2 ||
           fd_function_ip(ctx)!=ap_stream_tcp_target(protocol,copy->protocol_ip) ||
           stack!=(word&~7ULL) || !stream_membership_message(call,(void *)CORE(ctx->si),CORE(ctx->dx),(s32)CORE(ctx->cx)))goto failed;
    } else if(cookie==7) {
        struct unix_stream_read_state *state=(void *)CORE(ctx->di);
        struct msghdr *msg=0;u64 pipe=0,actor=0,length=0,returned=0; s32 flags=0;u32 splice=0;
        const u64 entry=word&~7ULL,ip=copy->protocol_ip;
        if(protocol!=3 || ap_stream_frame_stage(word)!=1 || entry<56 ||
           stack!=entry-56 || (u64)state!=entry-48 || CORE(ctx->si)!=1 ||
           !ip || ip>~0ULL-0xb70 || fd_function_ip(ctx)!=ip+0x70 ||
           fd_read_kernel(&returned,sizeof(returned),(const void *)stack) || returned!=ip+0x52 ||
           fd_read_kernel(&actor,sizeof(actor),CORE(&state->recv_actor)) || actor!=ip+0xb70 ||
           fd_read_kernel(&msg,sizeof(msg),CORE(&state->msg)) ||
           fd_read_kernel(&pipe,sizeof(pipe),CORE(&state->pipe)) || pipe ||
           fd_read_kernel(&length,sizeof(length),CORE(&state->size)) ||
           fd_read_kernel(&flags,sizeof(flags),CORE(&state->flags)) ||
           fd_read_kernel(&splice,sizeof(splice),CORE(&state->splice_flags)) || splice ||
           !stream_membership_message(call,msg,length,flags))goto failed;
    } else {
        /* Cookie6's standalone locked implementation has no proved owned
         * caller. All splice/read-skb/zerocopy/BPF routes are unsupported.
         * Their native effects remain untouched; the trace becomes invalid. */
        goto failed;
    }
    u64 next=ap_stream_frame_advance(word,protocol,3);
    if(!next)goto failed;
    call->security_socket=next;return 0;
failed:
    stream_membership_poison(known);stream_copy_problem(call,AP_FD_OUTCOME);return 0;
}
/* inet_recvmsg/inet6_recvmsg entry already has the complete dispatch
 * operands. Bind the concrete sk_prot target there; the separately attached
 * concrete protocol entry must later observe that exact function and frame.
 * The old mid-instruction tail-jump probes add no fact once both ends agree. */
static __attribute__((noinline)) int stream_membership_wrapper_dispatch(
        struct ap_fd_call *call,struct socket *socket,struct msghdr *msg,u64 length,s32 flags) {
    if(!call)goto failed;
    const u64 word=call->security_socket,protocol=ap_stream_frame_protocol(word);
    struct sock *sk=0;struct proto *prot=0;u64 target=0;
    if((protocol!=1 && protocol!=2) || !ap_ftrace_role_enabled(protocol==1?16:17) ||
       ap_stream_frame_stage(word)!=1 ||
       !socket || fd_read_kernel(&sk,sizeof(sk),CORE(&socket->sk)) || !sk ||
       fd_read_kernel(&prot,sizeof(prot),CORE(&sk->__sk_common.skc_prot)) || !prot ||
       fd_read_kernel(&target,sizeof(target),CORE(&prot->recvmsg)) ||
       !target || target!=ap_stream_tcp_target(protocol,call->original.stream_copy.protocol_ip) ||
       !stream_membership_message(call,msg,length,flags))goto failed;
    u64 next=ap_stream_frame_advance(word,protocol,2);if(!next)goto failed;
    call->security_socket=next;return 1;
failed:
    return 0;
}
static __attribute__((always_inline)) inline int stream_membership_wrapper_admitted(
        struct ap_fd_file *known,int accepted) {
    if(!accepted && known)stream_membership_poison(known);
    return accepted;
}
#ifndef AP_FTRACE_PROVIDER
static __attribute__((noinline)) int stream_membership_dispatch(struct pt_regs *ctx) {
    /* Actual dispatcher RDI is sock*, after socket->sk. Resolve the tracked
     * file before looking for the current task's command or Call. */
    struct file *file=stream_membership_file(ctx,5);u64 raw=(u64)file;
    struct ap_fd_file *known=file?lookup(&fd_files,&raw):0;
    if(!known)return 0;
    if(!known->identity || !stream_membership_frontier(known)) {
        stream_membership_poison(known);fd_problem(AP_FD_OUTCOME);return 0;
    }
    struct ap_fd_call *call=stream_copy_call();
    if(!known->identity || !call || call->selected_file!=known->identity ||
       (call->selection.word&~3ULL)!=raw) {
        stream_membership_poison(known);fd_problem(AP_FD_OUTCOME);return 0;}
    const u64 word=call->security_socket,protocol=ap_stream_frame_protocol(word);
    const u64 stage=ap_stream_frame_stage(word);
    const u64 checked_word=stage==2?(word&~7ULL)|protocol:word;
    if((stage!=1 && stage!=2) || !ap_stream_dispatch_site(checked_word,call->original.stream_copy.protocol_ip,
          fd_attach_cookie(ctx),CORE(ctx->ip),CORE(ctx->sp),CORE(ctx->ax)) ||
       !stream_membership_message(call,(void *)CORE(ctx->si),CORE(ctx->dx),(s32)CORE(ctx->cx))) {
        stream_membership_poison(known);stream_copy_problem(call,AP_FD_OUTCOME);return 0;
    }
    if(stage==2)return 0; /* Transitional classic observer confirms only. */
    u64 next=ap_stream_frame_advance(word,protocol,2);
    if(!next) {stream_membership_poison(known);stream_copy_problem(call,AP_FD_DUPLICATE);return 0;}
    call->security_socket=next;return 0;
}
#endif
