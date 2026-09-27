/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
/* Included by driver.c after the existing command/FD owner. */
static int fd_reserve_enrollment(struct ap_session *s,const struct ap_task_command *c) {
    if(c->operation!=AP_TABLE_ENROLLMENT)return 0;
    int map=fd_map(s,"fd_enrollments");if(map<0)return -1;
    u32 slot=ap_command_slot(c->command);
    struct ap_fd_enrollment old,empty={0},reserved={.command=c->command,
        .registration=c->generation_before,.owner_mm=c->generation_after,.expected_table=c->expected_object};
    if(bpf_map_lookup_elem(map,&slot,&old))return -1;
    if(memcmp(&old,&empty,sizeof(empty))) { errno=EPROTO;return -1; }
    return bpf_map_update_elem(map,&slot,&reserved,BPF_EXIST);
}
static int fd_ack_enrollment(struct ap_session *s,struct ap_pending_command *p) {
    if(p->submitted.operation!=AP_TABLE_ENROLLMENT)return 0;
    if(!p->enrollment_collected) { errno=EPROTO;return -1; }
    int map=fd_map(s,"fd_enrollments");if(map<0)return -1;
    u32 slot=ap_command_slot(p->submitted.command);
    struct ap_fd_enrollment observed,empty={0};
    if(bpf_map_lookup_elem(map,&slot,&observed))return -1;
    if(memcmp(&observed,&p->enrollment_receipt,sizeof(observed))) { errno=EPROTO;return -1; }
    if(bpf_map_update_elem(map,&slot,&empty,BPF_EXIST))return -1;
    if(bpf_map_lookup_elem(map,&slot,&observed))return -1;
    if(memcmp(&observed,&empty,sizeof(empty))) { errno=EPROTO;return -1; }
    return 0;
}
int ap_prepare_table_enrollment(struct ap_session *s,int pidfd,u64 registration,
                                u64 owner_mm,u64 expected_table,u64 *command) {
    if(!s || !command || !registration)return invalid();
    if(enter_commands(s))return -1;
    struct ap_task_command c={.operation=AP_TABLE_ENROLLMENT,.expected_object=expected_table,
        .generation_before=registration,.generation_after=owner_mm,
        .expected_level=AP_PTRACE_GETREGSET,.expected_option=AP_NT_PRSTATUS};
    int rc=submit(s,pidfd,&c);if(!rc)*command=c.command;
    leave_commands(s);return rc;
}
int ap_collect_table_enrollment(struct ap_session *s,int pidfd,u64 command,
                                struct ap_command_result *result,struct ap_fd_enrollment *receipt) {
    if(!result || !receipt)return invalid();
    if(enter_commands(s))return -1;
    int rc=-1;struct ap_command_result observed={0};
    int completed=read_completion(s,pidfd,command,AP_TABLE_ENROLLMENT,&observed);
    int completion_errno=errno;*result=observed;
    int map=fd_map(s,"fd_enrollments");if(map<0)goto done;
    u32 slot=ap_command_slot(command);struct ap_fd_enrollment first;
    if(bpf_map_lookup_elem(map,&slot,&first))goto done;
    *receipt=first;if(completed)goto done;
    atomic_thread_fence(memory_order_acquire);
    if(bpf_map_lookup_elem(map,&slot,receipt))goto done;
    struct ap_pending_command *p=&s->pending[slot];
    if(memcmp(&first,receipt,sizeof(first)) ||
       !ap_fd_enrollment_matches(&p->submitted,&observed,receipt)) { errno=EPROTO;goto done; }
    struct ap_fd_status status;
    if(ap_read_fd_status(s,&status))goto done;
    if(status.problem) { errno=EPROTO;goto done; }
    p->enrollment_receipt=*receipt;p->enrollment_collected=true;
    if(collect(s,pidfd,&observed))goto done;
    rc=0;
done:
    if(completed) { errno=completion_errno;rc=completed; }
    leave_commands(s);return rc;
}
