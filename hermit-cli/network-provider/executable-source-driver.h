/* SPDX-License-Identifier: BSD-3-Clause */
/* Included after the existing command and fd_map implementation. */
static int executable_reserve(struct ap_session *s,const struct ap_task_command *c,
                              const struct ap_executable_intent *intent) {
    if(c->operation!=AP_EXECUTABLE_SOURCE)return intent?invalid():0;
    if(!intent)return invalid();
    struct ap_executable_source reserved={.intent=*intent};reserved.intent.command=c->command;
    if(!ap_executable_intent_matches(c,&reserved.intent))return invalid();
    int map=fd_map(s,"executable_sources");if(map<0)return -1;
    u32 slot=ap_command_slot(c->command);struct ap_executable_source old,empty={0};
    if(bpf_map_lookup_elem(map,&slot,&old))return -1;
    if(memcmp(&old,&empty,sizeof(old))) {errno=EPROTO;return -1;}
    return bpf_map_update_elem(map,&slot,&reserved,BPF_EXIST);
}
static int executable_ack(struct ap_session *s,struct ap_pending_command *p) {
    if(p->submitted.operation!=AP_EXECUTABLE_SOURCE)return 0;
    if(!p->executable_collected) {errno=EPROTO;return -1;}
    int map=fd_map(s,"executable_sources");if(map<0)return -1;
    u32 slot=ap_command_slot(p->submitted.command);
    struct ap_executable_source observed,empty={0};
    if(bpf_map_lookup_elem(map,&slot,&observed))return -1;
    if(memcmp(&observed,&p->executable_receipt,sizeof(observed))) {errno=EPROTO;return -1;}
    if(bpf_map_update_elem(map,&slot,&empty,BPF_EXIST))return -1;
    if(bpf_map_lookup_elem(map,&slot,&observed))return -1;
    if(memcmp(&observed,&empty,sizeof(observed))) {errno=EPROTO;return -1;}
    return 0;
}
int ap_prepare_executable_source(struct ap_session *s,int pidfd,u64 registration,u64 owner_mm,
        u64 call,u64 address,u64 length,u64 iovec,u64 registers,u64 *command) {
#if defined(AP_GROUPED_PROVIDER) && defined(AP_FTRACE_PROVIDER)
    if(!s || !command || !registration || !call || !ap_executable_range(address,length) ||
       !iovec || !registers || iovec==registers)return invalid();
    if(enter_commands(s))return -1;
    int rc=-1;
    if(!s->group_anchor) {unavailable();goto done;}
    struct ap_task_command c={.operation=AP_EXECUTABLE_SOURCE,.expected_object=call,
        .generation_before=registration,.generation_after=owner_mm,
        .expected_level=AP_PTRACE_GETREGSET,.expected_option=AP_NT_PRSTATUS,.original_count=length};
    struct ap_executable_intent intent={0,registration,owner_mm,call,address,length,iovec,registers};
    rc=submit_with_executable(s,pidfd,&c,&intent);if(!rc)*command=c.command;
 done:leave_commands(s);return rc;
#else
    (void)s;(void)pidfd;(void)registration;(void)owner_mm;(void)call;(void)address;
    (void)length;(void)iovec;(void)registers;(void)command;errno=ENOTSUP;return -1;
#endif
}
int ap_collect_executable_source(struct ap_session *s,int pidfd,u64 command,
        struct ap_command_result *result,struct ap_executable_source *receipt) {
#if defined(AP_GROUPED_PROVIDER) && defined(AP_FTRACE_PROVIDER)
    if(!result || !receipt)return invalid();
    if(enter_commands(s))return -1;
    int rc=-1;struct ap_command_result observed={0};
    int completed=read_completion(s,pidfd,command,AP_EXECUTABLE_SOURCE,&observed);
    int completion_errno=errno;*result=observed;
    int map=fd_map(s,"executable_sources");if(map<0)goto done;
    u32 slot=ap_command_slot(command);struct ap_executable_source first;
    if(bpf_map_lookup_elem(map,&slot,&first))goto done;
    *receipt=first;if(completed)goto done;
    atomic_thread_fence(memory_order_acquire);
    if(bpf_map_lookup_elem(map,&slot,receipt))goto done;
    struct ap_pending_command *p=&s->pending[slot];
    if(memcmp(&first,receipt,sizeof(first)) ||
       !ap_executable_source_matches(&p->submitted,&observed,receipt,s->group_anchor)) {
        errno=EPROTO;goto done;
    }
    struct ap_fd_status status;
    if(ap_read_fd_status(s,&status))goto done;
    if(status.problem) {errno=EPROTO;goto done;}
    p->executable_receipt=*receipt;p->executable_collected=true;
    if(collect(s,pidfd,&observed))goto done;
    rc=0;
 done:if(completed) {errno=completion_errno;rc=completed;}
    leave_commands(s);return rc;
#else
    (void)s;(void)pidfd;(void)command;(void)result;(void)receipt;errno=ENOTSUP;return -1;
#endif
}
