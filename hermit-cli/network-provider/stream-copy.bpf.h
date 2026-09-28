/* SPDX-License-Identifier: GPL-2.0 */
/* Actual original-Read copy producer. Driver attachment/counter binding and
 * userspace same-Call drain/ACK retain each unit through actual completion. */
#include "stream-copy.h"
#if AP_NATIVE_COPY_VERSION == 5 && !defined(AP_GROUPED_PROVIDER)
#error "native copy5 requires the actual global membership and same-Call frame issuer"
#endif
struct { __uint(type,BPF_MAP_TYPE_RINGBUF);
    __uint(max_entries,AP_STREAM_COPY_RING_BYTES); } stream_copy_records SEC(".maps");
static void *(*stream_copy_reserve)(void *,u64,u64)=(void *)BPF_FUNC_ringbuf_reserve;
static void (*stream_copy_submit)(void *,u64)=(void *)BPF_FUNC_ringbuf_submit;
static void (*stream_copy_discard)(void *,u64)=(void *)BPF_FUNC_ringbuf_discard;
static long (*stream_copy_loop)(u32,void *,void *,u64)=(void *)BPF_FUNC_loop;
extern struct kmem_cache *bpf_get_kmem_cache(u64 addr) __attribute__((section(".ksyms")));
#define AP_STREAM_COPY_PHASE_MASK 255ULL
#define AP_STREAM_COPY_TRANSPORT_SHIFT 8
static __attribute__((noinline)) int stream_copy_unit_enter(struct ap_fd_call *,struct pt_regs *);
static __attribute__((noinline)) int stream_copy_unit_exit(struct ap_fd_call *,struct pt_regs *);
static __attribute__((noinline)) void stream_copy_record_init(
    struct ap_stream_copy_record *,const struct ap_fd_call *);
static __attribute__((noinline)) int stream_copy_frontier_semantics(const struct ap_fd_call *,u64);

/* A kprobe register or a pointer saved in a map is a scalar to the verifier.
 * CO-RE relocates these field addresses; probe_read_kernel performs every
 * actual dereference and a failed read refuses the custody certificate. */
struct stream_copy_skb_view {
    u64 head,data;
    u32 tail,end,size,nonlinear;
    s32 users;
};
struct stream_copy_shared_view { u64 frag_list; s32 dataref; u8 count,flags; };
static __attribute__((noinline)) int stream_copy_read_skb(
        struct sk_buff *skb,struct stream_copy_skb_view *out) {
    return skb &&
        !fd_read_kernel(&out->users,sizeof(out->users),CORE(&skb->users.refs.counter)) &&
        !fd_read_kernel(&out->head,sizeof(out->head),CORE(&skb->head)) &&
        !fd_read_kernel(&out->data,sizeof(out->data),CORE(&skb->data)) &&
        !fd_read_kernel(&out->tail,sizeof(out->tail),CORE(&skb->tail)) &&
        !fd_read_kernel(&out->end,sizeof(out->end),CORE(&skb->end)) &&
        !fd_read_kernel(&out->size,sizeof(out->size),CORE(&skb->len)) &&
        !fd_read_kernel(&out->nonlinear,sizeof(out->nonlinear),CORE(&skb->data_len));
}
static __attribute__((noinline)) int stream_copy_read_shared(
        struct skb_shared_info *shared,struct stream_copy_shared_view *out) {
    return !fd_read_kernel(&out->count,sizeof(out->count),CORE(&shared->nr_frags)) &&
        !fd_read_kernel(&out->flags,sizeof(out->flags),CORE(&shared->flags)) &&
        !fd_read_kernel(&out->frag_list,sizeof(out->frag_list),CORE(&shared->frag_list)) &&
        !fd_read_kernel(&out->dataref,sizeof(out->dataref),CORE(&shared->dataref.counter));
}

/* The positive slab result is joined to the valid skb ownership contract:
 * one skb owner, one FULL data reference, no page-backed head or nonlinear
 * payload. Header-cloned/low-half tests do not exclude payload-only aliases.
 * No mapping==NULL inference or borrowed page snapshot is payload authority. */
static __attribute__((noinline)) int stream_copy_linear_head(struct sk_buff *skb,
        u64 source,u64 length,const struct stream_copy_skb_view *view) {
    if(view->nonlinear || view->users!=1)return 0;
    const u64 head=view->head,data=view->data,tail=view->tail,end=view->end,size=view->size;
    u64 head_frag=0;
    const u32 bit_bytes=__builtin_preserve_field_info(skb->head_frag,1);
    const u32 bit_offset=__builtin_preserve_field_info(skb->head_frag,0);
    if(!bit_bytes || bit_bytes>8 || fd_read_kernel(&head_frag,bit_bytes,
          (const u8 *)skb+bit_offset))return 0;
    head_frag<<=__builtin_preserve_field_info(skb->head_frag,4);
    head_frag>>=__builtin_preserve_field_info(skb->head_frag,5);
    if(head_frag)return 0;
    if(!head || !data || end>~0ULL-head || tail>end || data<head ||
       data>head+tail || size!=head+tail-data || source<data ||
       source>head+tail || length>head+tail-source || !bpf_get_kmem_cache(head))return 0;
    struct skb_shared_info *shared=(struct skb_shared_info *)(head+end);
    struct stream_copy_shared_view info={0};
    if(!stream_copy_read_shared(shared,&info))return 0;
    return !info.count && !info.frag_list && info.dataref==1;
}

/* Qualified tcp_recvmsg_locked consumes the native TCP receive queue. TCP
 * marks externally overwritable splice fragments SHARED_FRAG; its ordinary
 * copy-to-page producer owns the copied range even when the page is shared
 * with a cache or another immutable skb. skb_try_coalesce propagates that mark.
 * ZEROCOPY/managed fragments and frag lists require separate certificates.
 * Unix's splice helper does not establish this contract and is excluded.
 * See the retained producer/sanitization source for this protocol-specific
 * invariant: a refcount or mapping==NULL is not used as its substitute. */
static __attribute__((noinline)) int stream_copy_tcp_fragments(struct sk_buff *skb,u64 offset,u64 length,
        const struct stream_copy_skb_view *view) {
    if(view->users!=1)return 0;
    const u64 head=view->head,data=view->data,tail=view->tail,end=view->end,
        size=view->size,nonlinear=view->nonlinear;
    u64 unreadable=0;
    const u32 bit_bytes=__builtin_preserve_field_info(skb->unreadable,1);
    const u32 bit_offset=__builtin_preserve_field_info(skb->unreadable,0);
    if(!bit_bytes || bit_bytes>8 || fd_read_kernel(&unreadable,bit_bytes,(const u8 *)skb+bit_offset))return 0;
    unreadable<<=__builtin_preserve_field_info(skb->unreadable,4);
    unreadable>>=__builtin_preserve_field_info(skb->unreadable,5);
    if(unreadable)return 0;
    if(!head || !data || end>~0ULL-head || tail>end || data<head || data>head+tail ||
       !nonlinear || nonlinear>size || size-nonlinear!=head+tail-data ||
       offset>size || length>size-offset)return 0;
    struct skb_shared_info *shared=(struct skb_shared_info *)(head+end);
    struct stream_copy_shared_view info={0};
    if(!stream_copy_read_shared(shared,&info))return 0;
    const u32 count=info.count;
    if(!count || count>sizeof(shared->frags)/sizeof(shared->frags[0]) ||
       info.frag_list || !ap_stream_copy_tcp_storage_flags(info.flags) || info.dataref<=0)return 0;
    if(ap_stream_copy_linear_take(offset,length,size-nonlinear)) {
        u64 head_frag=0;
        const u32 head_frag_bytes=__builtin_preserve_field_info(skb->head_frag,1);
        const u32 head_frag_offset=__builtin_preserve_field_info(skb->head_frag,0);
        if(!head_frag_bytes || head_frag_bytes>8 || fd_read_kernel(&head_frag,head_frag_bytes,
              (const u8 *)skb+head_frag_offset))return 0;
        head_frag<<=__builtin_preserve_field_info(skb->head_frag,4);
        head_frag>>=__builtin_preserve_field_info(skb->head_frag,5);
        if(!ap_stream_copy_mixed_head_owned(offset,length,size,nonlinear,
              info.dataref,head_frag))return 0;
    }
    u64 bytes=0;
    for(u32 i=0;i<sizeof(shared->frags)/sizeof(shared->frags[0]);i++) {
        if(i>=count)break;
        u64 netmem=0;u32 length=0,offset=0;
        if(fd_read_kernel(&netmem,sizeof(netmem),CORE(&shared->frags[i].netmem)) ||
           fd_read_kernel(&length,sizeof(length),CORE(&shared->frags[i].len)) ||
           fd_read_kernel(&offset,sizeof(offset),CORE(&shared->frags[i].offset)))return 0;
        /* net_iov device memory is not host-readable page storage. No page
         * ownership conclusion is drawn from this representation check. */
        if(!netmem || (netmem&1) || !length || offset>0xffffffffULL-length ||
           bytes>nonlinear || length>nonlinear-bytes)return 0;
        bytes+=length;
    }
    return bytes==nonlinear;
}
static __attribute__((noinline)) int stream_copy_unit_custody(
        struct sk_buff *skb,u64 offset,u64 length,u64 transport) {
    struct stream_copy_skb_view view={0};
    if(!stream_copy_read_skb(skb,&view))return 0;
    if(view.nonlinear)return transport==AP_STREAM_COPY_TCP &&
        stream_copy_tcp_fragments(skb,offset,length,&view);
    return view.data<=~0ULL-offset && stream_copy_linear_head(skb,view.data+offset,length,&view);
}

static __attribute__((noinline)) void stream_copy_problem(struct ap_fd_call *call,u64 problem) {
    call->original.problem|=problem;
    if(AP_NATIVE_COPY_VERSION==5) {
        const u64 pointer=call->selection.word&~3ULL;
        struct ap_fd_file *file=pointer?lookup(&fd_files,&pointer):0;
        ap_stream_frontier_poison(file);
    }
    /* A blocking original Read need not reach EXIT promptly after a failed
     * observation. Publish refusal now; the existing actual-task terminal
     * owner still has to settle its original Call and retained records. */
    fd_problem(problem);
}

static __attribute__((noinline)) struct ap_fd_call *stream_copy_call(void) {
    struct ap_task_command *c=command();
    if(!c || !ap_original_receive(c->operation))return 0;
    struct ap_invocation_key key;
    struct ap_fd_call *call=fd_actor_call(&key);
    struct ap_command_result *r=result(c->command);
    if(ap_original_recv(c->operation)) {
        if(!ap_original_recv_context(c,r,call,key.task,key.start) ||
           call->raw_table!=(u64)CORE(current_task()->files) || call->new_file!=(u64)CORE(current_task()->mm))return 0;
        return call;
    }
    if(!call || !r || call->operation!=AP_ORIGINAL_READ ||
       call->command!=c->command || r->phase!=AP_COMMAND_RUNNING ||
       r->operation!=AP_ORIGINAL_READ || r->command!=c->command ||
       r->task!=key.task || r->start_boottime!=key.start ||
       !ap_original_selection_matches(c,&call->original.selection) ||
       !ap_fd_selection_complete(&call->selection) || call->original.problem)
        return 0;
    return call;
}

/* Bind only the already-entered actual helper. This supplies no guest-table
 * enrollment and never manufactures fdget entry/return phases. Selection is
 * published after count/flags/plain-msghdr checks in the protocol callback. */
static __attribute__((noinline)) struct ap_fd_call *stream_copy_recv_pending(
    const struct ap_task_command *c) {
    struct ap_invocation_key key;struct ap_fd_call *call=fd_actor_call(&key);
    struct ap_command_result *r=result(c->command);
    if(!call || !r || call->command!=c->command || call->operation!=c->operation ||
       r->command!=c->command || r->operation!=c->operation || r->phase!=AP_COMMAND_RUNNING ||
       r->task!=key.task || r->start_boottime!=key.start || r->identity.provider!=c->provider ||
       r->original_count!=c->original_count || call->original.problem || call->original.complete ||
       call->raw_table!=(u64)CORE(current_task()->files) || call->new_file!=(u64)CORE(current_task()->mm) ||
       call->original.selection.ready || call->selected_file || call->selection.word ||
       call->selection.entered || call->selection.returned || call->entry_stack || call->file_entry_ip ||
       call->copied_address || call->security_socket) {
        fd_problem(AP_FD_IDENTITY);return 0;
    }
    return call;
}
static __attribute__((noinline)) int stream_copy_recv_select(struct ap_fd_call *call,
    const struct ap_task_command *c,struct file *file) {
    u64 identity=fd_file(file);
    struct ap_original_selection selected=call->original.selection;
    selected.file=identity;selected.fdput_flags=0;selected.ready=1;
    if(!identity || !ap_original_selection_matches(c,&selected))return 0;
    call->selected_file=identity;call->selection.word=(u64)file;
    call->original.selection.file=identity;return 1;
}
static __attribute__((noinline)) int stream_copy_plain_message(struct msghdr *msg,u64 expected_name,
        int unix_name_completed) {
    struct ap_stream_message_view view={0};
    return !fd_read_kernel(&view.name,sizeof(view.name),CORE(&msg->msg_name)) &&
        !fd_read_kernel(&view.name_length,sizeof(view.name_length),CORE(&msg->msg_namelen)) &&
        !fd_read_kernel(&view.control,sizeof(view.control),CORE(&msg->msg_control)) &&
        !fd_read_kernel(&view.length,sizeof(view.length),CORE(&msg->msg_controllen)) &&
        !fd_read_kernel(&view.flags,sizeof(view.flags),CORE(&msg->msg_flags)) &&
        ap_stream_message_plain(&view,expected_name,unix_name_completed);
}
static __attribute__((noinline)) u64 stream_copy_recvmsg_scratch(
        const struct ap_task_command *c,struct pt_regs *ctx,struct msghdr *msg) {
    return ap_stream_recvmsg_scratch(fd_attach_cookie(ctx),fd_function_ip(ctx),
        c->generation_before,CORE(ctx->sp),CORE(&msg->msg_name),fd_read_kernel);
}
#ifdef AP_GROUPED_PROVIDER
#include "stream-membership.bpf.h"
#endif

/* sock_read_iter inlines sock_recvmsg on the qualified image. The driver binds
 * these callbacks to actual inet_recvmsg/inet6_recvmsg/unix_stream_recvmsg
 * protocol entries, which share (socket,msghdr,len,flags), not to that unused
 * out-of-line wrapper. The selected original file remains referenced here. */
SEC("kprobe.multi") int fd_stream_copy_protocol_enter(struct pt_regs *ctx) {
#ifdef AP_GROUPED_PROVIDER
    u64 membership_cookie=fd_attach_cookie(ctx);
    if(membership_cookie!=4 && (!stream_membership_entry(ctx,membership_cookie) || membership_cookie>3))return 0;
#endif
    /* Cookie4 has skb arguments, not socket arguments. Never reinterpret it. */
    if(fd_attach_cookie(ctx)==4) {
        struct ap_fd_call *unit_call=stream_copy_call();
        return unit_call?stream_copy_unit_enter(unit_call,ctx):0;
    }
    struct socket *socket=(struct socket *)CORE(ctx->di);
    struct msghdr *msg=(struct msghdr *)CORE(ctx->si);
    struct file *file=0;
    if(!socket || !msg || fd_read_kernel(&file,sizeof(file),CORE(&socket->file))) {
        /* Global membership must cover every tracked bypass before effects;
         * inability to read an actual protocol file cannot be treated as idle. */
        if(AP_NATIVE_COPY_VERSION==5)fd_problem(AP_FD_IDENTITY);
        return 0;
    }
    const u64 pointer=(u64)file;
    struct ap_fd_file *known=pointer?lookup(&fd_files,&pointer):0;
    if(AP_NATIVE_COPY_VERSION==5 && known &&
       !ap_stream_frontier_member(known,known->identity)) {
        fd_problem(AP_FD_IDENTITY);return 0;
    }
    /* Lookup the actual file before testing command membership. A tracked
     * unowned consumer poisons this incarnation permanently, including an
     * unenrolled file; a later allowed command cannot silently invent zero. */
    struct ap_fd_call *call=stream_copy_call();
    struct ap_task_command *c=command();
    const int helper=c && ap_original_recv(c->operation);
    if(!call && helper)call=stream_copy_recv_pending(c);
    if(!call) {
        if(AP_NATIVE_COPY_VERSION==5 && known) {
            ap_stream_frontier_poison(known);fd_problem(AP_FD_IDENTITY);
        }
        return 0;
    }
    if(AP_NATIVE_COPY_VERSION==5 && !known) {
        stream_copy_problem(call,AP_FD_IDENTITY);return 0;
    }
    if(!helper && (call->selection.word&~3ULL)!=(u64)file) {
        if(AP_NATIVE_COPY_VERSION==5) {
            ap_stream_frontier_poison(known);stream_copy_problem(call,AP_FD_IDENTITY);
        }
        return 0;
    }
    if(!file || (!helper && (!call->selected_file || call->selected_file!=fd_file(file)))) {
        stream_copy_problem(call,AP_FD_IDENTITY);return 0;
    }
    struct ap_stream_copy_state *copy=&call->original.stream_copy;
    u64 expected_name=0;
    if(helper && c->operation==AP_ORIGINAL_RECVMSG_CALL) {
        expected_name=stream_copy_recvmsg_scratch(c,ctx,msg);
        if(!expected_name) {stream_copy_problem(call,AP_FD_OUTCOME);return 0;}
    }
    u64 count=0;
    if(fd_read_kernel(&count,sizeof(count),CORE(&msg->msg_iter.count))) {
        stream_copy_problem(call,AP_FD_OUTCOME);return 0;
    }
    if(copy->summary.version || count>AP_READ_MAX_COUNT ||
       count>call->original.selection.original_count || CORE(ctx->dx)!=count ||
       (helper && (count!=c->original_count || !stream_copy_plain_message(msg,expected_name,0)))
#ifdef AP_GROUPED_PROVIDER
       || (!helper && !stream_copy_plain_message(msg,0,0))
#endif
       ) {
        stream_copy_problem(call,AP_FD_OUTCOME);return 0;
    }
    /* This operation's otherwise unused private word retains only the
     * authenticated kernel scratch while the paired protocol is active. */
    if(helper)call->copied_address=expected_name;
    copy->summary.version=AP_NATIVE_COPY_VERSION;
    copy->summary.initial_count=count;
    copy->iterator=(u64)CORE(&msg->msg_iter);
    copy->protocol_ip=fd_function_ip(ctx);
    const u64 protocol=fd_attach_cookie(ctx);
    /* Original Read has no receive flag operand. Native sock_read_iter may
     * supply MSG_DONTWAIT from the actual file/kiocb; it cannot request PEEK,
     * TRUNC, OOB or DEVMEM semantics through this scalar boundary. */
    if(protocol<1 || protocol>3 ||
       (helper ? (s32)CORE(ctx->cx)!=c->expected_option : ((u32)CORE(ctx->cx)&~0x40U)!=0) ||
       (helper && !stream_copy_recv_select(call,c,file))) {
        stream_copy_problem(call,AP_FD_OUTCOME);return 0;
    }
#ifdef AP_GROUPED_PROVIDER
    u64 frame=ap_stream_frame_begin(protocol,CORE(ctx->sp));
    if(!frame || call->security_socket) {stream_copy_problem(call,AP_FD_IDENTITY);return 0;}
    call->security_socket=frame;
    if((protocol==1 || protocol==2) && !stream_membership_wrapper_admitted(known,
          stream_membership_wrapper_dispatch(call,socket,msg,CORE(ctx->dx),(s32)CORE(ctx->cx)))) {
        stream_copy_problem(call,AP_FD_OUTCOME);return 0;
    }
#endif
    copy->copy_active=(protocol==3?AP_STREAM_COPY_UNIX:AP_STREAM_COPY_TCP)
        <<AP_STREAM_COPY_TRANSPORT_SHIFT;
    if(AP_NATIVE_COPY_VERSION==5 && !stream_copy_frontier_semantics(call,
            copy->copy_active>>AP_STREAM_COPY_TRANSPORT_SHIFT)) {
        stream_copy_problem(call,AP_FD_OUTCOME);return 0;
    }
    if(helper && __sync_val_compare_and_swap(&call->original.selection.ready,0,1)!=0)
        stream_copy_problem(call,AP_FD_DUPLICATE);
    return 0;
}

SEC("kretprobe.multi") int fd_stream_copy_protocol_exit(struct pt_regs *ctx) {
    struct ap_fd_call *call=stream_copy_call();if(!call)return 0;
    if(fd_attach_cookie(ctx)==4)return stream_copy_unit_exit(call,ctx);
    struct ap_stream_copy_state *copy=&call->original.stream_copy;
    if(!copy->iterator)return 0;
    if(copy->protocol_ip!=fd_function_ip(ctx) || (copy->copy_active&AP_STREAM_COPY_PHASE_MASK) ||
       copy->skb || copy->source || copy->requested || copy->before_count || copy->copied ||
       copy->summary.protocol_complete || copy->callback.source || copy->callback.count) {
        stream_copy_problem(call,AP_FD_OUTCOME);return 0;
    }
#ifdef AP_GROUPED_PROVIDER
    if(ap_stream_frame_stage(call->security_socket)!=3 ||
       ap_stream_frame_protocol(call->security_socket)!=fd_attach_cookie(ctx)) {
        stream_copy_problem(call,AP_FD_OUTCOME);return 0;
    }
#endif
    struct iov_iter *iterator=(struct iov_iter *)copy->iterator;
    u64 count=0;s64 returned=ap_stream_copy_protocol_return(CORE(ctx->ax));
    if(fd_read_kernel(&count,sizeof(count),CORE(&iterator->count))) {
        stream_copy_problem(call,AP_FD_OUTCOME);return 0;
    }
    struct msghdr *msg=(struct msghdr *)((u8 *)iterator-
        __builtin_preserve_field_info(((struct msghdr *)0)->msg_iter,0));
    if(count>copy->summary.initial_count || returned< -4095 ||
       returned>(s64)copy->summary.initial_count ||
       (ap_original_recv(call->operation) && !stream_copy_plain_message(msg,call->copied_address,
            call->operation==AP_ORIGINAL_RECVMSG_CALL &&
            (copy->copy_active>>AP_STREAM_COPY_TRANSPORT_SHIFT)==AP_STREAM_COPY_UNIX))
#ifdef AP_GROUPED_PROVIDER
       || (!ap_original_recv(call->operation) && !stream_copy_plain_message(msg,0,0))
#endif
       ) {
        stream_copy_problem(call,AP_FD_OUTCOME);return 0;
    }
    if(AP_NATIVE_COPY_VERSION==5 && !stream_copy_frontier_semantics(call,
            copy->copy_active>>AP_STREAM_COPY_TRANSPORT_SHIFT)) {
        stream_copy_problem(call,AP_FD_OUTCOME);return 0;
    }
    copy->summary.final_count=count;
    copy->summary.protocol_returned=(u64)returned;
    copy->summary.protocol_complete=1;
    if(ap_original_recv(call->operation))call->copied_address=0;
#ifdef AP_GROUPED_PROVIDER
    call->security_socket=0;
#endif
    copy->iterator=0;copy->protocol_ip=0;copy->copy_active=0;
    return 0;
}

/* This check is repeated inside every paired locked attempt. Protocol entry
 * alone precedes the receive lock and cannot freeze a mutable PEEK_OFF/profile.
 * General guest PEEK and urgent-data layouts need separate audited issuers. */
static __attribute__((noinline)) int stream_copy_frontier_semantics(
        const struct ap_fd_call *call,u64 transport) {
    struct file *file=(struct file *)(call->selection.word&~3ULL);
    struct socket *socket=0;struct sock *sk=0;
    if(!file || fd_read_kernel(&socket,sizeof(socket),CORE(&file->private_data)) ||
       !socket || fd_read_kernel(&sk,sizeof(sk),CORE(&socket->sk)) || !sk)return 0;
    if(call->operation==AP_ORIGINAL_RECVMSG_CALL) {
        s32 peek=0;
        if(fd_read_kernel(&peek,sizeof(peek),CORE(&sk->sk_peek_off)) || peek!=-1)return 0;
    }
    if(transport==AP_STREAM_COPY_TCP) {
        u16 urgent=0;struct tcp_sock *tcp=(struct tcp_sock *)sk;
        if(fd_read_kernel(&urgent,sizeof(urgent),CORE(&tcp->urg_data)) || urgent)return 0;
    }
    return 1;
}
/* The frame word is issued only by the actual primary protocol, exact
 * dispatcher and concrete locked implementation. Protocol hook tags are a
 * different domain from transport: inet1/inet6 2 are TCP1, Unix3 is Unix2.
 * Merely observing transport or a later protocol return cannot issue Begin. */
static __attribute__((noinline)) int stream_copy_frontier_locked_frame(
        const struct ap_fd_call *call,u64 transport) {
#ifdef AP_GROUPED_PROVIDER
    return ap_stream_frontier_frame_valid(ap_stream_frame_protocol(call->security_socket),
        ap_stream_frame_stage(call->security_socket),transport);
#else
    (void)call;(void)transport;return 0;
#endif
}
static __attribute__((noinline)) int stream_copy_begin_record(
        struct ap_fd_call *call,struct sk_buff *skb,u64 transport) {
    struct ap_stream_copy_state *copy=&call->original.stream_copy;
    const u64 pointer=call->selection.word&~3ULL;
    struct ap_fd_file *file=lookup(&fd_files,&pointer);
    struct stream_copy_skb_view view={0};u32 sequence=0;
    if(!file || file->identity!=call->selected_file ||
       !stream_copy_frontier_locked_frame(call,transport) ||
       !stream_copy_frontier_semantics(call,transport) ||
       !stream_copy_read_skb(skb,&view) || copy->source>view.size)goto failed;
    if(transport==AP_STREAM_COPY_TCP) {
        struct tcp_skb_cb *cb=(struct tcp_skb_cb *)CORE(&skb->cb);
        if(fd_read_kernel(&sequence,sizeof(sequence),CORE(&cb->seq)))goto failed;
    }
    struct ap_stream_frontier_request request={.file=file->identity,
        .command=call->command,.attempt=copy->summary.attempts,
        .disposition=ap_original_copy_disposition(call->operation,call->original.selection.address_length),
        .iterator_offset=copy->summary.initial_count-copy->before_count,
        .requested=copy->requested,.available=view.size-copy->source};
    struct ap_stream_copy_record *record=stream_copy_reserve(&stream_copy_records,sizeof(*record),0);
    if(!record) {stream_copy_problem(call,AP_FD_CAPACITY);return 0;}
    struct ap_stream_frontier_begin start={0};
    if(!ap_stream_frontier_begin_attempt(file,&request,&start)) {
        stream_copy_discard(record,0);goto failed;
    }
    const struct ap_stream_copy_begin begin={.file=file->identity,
        .before=start.before,.start=start.start,.order=start.order,.offset=request.iterator_offset,
        .requested=request.requested,.available=request.available,
        .source_offset=copy->source,.skb_length=view.size,.nonlinear=view.nonlinear,
        .position=transport==AP_STREAM_COPY_TCP?(u32)(sequence+(u32)copy->source):copy->source,
        .transport=transport,.disposition=request.disposition};
    if(!ap_stream_copy_begin_valid(&begin,copy->summary.initial_count,request.iterator_offset,request.disposition)) {
        stream_copy_discard(record,0);goto failed;
    }
    /* Same row, under this native receive lock; no whole-row overwrite. Only
     * these active geometry words change, so concurrent poison remains sticky. */
    file->stream_layout=begin;
    stream_copy_record_init(record,call);record->offset=begin.offset;
    record->kind=AP_STREAM_COPY_BEGIN;record->length=sizeof(begin);
    __builtin_memcpy(record->bytes,&begin,sizeof(begin));
    copy->summary.records++;stream_copy_submit(record,0);return 1;
failed:
    stream_copy_problem(call,AP_FD_OUTCOME);return 0;
}
/* The exact helper caller proves which receive lock owns the unit. The
 * relative instruction coordinate is bound to the package's kernel image. */
static __attribute__((noinline)) int stream_copy_unit_enter(
        struct ap_fd_call *call,struct pt_regs *ctx) {
    struct ap_stream_copy_state *copy=&call->original.stream_copy;
    if(!copy->iterator)return 0;
    const u64 transport=copy->copy_active>>AP_STREAM_COPY_TRANSPORT_SHIFT;
    u64 return_ip=0,ip=fd_function_ip(ctx),stack=CORE(ctx->sp);
    struct sk_buff *skb=(struct sk_buff *)CORE(ctx->di);
    const s32 offset=(s32)CORE(ctx->si),length=(s32)CORE(ctx->cx);
    struct iov_iter *iterator=(struct iov_iter *)CORE(ctx->dx);
    if(!skb || !iterator || !stack || (stack&7) ||
       fd_read_kernel(&return_ip,sizeof(return_ip),(const void *)stack) ||
       !ap_stream_copy_unit_caller(transport,ip,return_ip) ||
       (copy->copy_active&AP_STREAM_COPY_PHASE_MASK) || copy->skb || copy->copied ||
       copy->iterator!=(u64)iterator || offset<0 || length<=0 ||
       copy->summary.attempts==~0ULL || copy->summary.protocol_complete ||
       copy->callback.source || copy->callback.count)goto failed;
    u64 data=0,before=0;
    if(fd_read_kernel(&data,sizeof(data),CORE(&skb->data)) ||
       fd_read_kernel(&before,sizeof(before),CORE(&iterator->count)))goto failed;
    if(data>~0ULL-(u32)offset || before>copy->summary.initial_count || (u32)length>before)goto failed;
    if(!stream_copy_unit_custody(skb,(u32)offset,(u32)length,transport)) {
        stream_copy_problem(call,AP_FD_COPY_CUSTODY);return 0;
    }
    copy->summary.attempts++;
    copy->source=(u32)offset;copy->requested=(u32)length;copy->before_count=before;
    copy->skb=(u64)skb;copy->copy_active|=1;
    if(AP_NATIVE_COPY_VERSION==5)stream_copy_begin_record(call,skb,transport);
    return 0;
failed:
    stream_copy_problem(call,AP_FD_OUTCOME);return 0;
}
/* _copy_to_iter is notrace and this kernel excludes even classic probes
 * there. Its two devirtualized calls in the traceable datagram loop supply
 * the exact operands and five-byte successors supply RAX before any revert.
 * Both sites are bound to the installed build ID and audited executable;
 * missing, nested, cross-branch or mismatched pairs refuse the whole Call. */
#if defined(AP_GROUPED_PROVIDER) && !defined(AP_FTRACE_PROVIDER)
static __attribute__((noinline)) int fd_stream_copy_enter(struct pt_regs *ctx) {
#elif !defined(AP_GROUPED_PROVIDER)
SEC("kprobe") int fd_stream_copy_enter(struct pt_regs *ctx) {
#endif
#ifndef AP_FTRACE_PROVIDER
    struct ap_fd_call *call=stream_copy_call();if(!call)return 0;
    struct ap_stream_copy_state *copy=&call->original.stream_copy;
    if(!copy->iterator || !(copy->copy_active&AP_STREAM_COPY_PHASE_MASK))return 0;
    const u64 source=CORE(ctx->di),length=CORE(ctx->si),pointer=CORE(ctx->dx);
    const u64 cookie=fd_attach_cookie(ctx),ip=CORE(ctx->ip);
#ifdef AP_GROUPED_PROVIDER
    if(!ap_stream_copy_classic_site(call->security_socket,copy->protocol_ip,cookie,ip)) {
        stream_copy_problem(call,AP_FD_IDENTITY);return 0;
    }
#endif
    if((cookie!=AP_STREAM_COPY_ENTRY_COOKIE && cookie!=AP_STREAM_COPY_FRAG_ENTRY_COOKIE) ||
       !ip || (copy->copy_active&AP_STREAM_COPY_PHASE_MASK)!=1 || pointer!=copy->iterator ||
       copy->callback.source || copy->callback.count || copy->callback.complete) {
        stream_copy_problem(call,AP_FD_OUTCOME);return 0;
    }
    u64 count=0;
    struct iov_iter *iterator=(struct iov_iter *)pointer;
    if(fd_read_kernel(&count,sizeof(count),CORE(&iterator->count))) {
        stream_copy_problem(call,AP_FD_OUTCOME);return 0;
    }
    if(!copy->skb || !source || copy->copied>copy->requested ||
       !length || length>copy->requested-copy->copied || count!=copy->before_count-copy->copied) {
        stream_copy_problem(call,AP_FD_OUTCOME);return 0;
    }
    struct sk_buff *skb=(struct sk_buff *)copy->skb;
    u32 nonlinear=0;u64 data=0;
    if(fd_read_kernel(&nonlinear,sizeof(nonlinear),CORE(&skb->data_len)) ||
       fd_read_kernel(&data,sizeof(data),CORE(&skb->data))) {
        stream_copy_problem(call,AP_FD_OUTCOME);return 0;
    }
    if(!nonlinear && (source!=data+copy->source || length!=copy->requested || copy->copied)) {
        stream_copy_problem(call,AP_FD_OUTCOME);return 0;
    }
    copy->callback.source=source;copy->callback.count=length;copy->callback.complete=ip;
    copy->copy_active=(copy->copy_active&~AP_STREAM_COPY_PHASE_MASK)|
        (cookie==AP_STREAM_COPY_ENTRY_COOKIE?2:3);
    return 0;
}
#endif

/* All record kinds carry the same authenticated Call identity and next
 * sequence. Keep the fixed initialized payload tail common to every issuer. */
static __attribute__((noinline)) void stream_copy_record_init(
        struct ap_stream_copy_record *record,const struct ap_fd_call *call) {
    const struct ap_original_selection *selected=&call->original.selection;
    record->provider=selected->provider;record->command=selected->command;
    record->call=selected->call;record->task=selected->task;record->task_start=selected->task_start;
    record->sequence=call->original.stream_copy.summary.records+1;
    record->attempt=call->original.stream_copy.summary.attempts;
    __builtin_memset(record->bytes,0,sizeof(record->bytes));
}

struct stream_copy_emit_context { struct ap_fd_call *call; u64 source,count,offset; };
static long stream_copy_emit(u32 index,void *raw) {
    struct stream_copy_emit_context *context=raw;
    struct ap_fd_call *call=context->call;
    struct ap_stream_copy_state *copy=&call->original.stream_copy;
    u64 start=(u64)index*AP_STREAM_COPY_BYTES;
    if(start>=context->count || call->original.problem)return 1;
    u32 length=context->count-start;
    if(length>AP_STREAM_COPY_BYTES)length=AP_STREAM_COPY_BYTES;
    struct ap_stream_copy_record *record=stream_copy_reserve(
        &stream_copy_records,sizeof(*record),0);
    if(!record) {stream_copy_problem(call,AP_FD_CAPACITY);return 1;}
    stream_copy_record_init(record,call);
    record->offset=context->offset+start;record->length=length;record->kind=AP_STREAM_COPY_DATA;
    /* Ring records have a fixed initialized tail; no kernel pointer or stale
     * ring contents cross the boundary. The authenticated
     * skb unit remains owned under the receive lock. This exact copy callback
     * belongs to its private iterator; linear storage has a unique slab-head
     * certificate, TCP fragments have the qualified producer-range contract.
     * No guest reread, page-ref inference or unqualified copy is an issuer. */
    if(fd_read_kernel(record->bytes,length,(const void *)(context->source+start))) {
        stream_copy_discard(record,0);stream_copy_problem(call,AP_FD_OUTCOME);return 1;
    }
    copy->summary.records++;
    stream_copy_submit(record,0);
    return 0;
}
static __attribute__((noinline)) int stream_copy_emit_range(
        struct ap_fd_call *call,u64 source,u64 count,u64 offset) {
    if(!call || !source || !count || source>~0ULL-(count-1) || offset>~0ULL-count) {
        if(call)stream_copy_problem(call,AP_FD_OUTCOME);return 0;
    }
    struct stream_copy_emit_context context={.call=call,.source=source,.count=count,.offset=offset};
    u32 records=(count+AP_STREAM_COPY_BYTES-1)/AP_STREAM_COPY_BYTES;
    if(stream_copy_loop(records,stream_copy_emit,&context,0)<0)
        stream_copy_problem(call,AP_FD_OUTCOME);
    return !call->original.problem;
}

#if defined(AP_GROUPED_PROVIDER) && !defined(AP_FTRACE_PROVIDER)
static __attribute__((noinline)) int fd_stream_copy_exit(struct pt_regs *ctx) {
#elif !defined(AP_GROUPED_PROVIDER)
SEC("kprobe") int fd_stream_copy_exit(struct pt_regs *ctx) {
#endif
#ifndef AP_FTRACE_PROVIDER
    struct ap_fd_call *call=stream_copy_call();if(!call)return 0;
    struct ap_stream_copy_state *copy=&call->original.stream_copy;
    if(!copy->iterator || !(copy->copy_active&AP_STREAM_COPY_PHASE_MASK))return 0;
    u64 returned=CORE(ctx->ax),after=0;
    struct iov_iter *iterator=(struct iov_iter *)copy->iterator;
    if(fd_read_kernel(&after,sizeof(after),CORE(&iterator->count))) {
        stream_copy_problem(call,AP_FD_OUTCOME);return 0;
    }
    const u64 source=copy->callback.source,length=copy->callback.count,entry=copy->callback.complete;
    const u64 cookie=fd_attach_cookie(ctx),ip=CORE(ctx->ip);
    if(!ap_stream_copy_pair_matches(copy->copy_active&AP_STREAM_COPY_PHASE_MASK,cookie,entry,ip) ||
       !copy->skb || !source ||
       !length || copy->copied>copy->requested || length>copy->requested-copy->copied ||
       returned>length || returned>copy->before_count-copy->copied ||
       after!=copy->before_count-copy->copied-returned || copy->summary.copied>~0ULL-returned) {
        stream_copy_problem(call,AP_FD_OUTCOME);return 0;
    }
    if(returned)stream_copy_emit_range(call,source,returned,
        copy->summary.initial_count-copy->before_count+copy->copied);
    copy->summary.copied+=returned;copy->copied+=returned;
    copy->callback.source=0;copy->callback.count=0;copy->callback.complete=0;
    copy->copy_active=(copy->copy_active&~AP_STREAM_COPY_PHASE_MASK)|1;
    return 0;
}
#endif

#ifdef AP_GROUPED_PROVIDER
/* The grouped successor has no notrace _copy_to_iter callbacks. At the
 * traceable skb_copy_datagram_iter return, the original receive lock still
 * owns this skb and the helper has not yet consumed or unlinked it. Emit the
 * successful source range directly; never reread the guest destination. */
#include "stream-copy-grouped-source.inc"
#endif

static __attribute__((noinline)) int stream_copy_unit_exit(
        struct ap_fd_call *call,struct pt_regs *ctx) {
    struct ap_stream_copy_state *copy=&call->original.stream_copy;
    if(!copy->iterator)return 0;
    struct sk_buff *skb=(struct sk_buff *)copy->skb;
    const u64 transport=copy->copy_active>>AP_STREAM_COPY_TRANSPORT_SHIFT;
    u64 after=0;
    struct iov_iter *iterator=(struct iov_iter *)copy->iterator;
    if(fd_read_kernel(&after,sizeof(after),CORE(&iterator->count)))goto failed;
    const s64 returned=(s32)CORE(ctx->ax);
    if((copy->copy_active&AP_STREAM_COPY_PHASE_MASK)!=1 || !skb ||
       copy->callback.source || copy->callback.count || (returned!=0 && returned!=-14) ||
       (returned!=0 && after!=copy->before_count) ||
       !stream_copy_unit_custody(skb,copy->source,copy->requested,transport))goto failed;
#ifdef AP_GROUPED_PROVIDER
    if(!returned && !copy->copied) {
        if(after!=copy->before_count-copy->requested ||
           !stream_copy_emit_grouped_source(call,skb))goto failed;
        copy->copied=copy->requested;copy->summary.copied+=copy->requested;
    }
#endif
    if(returned==0 &&
       (copy->copied!=copy->requested || after!=copy->before_count-copy->copied))goto failed;
    const u64 file_pointer=call->selection.word&~3ULL;
    struct ap_fd_file *file=lookup(&fd_files,&file_pointer);
    if(!file || file->identity!=call->selected_file || !file->identity)goto failed;
    u32 tcp_sequence=0;
    struct tcp_skb_cb *tcp_cb=(struct tcp_skb_cb *)CORE(&skb->cb);
    if(transport==AP_STREAM_COPY_TCP &&
       fd_read_kernel(&tcp_sequence,sizeof(tcp_sequence),CORE(&tcp_cb->seq)))goto failed;
    struct ap_stream_copy_record *record=stream_copy_reserve(
        &stream_copy_records,sizeof(*record),0);
    if(!record) {stream_copy_problem(call,AP_FD_CAPACITY);return 0;}
    /* Failed copy may expose bytes without consuming the queue. Retain the
     * current successful frontier under the same authenticated receive lock;
     * only an actual successful helper advances that per-file counter. */
    const u64 disposition=ap_original_copy_disposition(call->operation,call->original.selection.address_length);
    if(!disposition) {stream_copy_discard(record,0);goto failed;}
    if(AP_NATIVE_COPY_VERSION==5) {
        const struct ap_stream_copy_begin begin=file->stream_layout;
        struct stream_copy_skb_view view={0};
        if(!stream_copy_frontier_locked_frame(call,transport) ||
           !stream_copy_frontier_semantics(call,transport) || !stream_copy_read_skb(skb,&view) ||
           begin.source_offset!=copy->source || begin.skb_length!=view.size || begin.nonlinear!=view.nonlinear ||
           begin.position!=(transport==AP_STREAM_COPY_TCP?(u32)(tcp_sequence+(u32)copy->source):copy->source) ||
           begin.transport!=transport || begin.disposition!=disposition) {
            stream_copy_discard(record,0);goto failed;
        }
        const struct ap_stream_frontier_request request={.file=file->identity,.command=call->command,
            .attempt=copy->summary.attempts,.disposition=disposition,
            .iterator_offset=copy->summary.initial_count-copy->before_count,
            .requested=copy->requested,.available=begin.available};
        struct ap_stream_frontier_end frontier={0};
        if(!ap_stream_frontier_end_attempt(file,&request,copy->copied,returned,&frontier)) {
            stream_copy_discard(record,0);goto failed;
        }
        const struct ap_stream_copy_end end={.unit={.file=file->identity,.order=frontier.order_after,
            .offset=begin.offset,.requested=copy->requested,.copied=copy->copied,.returned=returned,
            .position=begin.position,.transport=transport,.disposition=disposition},
            .before=frontier.before,.after=frontier.after};
        if(!ap_stream_copy_end_valid(&begin,&end,copy->copied)) {
            stream_copy_discard(record,0);goto failed;
        }
        stream_copy_record_init(record,call);record->offset=begin.offset;
        record->length=sizeof(end);record->kind=AP_STREAM_COPY_END;
        __builtin_memcpy(record->bytes,&end,sizeof(end));
        copy->summary.records++;stream_copy_submit(record,0);
        copy->source=0;copy->requested=0;copy->before_count=0;copy->skb=0;copy->copied=0;
        copy->copy_active&=~AP_STREAM_COPY_PHASE_MASK;return 0;
    }
    u64 order=file->stream_units;
    if(!returned && disposition==AP_STREAM_COPY_CONSUME) {
        order=__sync_fetch_and_add(&file->stream_units,1)+1;
        if(!order) {stream_copy_discard(record,0);goto failed;}
    }
    stream_copy_record_init(record,call);
    record->offset=copy->summary.initial_count-copy->before_count;
    record->length=sizeof(struct ap_stream_copy_unit);record->kind=AP_STREAM_COPY_UNIT;
    struct ap_stream_copy_unit unit={.file=file->identity,.order=order,.offset=record->offset,
        .requested=copy->requested,.copied=copy->copied,.returned=returned,
        .position=copy->source,.transport=transport,.disposition=disposition};
    if(transport==AP_STREAM_COPY_TCP)
        unit.position=(u32)(tcp_sequence+(u32)unit.position);
    __builtin_memcpy(record->bytes,&unit,sizeof(unit));
    copy->summary.records++;
    stream_copy_submit(record,0);
    copy->source=0;copy->requested=0;copy->before_count=0;copy->skb=0;copy->copied=0;
    copy->copy_active&=~AP_STREAM_COPY_PHASE_MASK;
    return 0;
failed:
    stream_copy_problem(call,AP_FD_OUTCOME);return 0;
}

/* Called only after the existing original-read sys_exit predicate authenticates
 * the raw return. Protocol return is a copy boundary, never the syscall receipt.
 * Keep all original public Read result bytes canonical after publishing this
 * separate same-Call commit; no borrowed pointer escapes in OriginalResult.
 * Even a Read that never entered a socket protocol emits its zero-summary
 * commit, so a blocked ring record cannot be mistaken for an absent protocol. */
static __attribute__((noinline)) void stream_copy_commit(struct ap_fd_call *call,s64 returned) {
    struct ap_stream_copy_state *copy=&call->original.stream_copy;
    if((ap_original_recv(call->operation) && copy->summary.version!=AP_NATIVE_COPY_VERSION) ||
       (copy->summary.version && copy->summary.version!=AP_NATIVE_COPY_VERSION) || copy->iterator ||
       copy->protocol_ip || copy->copy_active || copy->source || copy->requested ||
       copy->before_count || copy->skb || copy->copied || (copy->summary.version && (copy->summary.protocol_complete!=1 ||
       (s64)copy->summary.protocol_returned!=returned))) {
        stream_copy_problem(call,AP_FD_OUTCOME);
    } else {
        struct ap_stream_copy_record *record=stream_copy_reserve(
            &stream_copy_records,sizeof(*record),0);
        if(!record)stream_copy_problem(call,AP_FD_CAPACITY);
        else {
            stream_copy_record_init(record,call);
            record->offset=(u64)returned;record->length=sizeof(copy->summary);
            record->kind=AP_STREAM_COPY_COMMIT;
            __builtin_memcpy(record->bytes,&copy->summary,sizeof(copy->summary));
            stream_copy_submit(record,0);
        }
    }
    if(call->original.problem)fd_problem(call->original.problem);
    __builtin_memset(call->original.address,0,sizeof(call->original.address));
}
