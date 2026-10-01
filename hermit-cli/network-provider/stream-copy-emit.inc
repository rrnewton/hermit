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
