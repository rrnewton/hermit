/* SPDX-License-Identifier: MIT */
/* Included after the maintained driver's shared command owner. */
static int fd_map(struct ap_session *s,const char *name) {
    int fd=bpf_object__find_map_fd_by_name(s->object,name);
    if(fd<0)return unavailable();
    return fd;
}
/* Runs inside submit(), after reserving the shared ticket and before arming the
 * actual task. Failure quarantines that original reservation in the caller. */
static int fd_reserve_accept(struct ap_session *s,const struct ap_task_command *c) {
    if(c->operation!=AP_ACCEPT_EFFECT)return 0;
    int map=fd_map(s,"fd_accepts");if(map<0)return -1;
    u32 slot=ap_command_slot(c->command);
    struct ap_fd_accept old,empty={0},reserved={.command=c->command,
        .accept_lease=c->generation_before,.owner_mm=c->generation_after};
    if(bpf_map_lookup_elem(map,&slot,&old))return -1;
    if(memcmp(&old,&empty,sizeof(empty))) { errno=EPROTO;return -1; }
    return bpf_map_update_elem(map,&slot,&reserved,BPF_EXIST);
}
/* Invoked only after ap_ack_command checked its exact collected command and
 * raw receipt. Both receipts are owned by that same bounded pending slot. */
static int fd_ack_accept(struct ap_session *s,struct ap_pending_command *p) {
    if(p->submitted.operation!=AP_ACCEPT_EFFECT)return 0;
    if(!p->fd_collected) { errno=EPROTO;return -1; }
    int map=fd_map(s,"fd_accepts");if(map<0)return -1;
    u32 slot=ap_command_slot(p->submitted.command);
    struct ap_fd_accept observed,empty={0};
    if(bpf_map_lookup_elem(map,&slot,&observed))return -1;
    if(memcmp(&observed,&p->fd_receipt,sizeof(observed))) { errno=EPROTO;return -1; }
    if(bpf_map_update_elem(map,&slot,&empty,BPF_EXIST))return -1;
    if(bpf_map_lookup_elem(map,&slot,&observed))return -1;
    if(memcmp(&observed,&empty,sizeof(empty))) { errno=EPROTO;return -1; }
    return 0;
}
int ap_prepare_accept(struct ap_session *s,int pidfd,struct ap_identity listener,
                      u64 lease,u64 mm,int fd,int flags,u64 *command) {
    if(!s || !command || !lease || listener.provider!=s->incarnation ||
       !listener.object || !listener.namespace)return invalid();
    if(enter_commands(s))return -1;
    struct ap_task_command c={.operation=AP_ACCEPT_EFFECT,.expected_object=listener.object,
        .generation_before=lease,.generation_after=mm,.expected_level=fd,.expected_option=flags};
    int rc=submit(s,pidfd,&c);
    if(!rc)*command=c.command;
    leave_commands(s);return rc;
}
int ap_read_fd_status(struct ap_session *s,struct ap_fd_status *out) {
    if(!s || !s->ready || !out)return invalid();
    int map=fd_map(s,"fd_status");if(map<0)return -1;
    u32 zero=0;return bpf_map_lookup_elem(map,&zero,out);
}
int ap_collect_accept(struct ap_session *s,int pidfd,u64 command,
                      struct ap_command_result *result,struct ap_fd_accept *receipt) {
    if(!result || !receipt)return invalid();
    if(enter_commands(s))return -1;
    int rc=-1;struct ap_command_result observed={0};
    int completed=read_completion(s,pidfd,command,AP_ACCEPT_EFFECT,&observed);
    int completion_errno=errno; /* Diagnostic lookups must not replace the primary failure. */
    *result=observed;
    u32 slot=ap_command_slot(command);int map=fd_map(s,"fd_accepts");
    if(map<0)goto done;
    struct ap_fd_accept first;
    if(bpf_map_lookup_elem(map,&slot,&first))goto done;
    *receipt=first;
    if(completed)goto done;
    atomic_thread_fence(memory_order_acquire);
    struct ap_fd_accept second;
    if(bpf_map_lookup_elem(map,&slot,&second))goto done;
    *receipt=second;
    struct ap_pending_command *p=&s->pending[slot];
    if(memcmp(&first,receipt,sizeof(first)) ||
       !ap_fd_accept_matches(&p->submitted,&observed,&first)) { errno=EPROTO;goto done; }
    struct ap_fd_status status;
    if(ap_read_fd_status(s,&status))goto done;
    if(status.problem) { errno=EPROTO;goto done; }
    p->fd_receipt=first;p->fd_collected=true;
    if(collect(s,pidfd,&observed))goto done;
    rc=0;
done:
    if(completed) { errno=completion_errno;rc=completed; }
    leave_commands(s);return rc;
}
int ap_read_fd_event(struct ap_session *s,u64 sequence,struct ap_fd_event *out) {
    if(!s || !s->ready || !sequence || !out)return invalid();
    int map=fd_map(s,"fd_journal");if(map<0)return -1;
    struct ap_fd_event first;
    if(bpf_map_lookup_elem(map,&sequence,&first))return -1;
    *out=first;
    if(first.sequence!=sequence || first.complete!=1)return unavailable();
    atomic_thread_fence(memory_order_acquire);
    if(bpf_map_lookup_elem(map,&sequence,out))return -1;
    if(memcmp(&first,out,sizeof(first)))return unavailable();
    return 0;
}
int ap_ack_fd_event(struct ap_session *s,const struct ap_fd_event *receipt) {
    if(!receipt || !receipt->sequence || receipt->complete!=1)return invalid();
    if(enter_commands(s))return -1;
    int rc=-1,map=fd_map(s,"fd_journal");if(map<0)goto done;
    struct ap_fd_event observed;
    if(bpf_map_lookup_elem(map,&receipt->sequence,&observed))goto done;
    if(memcmp(&observed,receipt,sizeof(observed))) { errno=ESTALE;goto done; }
    /* No producer touches a completed event. A failed update is quarantined:
     * callers retain this exact ACK effect and cannot resubmit it as success. */
    if(bpf_map_delete_elem(map,&receipt->sequence))goto done;
    rc=0;
done:
    leave_commands(s);return rc;
}
