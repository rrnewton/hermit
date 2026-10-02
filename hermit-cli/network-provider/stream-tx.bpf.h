/* SPDX-License-Identifier: GPL-2.0 */
/* Same original Call, ring and zero-classic link owner. No guest memory read,
 * lock manipulation or network operation is performed by these callbacks. */
#ifdef AP_FTRACE_PROVIDER
static __attribute__((noinline)) void stream_tx_problem(struct ap_fd_call *call,u64 why) {
    if(call)call->original.problem|=why;
    fd_problem(why);
}
static __attribute__((noinline)) struct ap_fd_call *stream_tx_call(void) {
    struct ap_task_command *c=command();
    if(!c || c->operation!=AP_ORIGINAL_SENDTO_CALL)return 0;
    struct ap_invocation_key key;struct ap_fd_call *call=fd_actor_call(&key);
    struct ap_command_result *r=result(c->command);
    if(!ap_stream_tx_command(c) || !call || !r || call->operation!=c->operation ||
       call->command!=c->command || r->command!=c->command || r->operation!=c->operation ||
       r->phase!=AP_COMMAND_RUNNING || r->identity.provider!=c->provider ||
       r->task!=key.task || r->start_boottime!=key.start || r->original_count!=c->original_count ||
       call->raw_table!=(u64)CORE(current_task()->files) ||
       call->new_file!=(u64)CORE(current_task()->mm) || call->original.problem || call->original.complete ||
       call->original.selection.command!=c->command || call->original.selection.call!=c->expected_object ||
       call->original.selection.owner_mm!=c->generation_after ||
       call->original.selection.task!=key.task || call->original.selection.task_start!=key.start ||
       call->original.selection.provider!=c->provider || !call->original.selection.table ||
       call->original.selection.requested_fd!=c->expected_level ||
       call->original.selection.user_address!=c->generation_before ||
       call->original.selection.address_length!=c->expected_option ||
       call->original.selection.original_count!=c->original_count ||
       call->selection.entered || call->selection.returned || call->entry_stack || call->file_entry_ip) {
        stream_tx_problem(call,AP_FD_IDENTITY);return 0;
    }
    return call;
}
/* The exact installed tcp_sendmsg INLINES tcp_sendmsg_locked. The
 * caller's logical outbound lineage is joined separately to this actual file.
 * Actual kernel flags include O_NONBLOCK's MSG_DONTWAIT, even if Sendto itself
 * supplied only MSG_NOSIGNAL. Repair/fastopen/zerocopy/ancillary paths refuse. */
static __attribute__((noinline)) int stream_tx_shape(struct sock *sk,struct msghdr *msg,int locked) {
    /* Kprobe arguments and map-retained kernel addresses are verifier scalars.
     * CO-RE relocates addresses only; every value load uses the bounded reader.
     * Failed reads refuse, rather than turning zero-initialized fields into a
     * positive shape/ownership observation. */
    u8 state=0;u16 family=0,protocol=0,type=0;s32 owned=0,name_length=0;
    u32 flags=0;u64 name=0,control=0,control_length=0;
    if(!sk || !msg ||
       fd_read_kernel(&state,sizeof(state),(const void *)CORE(&sk->__sk_common.skc_state)) || state!=1 ||
       fd_read_kernel(&family,sizeof(family),CORE(&sk->__sk_common.skc_family)) || family!=2 ||
       fd_read_kernel(&protocol,sizeof(protocol),CORE(&sk->sk_protocol)) || protocol!=6 ||
       fd_read_kernel(&type,sizeof(type),CORE(&sk->sk_type)) || type!=1 ||
       (locked && (fd_read_kernel(&owned,sizeof(owned),CORE(&sk->sk_lock.owned)) || owned!=1)) ||
       fd_read_kernel(&flags,sizeof(flags),CORE(&msg->msg_flags)) || flags!=AP_STREAM_TX_FLAGS ||
       fd_read_kernel(&name,sizeof(name),CORE(&msg->msg_name)) || name ||
       fd_read_kernel(&name_length,sizeof(name_length),CORE(&msg->msg_namelen)) || name_length ||
       fd_read_kernel(&control,sizeof(control),CORE(&msg->msg_control)) || control ||
       fd_read_kernel(&control_length,sizeof(control_length),CORE(&msg->msg_controllen)) || control_length)return 0;
    struct tcp_sock *tcp=(struct tcp_sock *)sk;
    u64 repair=0;
    const u32 n=__builtin_preserve_field_info(tcp->repair,1);
    if(!n || n>8 || fd_read_kernel(&repair,n,(const u8 *)tcp+
          __builtin_preserve_field_info(tcp->repair,0)))return 0;
    repair<<=__builtin_preserve_field_info(tcp->repair,4);
    repair>>=__builtin_preserve_field_info(tcp->repair,5);
    return !repair;
}
static __attribute__((noinline)) struct file *stream_tx_file(struct sock *sk) {
    struct socket *socket=0;struct file *file=0;struct sock *back=0;u32 flags=0;
    if(!sk || fd_read_kernel(&socket,sizeof(socket),CORE(&sk->sk_socket)) || !socket ||
       fd_read_kernel(&back,sizeof(back),CORE(&socket->sk)) || back!=sk ||
       fd_read_kernel(&file,sizeof(file),CORE(&socket->file)) || !file ||
       fd_read_kernel(&flags,sizeof(flags),CORE(&file->f_flags)) || !(flags&04000U))return 0;
    return file;
}
/* Bind the real prologue, lock call and final unlock/result path. Entry nops
 * are ftrace-patched and deliberately excluded; no mid-instruction probe is
 * installed. Driver readback independently binds every function/cookie. */
static __attribute__((noinline)) int stream_tx_image(u64 function) {
    u64 a[5]={0},b[4]={0};
    u32 zero=0;struct ap_config *config=lookup(&ap_config_map,&zero);
    if(!config || config->anchor_phase!=AP_GROUPED_ANCHOR_ACTIVE ||
       function!=ap_grouped_image_address(config->anchor_ip,AP_STREAM_TX_IMAGE) ||
       function>~0ULL-0x871 || fd_read_kernel(a,35,(const void *)(function+5)) ||
       fd_read_kernel(b,28,(const void *)(function+0x855)))return 0;
    return ap_stream_tx_image_words(a,b);
}
static __attribute__((noinline)) int stream_tx_enter(struct pt_regs *ctx) {
    struct ap_fd_call *call=stream_tx_call();if(!call)return 0;
    struct ap_stream_tx_state *tx=&call->original.stream_tx;
    struct sock *sk=(struct sock *)CORE(ctx->di);
    struct msghdr *msg=(struct msghdr *)CORE(ctx->si);
    struct file *file=stream_tx_file(sk);u64 count=0;
    if(tx->active || tx->summary.version || call->selected_file || call->selection.word ||
       call->original.selection.ready || !stream_tx_shape(sk,msg,0) || !file ||
       CORE(ctx->dx)!=call->original.selection.original_count ||
       fd_read_kernel(&count,sizeof(count),CORE(&msg->msg_iter.count)) ||
       count!=call->original.selection.original_count) {
        stream_tx_problem(call,AP_FD_OUTCOME);return 0;
    }
    u64 selected=fd_file(file),function=fd_function_ip(ctx);
    if(!selected || !function || !stream_tx_image(function)) {stream_tx_problem(call,AP_FD_IDENTITY);return 0;}
    tx->summary.version=AP_STREAM_TX_VERSION;tx->summary.file=selected;
    tx->summary.requested=call->original.selection.original_count;
    tx->socket=(u64)sk;tx->message=(u64)msg;tx->function=function;tx->stack=CORE(ctx->sp);tx->active=1;
    call->selected_file=selected;call->selection.word=(u64)file;
    call->original.selection.file=selected;
    return 0;
}
static __attribute__((noinline)) int stream_tx_lock(struct pt_regs *ctx,int returning) {
    struct ap_fd_call *call=stream_tx_call();if(!call)return 0;
    struct ap_stream_tx_state *tx=&call->original.stream_tx;
    if(!tx->active)return 0; /* unrelated lock before/after this protocol invocation */
    if(fd_function_ip(ctx)!=tx->function-(AP_STREAM_TX_IMAGE-AP_STREAM_TX_LOCK_IMAGE)) {
        stream_tx_problem(call,AP_FD_IDENTITY);return 0;
    }
    if(!returning) {
        u64 returned=0,stack=CORE(ctx->sp);
        if(tx->active!=1 || CORE(ctx->di)!=tx->socket || CORE(ctx->si) ||
           fd_read_kernel(&returned,sizeof(returned),(const void *)stack) ||
           !ap_stream_tx_caller(tx->function,tx->stack,stack,returned,AP_STREAM_TX_LOCK_RETURN)) {
            stream_tx_problem(call,AP_FD_IDENTITY);return 0;
        }
        tx->active=2;return 0;
    }
    struct sock *sk=(struct sock *)tx->socket;
    u32 sequence=0;
    if(tx->active!=2 || !stream_tx_shape(sk,(struct msghdr *)tx->message,1) ||
       call->original.selection.ready ||
       fd_read_kernel(&sequence,sizeof(sequence),CORE(&((struct tcp_sock *)sk)->write_seq))) {
        stream_tx_problem(call,AP_FD_OUTCOME);return 0;
    }
    /* First sequence observation is AFTER this exact acquisition completed. */
    tx->summary.sequence_before=sequence;tx->active=3;
    if(__sync_val_compare_and_swap(&call->original.selection.ready,0,1)!=0)
        stream_tx_problem(call,AP_FD_DUPLICATE);
    return 0;
}
static __attribute__((noinline)) int stream_tx_emit(struct ap_fd_call *call,
        u64 source,u64 length,u64 offset) {
    if(!length || length>AP_STREAM_COPY_BYTES || offset>AP_STREAM_COPY_BYTES-length ||
       source>~0ULL-(length-1))return 0;
    struct ap_stream_copy_record *record=stream_copy_reserve(&stream_copy_records,sizeof(*record),0);
    if(!record)return 0;
    __builtin_memset(record,0,sizeof(*record));
    stream_copy_record_init(record,call);
    record->kind=AP_STREAM_TX_DATA;record->attempt=1;record->offset=offset;
    record->sequence=call->original.stream_tx.records+1;record->length=(u32)length;
    if(fd_read_kernel(record->bytes,(u32)length,(const void *)source)) {
        stream_copy_discard(record,0);return 0;
    }
    stream_copy_submit(record,0);call->original.stream_tx.records++;return 1;
}
/* The complete interval must lie in one retained SKB: a linear head and at
 * most one ordinary copied page fragment. Validate that entire storage before
 * emitting either part. No queue or fragment loop expands verifier states. */
static __attribute__((noinline)) int stream_tx_skb(struct ap_fd_call *call,
        struct sk_buff *skb,struct ap_stream_tx_interval *interval) {
    struct stream_copy_skb_view view={0};
    struct tcp_skb_cb *cb=(struct tcp_skb_cb *)CORE(&skb->cb);
    u32 sequence=0,end=0;unsigned short flags=0;
    if(!skb || !stream_copy_read_skb(skb,&view) ||
       fd_read_kernel(&sequence,sizeof(sequence),CORE(&cb->seq)) ||
       fd_read_kernel(&end,sizeof(end),CORE(&cb->end_seq)) ||
       fd_read_kernel(&flags,sizeof(flags),CORE(&cb->tcp_flags)))return 0;
    u64 offset=0,length=0;
    int kind=ap_stream_tx_interval_piece(interval,call->selected_file,sequence,end,
        view.size,flags,&offset,&length);
    if(kind!=1 || interval->covered!=interval->length || length!=interval->length)return 0;
    if(!view.head || view.end>~0ULL-view.head)return 0;
    struct skb_shared_info *shared=(struct skb_shared_info *)(view.head+view.end);
    struct stream_copy_shared_view info={0};
    if(!stream_copy_read_shared(shared,&info) ||
       !ap_stream_tx_single_storage(view.head,view.data,view.tail,view.end,view.size,view.nonlinear,
           view.users,info.count,info.flags,info.frag_list,info.dataref) ||
       info.count>sizeof(shared->frags)/sizeof(shared->frags[0]))return 0;
    u64 sum=0,netmem=0;u32 n=0,at=0;
    if(info.count) {
        if(fd_read_kernel(&netmem,sizeof(netmem),CORE(&shared->frags[0].netmem)) ||
           fd_read_kernel(&n,sizeof(n),CORE(&shared->frags[0].len)) ||
           fd_read_kernel(&at,sizeof(at),CORE(&shared->frags[0].offset)) ||
           !ap_stream_tx_fragment(netmem,at,n,view.nonlinear,&sum))return 0;
    }
    if(sum!=view.nonlinear)return 0;
    const u64 linear=view.size-view.nonlinear;
    u64 progress=0,take=ap_stream_copy_linear_take(offset,length,linear);
    if(take) {
        if(view.data>~0ULL-offset || !stream_tx_emit(call,view.data+offset,take,0))return 0;
        progress=take;
    }
    if(progress==length)return 1;
    u32 zero=0;struct ap_config *config=lookup(&ap_config_map,&zero);
    if(!info.count || !config || config->anchor_phase!=AP_GROUPED_ANCHOR_ACTIVE ||
       !config->vmemmap_base || !config->page_offset_base)return 0;
    u64 within=0;take=0;
    int found=ap_stream_copy_fragment_window(offset+progress,length-progress,linear,n,&within,&take);
    if(found!=1 || take!=length-progress || (u64)at>0xffffffffULL-within)return 0;
    u64 source=ap_stream_fragment_source(netmem,(u64)at+within,take,
        config->vmemmap_base,config->page_offset_base);
    return source && stream_tx_emit(call,source,take,progress);
}
static __attribute__((noinline)) int stream_tx_capture_queues(struct ap_fd_call *call,struct sock *sk) {
    struct ap_stream_tx_summary *s=&call->original.stream_tx.summary;
    struct ap_stream_tx_interval interval={s->file,(u32)s->sequence_before,(u32)s->captured,0};
    if(!interval.length)return 1;
    struct ap_stream_tx_queue_view q={.head=(u64)CORE(&sk->sk_write_queue)};
    if(fd_read_kernel(&q.root,sizeof(q.root),CORE(&sk->tcp_rtx_queue.rb_node)) ||
       fd_read_kernel(&q.next,sizeof(q.next),CORE(&sk->sk_write_queue.next)) ||
       fd_read_kernel(&q.previous,sizeof(q.previous),CORE(&sk->sk_write_queue.prev)) ||
       fd_read_kernel(&q.queued,sizeof(q.queued),CORE(&sk->sk_write_queue.qlen)))return 0;
    struct sk_buff *skb=0;
    if(q.root) {
        struct rb_node *node=(struct rb_node *)q.root;
        if(fd_read_kernel(&q.left,sizeof(q.left),CORE(&node->rb_left)) ||
           fd_read_kernel(&q.right,sizeof(q.right),CORE(&node->rb_right)) ||
           fd_read_kernel(&q.parent,sizeof(q.parent),CORE(&node->__rb_parent_color)))return 0;
        const u32 offset=__builtin_preserve_field_info(((struct sk_buff *)0)->rbnode,0);
        if(q.root<offset)return 0;
        skb=(struct sk_buff *)(q.root-offset);
    } else {
        skb=(struct sk_buff *)q.next;
        if(!skb || (u64)skb==q.head ||
           fd_read_kernel(&q.element_next,sizeof(q.element_next),CORE(&skb->next)) ||
           fd_read_kernel(&q.element_previous,sizeof(q.element_previous),CORE(&skb->prev)))return 0;
    }
    return ap_stream_tx_single_queue(&q) && stream_tx_skb(call,skb,&interval);
}
static __attribute__((noinline)) int stream_tx_unlock(struct pt_regs *ctx,int returning) {
    struct ap_fd_call *call=stream_tx_call();if(!call)return 0;
    struct ap_stream_tx_state *tx=&call->original.stream_tx;
    if(!tx->active)return 0;
    if(fd_function_ip(ctx)!=tx->function-(AP_STREAM_TX_IMAGE-AP_STREAM_TX_UNLOCK_IMAGE)) {
        stream_tx_problem(call,AP_FD_IDENTITY);return 0;
    }
    if(returning) {
        if(tx->active!=4) {stream_tx_problem(call,AP_FD_OUTCOME);return 0;}
        tx->active=5;return 0;
    }
    struct sock *sk=(struct sock *)tx->socket;struct msghdr *msg=(struct msghdr *)tx->message;
    u64 caller=0,stack=CORE(ctx->sp);
    /* Pinned final caller retains its actual result in EBP across release_sock,
     * then copies EBP to EAX. This provisional value is NOT completion: both
     * later function and original syscall returns must independently agree. */
    s64 returned=(s32)CORE(ctx->bp);
    if(tx->active!=3 || CORE(ctx->di)!=tx->socket ||
       fd_read_kernel(&caller,sizeof(caller),(const void *)stack) ||
       !ap_stream_tx_caller(tx->function,tx->stack,stack,caller,AP_STREAM_TX_UNLOCK_RETURN) ||
       !stream_tx_shape(sk,msg,1) || call->original.selection.ready!=1 ||
       !call->selected_file || tx->summary.file!=call->selected_file ||
       returned< -4095 || returned>(s64)tx->summary.requested) {
        stream_tx_problem(call,AP_FD_OUTCOME);return 0;
    }
    struct file *file=stream_tx_file(sk);u32 sequence=0;
    if(!file || (u64)file!=call->selection.word || fd_file(file)!=call->selected_file ||
       fd_read_kernel(&sequence,sizeof(sequence),CORE(&((struct tcp_sock *)sk)->write_seq))) {
        stream_tx_problem(call,AP_FD_IDENTITY);return 0;
    }
    tx->summary.sequence_after=sequence;
    tx->summary.captured=returned>0?(u64)returned:0;
    tx->summary.protocol_returned=(u64)returned;
    if((u32)((u32)tx->summary.sequence_after-(u32)tx->summary.sequence_before)!=tx->summary.captured ||
       !stream_tx_capture_queues(call,sk)) {
        stream_tx_problem(call,AP_FD_OUTCOME);return 0;
    }
    /* ACK processing may have run inside tcp_sendmsg_locked's backlog flush.
     * We do not infer retention from lock.owned alone: absent accepted bytes
     * fail the complete sequence walk above, including after a real effect. */
    tx->active=4;return 0;
}
static __attribute__((noinline)) int stream_tx_exit(struct pt_regs *ctx) {
    struct ap_fd_call *call=stream_tx_call();if(!call)return 0;
    struct ap_stream_tx_state *tx=&call->original.stream_tx;
    const s64 returned=(s32)CORE(ctx->ax);
    /* The original lock is already released. Do not dereference its queues or
     * replace staged bytes with a late snapshot. Only join the real return. */
    if(tx->active!=5 || tx->function!=fd_function_ip(ctx) ||
       tx->summary.protocol_complete || tx->summary.protocol_returned!=(u64)returned) {
        stream_tx_problem(call,AP_FD_OUTCOME);return 0;
    }
    tx->summary.protocol_complete=1;
    if(!ap_stream_tx_summary_valid(&tx->summary,call->selected_file,
          call->original.selection.original_count,returned)) {
        stream_tx_problem(call,AP_FD_OUTCOME);return 0;
    }
    tx->socket=tx->message=tx->function=tx->stack=tx->active=0;return 0;
}
static __attribute__((noinline)) int stream_tx_syscall_exit(u64 *ctx,struct ap_task_command *c) {
    struct pt_regs *regs=(struct pt_regs *)ctx[0];u64 nr=0,cs=0;
    if(!regs || fd_read_kernel(&nr,sizeof(nr),CORE(&regs->orig_ax))) {
        fd_problem(AP_FD_MISSING);return 0;
    }
    if(nr!=AP_SENDTO_SYSCALL)return 0;
    struct ap_fd_call *call=stream_tx_call();if(!call)return 0;
    struct ap_stream_tx_state *tx=&call->original.stream_tx;s64 returned=(s64)ctx[1];
    struct ap_command_result *result_row=result(c->command);
    u64 fd=0,address=0,count=0,flags=0,destination=0,address_length=0;
    if(!result_row || fd_read_kernel(&cs,sizeof(cs),CORE(&regs->cs)) || cs!=0x33 ||
       fd_read_kernel(&fd,sizeof(fd),CORE(&regs->di)) ||
       fd_read_kernel(&address,sizeof(address),CORE(&regs->si)) ||
       fd_read_kernel(&count,sizeof(count),CORE(&regs->dx)) ||
       fd_read_kernel(&flags,sizeof(flags),CORE(&regs->r10)) ||
       fd_read_kernel(&destination,sizeof(destination),CORE(&regs->r8)) ||
       fd_read_kernel(&address_length,sizeof(address_length),CORE(&regs->r9)) ||
       !ap_stream_tx_operands(c,nr,fd,address,count,flags,destination,address_length) ||
       !ap_original_selection_matches(c,&call->original.selection) ||
       tx->socket || tx->message || tx->function || tx->stack || tx->active ||
       !ap_stream_tx_summary_valid(&tx->summary,call->selected_file,c->original_count,returned)) {
        stream_tx_problem(call,AP_FD_OUTCOME);return 0;
    }
    struct ap_stream_copy_record *record=stream_copy_reserve(&stream_copy_records,sizeof(*record),0);
    if(!record) {stream_tx_problem(call,AP_FD_CAPACITY);return 0;}
    __builtin_memset(record,0,sizeof(*record));stream_copy_record_init(record,call);
    record->kind=AP_STREAM_TX_COMMIT;record->attempt=1;record->offset=(u64)returned;
    record->sequence=tx->records+1;record->length=sizeof(tx->summary);
    __builtin_memcpy(record->bytes,&tx->summary,sizeof(tx->summary));
    stream_copy_submit(record,0);
    tx->records=0;
    call->original.returned=(s32)returned;call->original.complete=1;result_row->returned=(s32)returned;
    publish_result(result_row);return 0;
}
#else
static __attribute__((noinline)) int stream_tx_syscall_exit(u64 *ctx,struct ap_task_command *c) {
    (void)ctx;(void)c;fd_problem(AP_FD_OUTCOME);return 0;
}
#endif
