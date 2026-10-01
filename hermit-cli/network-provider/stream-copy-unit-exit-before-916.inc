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
