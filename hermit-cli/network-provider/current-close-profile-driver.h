/* SPDX-License-Identifier: BSD-3-Clause */
/* Dedicated map; existing command reservation, disarm and ACK remain owners. */
static int close_profile_reserve(struct ap_session *s,const struct ap_task_command *c,
        const struct ap_close_profile_intent *intent) {
    if(c->operation!=AP_CURRENT_CLOSE_PROFILE)return intent?invalid():0;
#ifdef AP_CURRENT_CLOSE_PROFILE_ENABLED
    if(!intent)return invalid();
    struct ap_close_profile reserved={.intent=*intent};reserved.intent.command=c->command;
    if(!ap_close_intent_matches(c,&reserved.intent))return invalid();
    int map=fd_map(s,"current_close_profiles");if(map<0)return -1;
    u32 slot=ap_command_slot(c->command);struct ap_close_profile old,empty={0};
    if(bpf_map_lookup_elem(map,&slot,&old))return -1;
    if(memcmp(&old,&empty,sizeof(old))) {errno=EPROTO;return -1;}
    return bpf_map_update_elem(map,&slot,&reserved,BPF_EXIST);
#else
    (void)s;(void)intent;errno=ENOTSUP;return -1;
#endif
}
static int close_profile_ack(struct ap_session *s,struct ap_pending_command *p) {
    if(p->submitted.operation!=AP_CURRENT_CLOSE_PROFILE)return 0;
#ifdef AP_CURRENT_CLOSE_PROFILE_ENABLED
    if(!p->close_profile_collected) {errno=EPROTO;return -1;}
    int map=fd_map(s,"current_close_profiles");if(map<0)return -1;
    u32 slot=ap_command_slot(p->submitted.command);struct ap_close_profile observed,empty={0};
    if(bpf_map_lookup_elem(map,&slot,&observed))return -1;
    if(memcmp(&observed,&p->close_profile_receipt,sizeof(observed))) {errno=EPROTO;return -1;}
    if(bpf_map_update_elem(map,&slot,&empty,BPF_EXIST))return -1;
    if(bpf_map_lookup_elem(map,&slot,&observed))return -1;
    if(memcmp(&observed,&empty,sizeof(observed))) {errno=EPROTO;return -1;}
    return 0;
#else
    (void)s;errno=ENOTSUP;return -1;
#endif
}
int ap_prepare_current_close_profile(struct ap_session *s,int pidfd,
        const struct ap_close_profile_intent *intent,u64 *command) {
#ifdef AP_CURRENT_CLOSE_PROFILE_ENABLED
    if(!s || !intent || !command || intent->command)return invalid();
    if(enter_commands(s))return -1;
    int rc=-1;
    if(!s->group_anchor) {unavailable();goto done;}
    struct ap_task_command c={.operation=AP_CURRENT_CLOSE_PROFILE,.expected_object=intent->expected_table,
        .generation_before=intent->registration,.generation_after=intent->owner_mm,
        .expected_level=AP_PTRACE_GETREGSET,.expected_option=AP_NT_PRSTATUS};
    rc=submit_with_intents(s,pidfd,&c,NULL,intent);if(!rc)*command=c.command;
 done:leave_commands(s);return rc;
#else
    (void)s;(void)pidfd;(void)intent;(void)command;errno=ENOTSUP;return -1;
#endif
}
int ap_collect_current_close_profile(struct ap_session *s,int pidfd,u64 command,
        struct ap_command_result *result,struct ap_close_profile *receipt) {
#ifdef AP_CURRENT_CLOSE_PROFILE_ENABLED
    if(!result || !receipt)return invalid();
    if(enter_commands(s))return -1;
    int rc=-1;struct ap_command_result observed={0};
    int completed=read_completion(s,pidfd,command,AP_CURRENT_CLOSE_PROFILE,&observed);
    int completion_errno=errno;*result=observed;
    int map=fd_map(s,"current_close_profiles");if(map<0)goto done;
    u32 slot=ap_command_slot(command);struct ap_close_profile first;
    if(bpf_map_lookup_elem(map,&slot,&first))goto done;
    *receipt=first;if(completed)goto done;
    atomic_thread_fence(memory_order_acquire);
    if(bpf_map_lookup_elem(map,&slot,receipt))goto done;
    struct ap_pending_command *p=&s->pending[slot];
    if(memcmp(&first,receipt,sizeof(first)) || !ap_close_profile_matches(&p->submitted,&observed,receipt)) {
        errno=EPROTO;goto done;
    }
    struct ap_fd_status status;
    if(ap_read_fd_status(s,&status))goto done;
    if(status.problem) {errno=EPROTO;goto done;}
    p->close_profile_receipt=*receipt;p->close_profile_collected=true;
    if(collect(s,pidfd,&observed))goto done;
    rc=0;
 done:if(completed) {errno=completion_errno;rc=completed;}
    leave_commands(s);return rc;
#else
    (void)s;(void)pidfd;(void)command;(void)result;(void)receipt;errno=ENOTSUP;return -1;
#endif
}
/* Pure retained-byte check against the already authenticated image anchor.
 * A refusal does not consume/ACK the settled receipt. The caller still owns ACK. */
int ap_validate_current_close_profile(struct ap_session *s,const struct ap_command_result *result,
        const struct ap_close_profile *receipt) {
#ifdef AP_CURRENT_CLOSE_PROFILE_ENABLED
    if(!result || !receipt || !result->command)return invalid();
    if(enter_commands(s))return -1;
    int rc=-1;struct ap_pending_command *p=&s->pending[ap_command_slot(result->command)];
    if(p->state!=AP_SLOT_COLLECTED || !p->close_profile_collected ||
       memcmp(result,&p->receipt,sizeof(*result)) ||
       memcmp(receipt,&p->close_profile_receipt,sizeof(*receipt))) {errno=ESTALE;goto done;}
    if(!ap_close_profile_finite(&p->submitted,result,receipt,s->group_anchor)) {errno=EPROTO;goto done;}
    rc=0;
 done:leave_commands(s);return rc;
#else
    (void)s;(void)result;(void)receipt;errno=ENOTSUP;return -1;
#endif
}
