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
#ifdef AP_STREAM_COPY_FAULT_ENABLED
    if(!stream_copy_fault_arm(call,ctx))goto failed;
#endif
    if(AP_NATIVE_COPY_VERSION==5)stream_copy_begin_record(call,skb,transport);
    return 0;
failed:
    stream_copy_problem(call,AP_FD_OUTCOME);return 0;
}
