/* SPDX-License-Identifier: MIT */
#ifndef HERMIT_STREAM_TX_DRIVER_H
#define HERMIT_STREAM_TX_DRIVER_H
/* Included after the existing pending owner. Fixed-size bytes stay with that
 * command until the same collect/capture/ACK, not in an independent channel. */
static int stream_tx_record(struct ap_pending_command *p,const struct ap_stream_copy_record *r) {
    struct ap_stream_tx_owned *tx=&p->stream_tx;
    struct ap_stream_tx_capture *out=&tx->capture;
    if(p->state!=AP_SLOT_ACTIVE || !ap_stream_tx_command(&p->submitted) ||
       r->provider!=p->submitted.provider || r->command!=p->submitted.command ||
       r->call!=p->submitted.expected_object || !r->task || !r->task_start ||
       tx->committed || r->attempt!=1 || r->sequence!=tx->records+1 ||
       (out->task && (r->task!=out->task || r->task_start!=out->task_start)) ||
       r->length>sizeof(r->bytes))return -1;
    for(size_t i=r->length;i<sizeof(r->bytes);i++)if(r->bytes[i])return -1;
    if(r->kind==AP_STREAM_TX_DATA) {
        if(!r->length || tx->received>p->submitted.original_count ||
           r->length>p->submitted.original_count-tx->received || r->offset!=tx->received)return -1;
        memcpy(out->bytes+tx->received,r->bytes,r->length);tx->received+=r->length;
    } else if(r->kind==AP_STREAM_TX_COMMIT) {
        struct ap_stream_tx_summary summary;
        if(r->length!=sizeof(summary))return -1;
        memcpy(&summary,r->bytes,sizeof(summary));
        if(!ap_stream_tx_summary_valid(&summary,summary.file,p->submitted.original_count,(s64)r->offset) ||
           summary.captured!=tx->received)return -1;
        out->summary=summary;out->returned=(s64)r->offset;tx->committed=true;
    } else return -1;
    out->provider=r->provider;out->command=r->command;out->call=r->call;
    out->task=r->task;out->task_start=r->task_start;tx->records++;return 0;
}
static int stream_copy_drain(struct ap_session *);
int ap_original_sendto_capture(struct ap_session *s,u64 command,struct ap_stream_tx_capture *out) {
    if(!command || !out || enter_commands(s))return -1;
    int rc=-1;struct ap_pending_command *p=&s->pending[ap_command_slot(command)];
    struct ap_stream_tx_owned *tx=&p->stream_tx;
    if(stream_copy_drain(s))goto done;
    if(p->state!=AP_SLOT_COLLECTED || p->submitted.command!=command ||
       p->submitted.operation!=AP_ORIGINAL_SENDTO_CALL || !p->original_collected || !tx->committed ||
       tx->capture.provider!=s->incarnation || tx->capture.command!=command ||
       tx->capture.call!=p->submitted.expected_object || tx->capture.task!=p->receipt.task ||
       tx->capture.task_start!=p->receipt.start_boottime || tx->capture.returned!=p->receipt.returned ||
       !ap_stream_tx_summary_valid(&tx->capture.summary,p->original_receipt.original.selection.file,
           p->submitted.original_count,p->receipt.returned) ||
       memcmp(&tx->capture.summary,&p->original_receipt.original.stream_tx.summary,sizeof(tx->capture.summary))) {
        errno=EPROTO;goto done;
    }
    for(size_t i=tx->capture.summary.captured;i<sizeof(tx->capture.bytes);i++)
        if(tx->capture.bytes[i]) {errno=EPROTO;goto done;}
    *out=tx->capture;tx->read=true;rc=0;
done:
    leave_commands(s);return rc;
}
#endif
