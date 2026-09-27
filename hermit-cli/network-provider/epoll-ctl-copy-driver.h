/* SPDX-License-Identifier: MIT */
/* Included after fd-effects-driver.h: the existing bounded command owner,
 * exact PIDFD_THREAD gates, and physical row retirement are shared. */
int ap_prepare_epoll_ctl_copy(struct ap_session *s,int pidfd,u64 call,u64 mm,
                              int op,u64 pointer,u64 *command) {
    if(!s || !command || !call)return invalid();
    if(enter_commands(s))return -1;
    struct ap_task_command c={.operation=AP_EPOLL_CTL_COPY,.expected_object=call,
        .generation_before=pointer,.generation_after=mm,.expected_level=op,
        .expected_option=AP_EPOLL_CTL_SYSCALL};
    int rc=submit(s,pidfd,&c);
    if(!rc)*command=c.command;
    leave_commands(s);return rc;
}
int ap_collect_epoll_ctl_copy(struct ap_session *s,int pidfd,u64 command,
    struct ap_command_result *result,struct ap_epoll_ctl_copy *out) {
    if(!result || !out || !command)return invalid();
    if(enter_commands(s))return -1;
    int rc=-1;struct ap_command_result observed={0};
    int completed=read_completion(s,pidfd,command,AP_EPOLL_CTL_COPY,&observed);
    int completion_errno=errno;*result=observed;
    if(completed)goto done;
    struct ap_pending_command *p=&s->pending[ap_command_slot(command)];
    int map=fd_map(s,"fd_calls");if(map<0)goto done;
    struct ap_invocation_key key={.task=observed.task,.start=observed.start_boottime};
    struct ap_fd_call first,second;
    if(bpf_map_lookup_elem(map,&key,&first))goto done;
    *out=first.epoll_copy;
    if(first.command!=command || first.operation!=AP_EPOLL_CTL_COPY ||
       !ap_epoll_ctl_copy_matches(&p->submitted,&observed,out)) {unavailable();goto done;}
    atomic_thread_fence(memory_order_acquire);
    if(bpf_map_lookup_elem(map,&key,&second))goto done;
    *out=second.epoll_copy;
    if(memcmp(&first,&second,sizeof(first))) {unavailable();goto done;}
    struct ap_fd_status status;
    if(ap_read_fd_status(s,&status))goto done;
    if(status.problem) {unavailable();goto done;}
    /* One retained raw call-row buffer in the existing pending owner. No
     * logical original-file selection flag is set by this copy-only command. */
    p->original_receipt=second;p->epoll_collected=true;
    if(collect(s,pidfd,&observed))goto done;
    rc=0;
done:
    if(completed) {errno=completion_errno;rc=completed;}
    leave_commands(s);return rc;
}
static int fd_ack_epoll_ctl_copy(struct ap_session *s,struct ap_pending_command *p) {
    if(p->submitted.operation!=AP_EPOLL_CTL_COPY)return 0;
    if(!p->epoll_collected || p->original_selected || p->original_collected) {errno=EPROTO;return -1;}
    return fd_ack_call_row(s,p);
}
int ap_cancel_uninvoked_epoll_ctl_copy(struct ap_session *s,int pidfd,u64 command) {
    return cancel_uninvoked_command(s,pidfd,command,AP_EPOLL_CTL_COPY);
}
/* Death retires retained evidence, never completes an interrupted copy. The
 * positive exact-thread poll and original task binding precede all mutation;
 * unknown mutation outcomes retain the existing quarantined command owner. */
int ap_retire_dead_epoll_ctl_copy(struct ap_session *s,int pidfd,u64 command,
                                 struct ap_epoll_ctl_terminal *out) {
    if(!s || pidfd<0 || !command || !out)return invalid();
    if(enter_commands(s))return -1;
    int rc=-1;u32 slot=ap_command_slot(command);
    struct ap_pending_command *p=&s->pending[slot];
    if(p->state!=AP_SLOT_ACTIVE || p->submitted.command!=command ||
       !ap_epoll_ctl_command(&p->submitted)) {invalid();goto done;}
    if(fd_require_dead_thread(pidfd))goto done;
    struct ap_epoll_ctl_terminal retained={.call=p->submitted.expected_object};
    struct ap_command_result check;
    if(bpf_map_lookup_elem(s->commands,&slot,&retained.command) ||
       bpf_map_lookup_elem(s->commands,&slot,&check))goto done;
    *out=retained;
    if(memcmp(&retained.command,&check,sizeof(check)) || check.command!=command ||
       check.operation!=AP_EPOLL_CTL_COPY || check.original_count) {errno=ESTALE;goto done;}
    struct ap_task_command task;bool stored=false;
    if(!bpf_map_lookup_elem(s->tasks,&pidfd,&task)) {
        if(memcmp(&task,&p->submitted,sizeof(task))) {errno=ESTALE;goto done;}
        stored=true;
    } else if(errno!=ENOENT && errno!=ESRCH)goto done;
    int map=fd_map(s,"fd_calls");if(map<0)goto done;
    struct ap_invocation_key key={.task=check.task,.start=check.start_boottime};
    struct ap_fd_call call,again;
    if(check.phase==AP_COMMAND_READY) {
        struct ap_command_result reserved={.command=command,.operation=AP_EPOLL_CTL_COPY,
            .phase=AP_COMMAND_READY};
        if(memcmp(&check,&reserved,sizeof(check))) {errno=EPROTO;goto done;}
    } else if(check.phase==AP_COMMAND_RUNNING || check.phase==AP_COMMAND_DONE) {
        if(!key.task || !key.start || check.identity.provider!=s->incarnation) {errno=EPROTO;goto done;}
        if(!bpf_map_lookup_elem(map,&key,&call)) {
            if(call.command!=command || call.operation!=AP_EPOLL_CTL_COPY ||
               !ap_epoll_ctl_binding(&p->submitted,&call.epoll_copy) ||
               call.epoll_copy.task!=key.task || call.epoll_copy.task_start!=key.start) {errno=ESTALE;goto done;}
            if(bpf_map_lookup_elem(map,&key,&again))goto done;
            if(memcmp(&call,&again,sizeof(call))) {errno=ESTALE;goto done;}
            retained.copy=call.epoll_copy;retained.fd_call_present=1;
        } else if(errno!=ENOENT)goto done;
    } else {errno=EPROTO;goto done;}
    *out=retained;
    rc=fd_retire_dead_rows(s,p,pidfd,stored,map,&key,retained.fd_call_present,&retained.task_absent);
    *out=retained;
done:
    leave_commands(s);return rc;
}
