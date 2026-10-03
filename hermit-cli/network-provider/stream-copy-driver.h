/* SPDX-License-Identifier: MIT */
/* Included after the existing session/pending-command definitions. All ring
 * records remain with that command through its ordinary collect/ACK owner. */
#include "stream-tx-driver.h"
static int stream_copy_record(void *raw,void *bytes,size_t length) {
    struct ap_session *s=raw;
    if(length!=sizeof(struct ap_stream_copy_record)) {s->stream_copy_error=EPROTO;return -EPROTO;}
    const struct ap_stream_copy_record *record=bytes;
    if(!record->command || record->provider!=s->incarnation || !record->call ||
       !record->task || !record->task_start) {s->stream_copy_error=EPROTO;return -EPROTO;}
    struct ap_pending_command *p=&s->pending[ap_command_slot(record->command)];
    if(p->submitted.operation==AP_ORIGINAL_SENDTO_CALL) {
        if(stream_tx_record(p,record)) {s->stream_copy_error=EPROTO;return -EPROTO;}
        return 0;
    }
    if(p->submitted.operation==AP_ORIGINAL_SENDTO_BLOCKING_CALL) {
        if(stream_tx_blocking_record(p,record)) {s->stream_copy_error=EPROTO;return -EPROTO;}
        return 0;
    }
    struct ap_stream_copy_owned *copy=&p->stream_copy;
    if(p->state!=AP_SLOT_ACTIVE || p->submitted.command!=record->command ||
       !ap_original_receive(p->submitted.operation) ||
       p->submitted.expected_object!=record->call || copy->committed ||
       (copy->task && (copy->task!=record->task || copy->task_start!=record->task_start)) ||
       record->sequence!=copy->count+1) {s->stream_copy_error=EPROTO;return -EPROTO;}
    copy->task=record->task;copy->task_start=record->task_start;
    if(record->kind==AP_STREAM_COPY_COMMIT) {
        if(record->length!=sizeof(copy->summary)) {s->stream_copy_error=EPROTO;return -EPROTO;}
        struct ap_stream_copy_summary summary;
        memcpy(&summary,record->bytes,sizeof(summary));
        if(!ap_original_read_return_value(p->submitted.original_count,(s64)record->offset)) {
            s->stream_copy_error=EPROTO;return -EPROTO;
        }
        if(summary.version==0) {
            if(ap_original_recv(p->submitted.operation)) {s->stream_copy_error=EPROTO;return -EPROTO;}
            const struct ap_stream_copy_summary empty={0};
            if(memcmp(&summary,&empty,sizeof(empty)) || copy->count || copy->copied ||
               copy->last_attempt || record->attempt) {
                s->stream_copy_error=EPROTO;return -EPROTO;
            }
        } else if(summary.version!=AP_NATIVE_COPY_VERSION || summary.records!=copy->count ||
           summary.initial_count>AP_READ_MAX_COUNT ||
           summary.initial_count>p->submitted.original_count ||
           (ap_original_recv(p->submitted.operation) && summary.initial_count!=p->submitted.original_count) ||
           summary.final_count>summary.initial_count || summary.protocol_complete!=1 ||
           summary.copied!=copy->copied || summary.attempts!=copy->completed_units ||
           copy->unit_bytes || copy->begin_active || summary.attempts!=copy->last_attempt ||
           copy->unit_cursor!=(summary.protocol_returned<(1ULL<<63)?summary.protocol_returned:0) ||
           record->attempt!=summary.attempts || record->offset!=summary.protocol_returned) {
            s->stream_copy_error=EPROTO;return -EPROTO;
        }
        for(size_t i=sizeof(summary);i<sizeof(record->bytes);i++)
            if(record->bytes[i]) {s->stream_copy_error=EPROTO;return -EPROTO;}
        copy->summary=summary;copy->returned=(s64)record->offset;copy->committed=true;return 0;
    }
    if(!record->length || record->length>AP_STREAM_COPY_BYTES ||
       record->attempt!=copy->completed_units+1 || !record->attempt ||
       record->offset>AP_READ_MAX_COUNT || copy->count>=SIZE_MAX/sizeof(*copy->records)) {
        s->stream_copy_error=EPROTO;return -EPROTO;
    }
    for(size_t i=record->length;i<sizeof(record->bytes);i++)
        if(record->bytes[i]) {s->stream_copy_error=EPROTO;return -EPROTO;}
    struct ap_stream_copy_unit unit={0};
    struct ap_stream_copy_begin begin={0};
    struct ap_stream_copy_end end={0};
    if(record->kind==AP_STREAM_COPY_BEGIN && AP_NATIVE_COPY_VERSION==5) {
        if(record->length!=sizeof(begin) || copy->begin_active || copy->unit_bytes) {
            s->stream_copy_error=EPROTO;return -EPROTO;
        }
        memcpy(&begin,record->bytes,sizeof(begin));
        const u64 maximum=p->submitted.original_count<AP_READ_MAX_COUNT
            ? p->submitted.original_count:AP_READ_MAX_COUNT;
        if(record->offset!=copy->unit_cursor ||
           !ap_stream_copy_begin_valid(&begin,maximum,copy->unit_cursor,
             ap_original_copy_disposition(p->submitted.operation,p->submitted.expected_option)) ||
           (copy->unit_file && begin.file!=copy->unit_file) ||
           (copy->frontier_seen && (begin.before<copy->byte_frontier || begin.order<copy->unit_order ||
             ((begin.before==copy->byte_frontier)!=(begin.order==copy->unit_order)) ||
             (begin.disposition==AP_STREAM_COPY_OBSERVE && begin.before!=copy->byte_frontier)))) {
            s->stream_copy_error=EPROTO;return -EPROTO;
        }
    } else if(record->kind==AP_STREAM_COPY_DATA) {
        if((AP_NATIVE_COPY_VERSION==5 && (!copy->begin_active ||
             copy->unit_bytes>copy->current_begin.requested ||
             record->length>copy->current_begin.requested-copy->unit_bytes)) ||
           record->length>AP_READ_MAX_COUNT-record->offset ||
           record->offset!=copy->unit_cursor+copy->unit_bytes ||
           copy->copied>~0ULL-record->length || copy->unit_bytes>~0ULL-record->length) {
            s->stream_copy_error=EPROTO;return -EPROTO;
        }
    } else if((record->kind==AP_STREAM_COPY_UNIT && AP_NATIVE_COPY_VERSION==4) ||
              (record->kind==AP_STREAM_COPY_END && AP_NATIVE_COPY_VERSION==5)) {
        if(AP_NATIVE_COPY_VERSION==5) {
            if(record->length!=sizeof(end) || !copy->begin_active) {s->stream_copy_error=EPROTO;return -EPROTO;}
            memcpy(&end,record->bytes,sizeof(end));unit=end.unit;
            if(!ap_stream_copy_end_valid(&copy->current_begin,&end,copy->unit_bytes)) {
                s->stream_copy_error=EPROTO;return -EPROTO;
            }
        } else {
            if(record->length!=sizeof(unit)) {s->stream_copy_error=EPROTO;return -EPROTO;}
            memcpy(&unit,record->bytes,sizeof(unit));
        }
        if(!unit.file || (copy->unit_file && unit.file!=copy->unit_file) ||
           (unit.transport!=AP_STREAM_COPY_TCP && unit.transport!=AP_STREAM_COPY_UNIX) ||
           unit.position>UINT32_MAX || unit.offset!=record->offset || unit.offset!=copy->unit_cursor ||
           !unit.requested || unit.requested>AP_READ_MAX_COUNT-unit.offset ||
           unit.offset>p->submitted.original_count ||
           unit.requested>p->submitted.original_count-unit.offset || unit.copied>unit.requested ||
           unit.copied!=copy->unit_bytes || (unit.returned!=0 && unit.returned!=-EFAULT) ||
           unit.disposition!=ap_original_copy_disposition(p->submitted.operation,p->submitted.expected_option) ||
           !unit.disposition ||
           (!unit.returned && unit.copied!=unit.requested) ||
           (!unit.returned && unit.disposition==AP_STREAM_COPY_CONSUME &&
            (!unit.order || unit.order<=copy->unit_order)) ||
           ((unit.returned || unit.disposition==AP_STREAM_COPY_OBSERVE) && unit.order<copy->unit_order) ||
           (unit.returned && unit.copied==unit.requested)) {
            s->stream_copy_error=EPROTO;return -EPROTO;
        }
    } else {s->stream_copy_error=EPROTO;return -EPROTO;}
    if(copy->count==copy->capacity) {
        size_t capacity=copy->capacity?copy->capacity*2:16;
        if(capacity<copy->capacity || capacity>SIZE_MAX/sizeof(*copy->records)) {
            s->stream_copy_error=EOVERFLOW;return -EOVERFLOW;
        }
        void *records=realloc(copy->records,capacity*sizeof(*copy->records));
        if(!records) {s->stream_copy_error=ENOMEM;return -ENOMEM;}
        copy->records=records;copy->capacity=capacity;
    }
    copy->records[copy->count++]=*record;
    copy->last_attempt=record->attempt;
    if(record->kind==AP_STREAM_COPY_BEGIN) {
        copy->current_begin=begin;copy->begin_active=true;
    } else if(record->kind==AP_STREAM_COPY_DATA) {
        copy->copied+=record->length;copy->unit_bytes+=record->length;
    } else {
        if(AP_NATIVE_COPY_VERSION==5) {
            copy->byte_frontier=end.after;copy->frontier_seen=true;copy->begin_active=false;
            memset(&copy->current_begin,0,sizeof(copy->current_begin));
        }
        copy->unit_file=unit.file;
        copy->unit_order=unit.order;
        if(!unit.returned)copy->unit_cursor+=unit.copied;
        copy->unit_bytes=0;copy->completed_units++;copy->unit_visible=copy->count;
    }
    return 0;
}

static int stream_copy_drain(struct ap_session *s) {
    if(!s->stream_copy_ring)return unavailable();
    int result=ring_buffer__consume(s->stream_copy_ring);
    if(result<0 && !s->stream_copy_error)s->stream_copy_error=-result;
    if(s->stream_copy_error) {errno=s->stream_copy_error;return -1;}
    return 0;
}

/* Read each retained immutable record by index. Repeated delivery of the same
 * record is idempotent; skipping an undelivered prefix is an error. The record
 * stays owned until the existing command ACK, including a lost RPC reply. */
int ap_original_read_copy_record(struct ap_session *s,u64 command,u64 index,
                                struct ap_stream_copy_record *out) {
    if(!command || !out || enter_commands(s))return -1;
    int rc=-1;
    struct ap_pending_command *p=&s->pending[ap_command_slot(command)];
    struct ap_stream_copy_owned *copy=&p->stream_copy;
    if((p->state!=AP_SLOT_ACTIVE && p->state!=AP_SLOT_COLLECTED) || p->submitted.command!=command ||
       !ap_original_receive(p->submitted.operation) || index>=(copy->terminal_drained?copy->count:copy->unit_visible) ||
       (p->state==AP_SLOT_COLLECTED && (!p->original_collected || !copy->manifest_read || !copy->committed)) ||
       index>copy->delivered) {invalid();goto done;}
    *out=copy->records[index];
    if(index==copy->delivered)copy->delivered++;
    rc=0;
done:
    leave_commands(s);return rc;
}

static int stream_copy_terminal_ready(struct ap_session *,struct ap_pending_command *);

/* This snapshot exposes only complete native helper units. It says nothing
 * about the syscall result until the actual EXIT COMMIT has been consumed.
 * An empty prefix is not a zero-copy outcome or evidence of quiescence. */
int ap_original_read_copy_progress(struct ap_session *s,int pidfd,u64 command,
                                  struct ap_stream_copy_progress *out) {
    if(!command || pidfd<0 || !out || enter_commands(s))return -1;
    int rc=-1;struct ap_pending_command *p=&s->pending[ap_command_slot(command)];
    if((p->state!=AP_SLOT_ACTIVE && p->state!=AP_SLOT_COLLECTED) ||
       p->submitted.command!=command || !ap_original_receive(p->submitted.operation)) {invalid();goto done;}
    bool dead=false;
    if(fd_thread_exited(pidfd,&dead))goto done;
    if(dead) {
        int drained=stream_copy_terminal_ready(s,p);
        if(drained<0)goto done;
    } else {
        if(stream_copy_drain(s) || stream_copy_observer_ready(s))goto done;
        struct ap_fd_status status;
        if(ap_read_fd_status(s,&status))goto done;
        if(status.problem) {unavailable();goto done;}
    }
    /* Incomplete final fragments are retained as diagnostics after actual
     * thread death, never published as a successful unit. The positive finite
     * ring cut is independent of the normal original EXIT marker. */
    const struct ap_stream_copy_owned *copy=&p->stream_copy;
    *out=(struct ap_stream_copy_progress){
        .records=copy->terminal_drained?copy->count:copy->unit_visible,
        .exited=copy->committed,.terminal=copy->terminal_drained,
        .protocol=copy->committed && copy->summary.version==AP_NATIVE_COPY_VERSION};
    rc=0;
done:
    leave_commands(s);return rc;
}

/* Maintenance uses the same single service owner and command lock. Returning
 * the borrowed epoll descriptor does not transfer or duplicate its ownership. */
int ap_original_copy_poll_fd(struct ap_session *s) {
    if(!s || !s->ready || !s->stream_copy_ring)return invalid();
    return ring_buffer__epoll_fd(s->stream_copy_ring);
}
int ap_drain_original_copy(struct ap_session *s) {
    if(enter_commands(s))return -1;
    int rc=stream_copy_drain(s);leave_commands(s);return rc;
}
/* Called only after the exact retained PIDFD_THREAD proves terminal. New
 * records from other tasks cannot extend this Call's finite drain target.
 * A libbpf consume that stops at BUSY does not certify an empty ring. */
static int stream_copy_terminal_ready(struct ap_session *s,struct ap_pending_command *p) {
    if(!ap_original_receive(p->submitted.operation) && !ap_stream_tx_operation(p->submitted.operation))return 1;
    struct ring *ring=ring_buffer__ring(s->stream_copy_ring,0);
    if(!ring)return -1;
    struct ap_stream_copy_owned *copy=&p->stream_copy;
    if(!copy->terminal_cut_set) {
        copy->terminal_cut=ring__producer_pos(ring);copy->terminal_cut_set=true;
    }
    if(stream_copy_drain(s))return -1;
    u64 consumed=ring__consumer_pos(ring);
    // The finite outstanding ring span is less than half the u64 space; the
    // unsigned difference also handles the monotonically wrapping positions.
    if(consumed-copy->terminal_cut<(1ULL<<63))copy->terminal_drained=true;
    return copy->terminal_drained?1:0;
}
/* Readiness only. The existing collector still authenticates the actual
 * native EXIT and original fd selection before any result or ACK is exposed.
 * A busy record belonging to another producer may precede this Call's commit. */
int ap_original_read_copy_ready(struct ap_session *s,int pidfd,u64 command,u32 terminal) {
    if(!command || pidfd<0 || terminal>1 || enter_commands(s))return -1;
    int rc=-1;struct ap_pending_command *p=&s->pending[ap_command_slot(command)];
    if(p->state!=AP_SLOT_ACTIVE || p->submitted.command!=command ||
       !ap_original_receive(p->submitted.operation)) {invalid();goto done;}
    if(terminal) {
        if(fd_require_dead_thread(pidfd))goto done;
        rc=stream_copy_terminal_ready(s,p);goto done;
    }
    if(stream_copy_drain(s) || stream_copy_observer_ready(s))goto done;
    struct ap_fd_status status;
    if(ap_read_fd_status(s,&status))goto done;
    if(status.problem) {unavailable();goto done;}
    rc=p->stream_copy.committed?1:0;
done:
    leave_commands(s);return rc;
}
int ap_original_read_copy_manifest(struct ap_session *s,u64 command,
                                  struct ap_stream_copy_manifest *out) {
    if(!command || !out || enter_commands(s))return -1;
    int rc=-1;struct ap_pending_command *p=&s->pending[ap_command_slot(command)];
    struct ap_stream_copy_owned *copy=&p->stream_copy;
    if(stream_copy_drain(s))goto done;
    if(p->state!=AP_SLOT_COLLECTED || p->submitted.command!=command ||
       !ap_original_receive(p->submitted.operation) || !p->original_collected ||
       !copy->committed) {invalid();goto done;}
    *out=(struct ap_stream_copy_manifest){.provider=s->incarnation,.command=command,
        .call=p->submitted.expected_object,.task=p->receipt.task,
        .task_start=p->receipt.start_boottime,.returned=p->receipt.returned};
    if(copy->task!=out->task || copy->task_start!=out->task_start ||
       copy->returned!=out->returned) {errno=EPROTO;goto done;}
    out->present=copy->summary.version==AP_NATIVE_COPY_VERSION;out->summary=copy->summary;
    copy->manifest_read=true;rc=0;
done:
    leave_commands(s);return rc;
}
