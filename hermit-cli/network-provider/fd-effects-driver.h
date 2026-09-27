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
int ap_prepare_original_connect(struct ap_session *s,int pidfd,u64 call,u64 mm,
                                int fd,u64 address,int length,u64 *command) {
    if(!s || !command || !call)return invalid();
    if(enter_commands(s))return -1;
    struct ap_task_command c={.operation=AP_ORIGINAL_CONNECT,.expected_object=call,
        .generation_before=address,.generation_after=mm,.expected_level=fd,.expected_option=length};
    int rc=submit(s,pidfd,&c);
    if(!rc)*command=c.command;
    leave_commands(s);return rc;
}
int ap_prepare_original_close(struct ap_session *s,int pidfd,u64 call,u64 mm,
                              int fd,u64 *command) {
    if(!s || !command || !call)return invalid();
    if(enter_commands(s))return -1;
    struct ap_task_command c={.operation=AP_ORIGINAL_CLOSE,.expected_object=call,
        .generation_after=mm,.expected_level=fd};
    int rc=submit(s,pidfd,&c);
    if(!rc)*command=c.command;
    leave_commands(s);return rc;
}
int ap_prepare_original_read(struct ap_session *s,int pidfd,u64 call,u64 mm,
                             int fd,u64 buffer,u64 count,u64 *command) {
    if(!s || !command || !call)return invalid();
    if(enter_commands(s))return -1;
    struct ap_task_command c={.operation=AP_ORIGINAL_READ,.expected_object=call,
        .generation_before=buffer,.generation_after=mm,.expected_level=fd,.original_count=count};
    int rc=stream_copy_observer_ready(s);
    if(!rc)rc=submit(s,pidfd,&c);
    if(!rc)*command=c.command;
    leave_commands(s);return rc;
}
/* AUTONOMOUS-BOT-IMPLEMENTED: typed native helper receive through the same Call.
 * TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/3174). */
static int prepare_original_receive(struct ap_session *s,int pidfd,u64 call,u64 mm,
    int fd,u64 pointer,u64 count,int flags,u64 operation,u64 *command) {
    if(!s || !command || !call || fd<0 || count>AP_READ_MAX_COUNT ||
       !ap_original_copy_disposition(operation,flags))return invalid();
    if(enter_commands(s))return -1;
    struct ap_task_command c={.operation=operation,.expected_object=call,
        .generation_before=pointer,.generation_after=mm,.expected_level=fd,
        .expected_option=flags,.original_count=count};
    int rc=stream_copy_observer_ready(s);
    if(!rc)rc=submit(s,pidfd,&c);
    if(!rc)*command=c.command;
    leave_commands(s);return rc;
}
int ap_prepare_original_recvfrom(struct ap_session *s,int pidfd,u64 call,u64 mm,
    int fd,u64 buffer,u64 count,int flags,u64 *command) {
    return prepare_original_receive(s,pidfd,call,mm,fd,buffer,count,flags,AP_ORIGINAL_RECVFROM_CALL,command);
}
int ap_prepare_original_recvmsg(struct ap_session *s,int pidfd,u64 call,u64 mm,
    int fd,u64 header,u64 count,int flags,u64 *command) {
    return prepare_original_receive(s,pidfd,call,mm,fd,header,count,flags,AP_ORIGINAL_RECVMSG_CALL,command);
}
int ap_prepare_original_file(struct ap_session *s,int pidfd,u64 call,u64 mm,
                             int fd,int syscall_nr,int file_command,u64 *command) {
    if(!s || !command || !call || !ap_original_file_shape((u64)syscall_nr,file_command))return invalid();
    if(enter_commands(s))return -1;
    struct ap_task_command c={.operation=AP_ORIGINAL_FILE,.expected_object=call,
        .generation_before=(u64)syscall_nr,.generation_after=mm,
        .expected_level=fd,.expected_option=file_command};
    int rc=submit(s,pidfd,&c);
    if(!rc)*command=c.command;
    leave_commands(s);return rc;
}
int ap_prepare_auxiliary_file(struct ap_session *s,int pidfd,u64 call,u64 mm,
                              int fd,u64 *command) {
    if(!s || !command || !call || fd<0)return invalid();
    if(enter_commands(s))return -1;
    struct ap_task_command c={.operation=AP_AUXILIARY_FILE,.expected_object=call,
        .generation_before=AP_FILE_SYSCALL_FCNTL,.generation_after=mm,
        .expected_level=fd,.expected_option=AP_FILE_GET_FLAGS};
    int rc=submit(s,pidfd,&c);
    if(!rc)*command=c.command;
    leave_commands(s);return rc;
}
int ap_prepare_original_socket(struct ap_session *s,int pidfd,u64 call,u64 mm,
                               int domain,int type,int protocol,u64 *command) {
    if(!s || !command || !call)return invalid();
    if(enter_commands(s))return -1;
    struct ap_task_command c={.operation=AP_ORIGINAL_SOCKET_CALL,.expected_object=call,
        .generation_before=(u64)(u32)type,.generation_after=mm,
        .expected_level=domain,.expected_option=protocol};
    int rc=submit(s,pidfd,&c);
    if(!rc)*command=c.command;
    leave_commands(s);return rc;
}
int ap_prepare_original_openat(struct ap_session *s,int pidfd,u64 call,u64 mm,
                               int dfd,u64 pathname,int flags,u64 mode,u64 *command) {
    if(!s || !command || !call)return invalid();
    if(enter_commands(s))return -1;
    struct ap_task_command c={.operation=AP_ORIGINAL_OPENAT_CALL,.expected_object=call,
        .generation_before=pathname,.generation_after=mm,.expected_level=dfd,
        .expected_option=flags,.original_count=mode};
    int rc=submit(s,pidfd,&c);
    if(!rc)*command=c.command;
    leave_commands(s);return rc;
}
/* No prevalidation of the legacy size or create1 flags: their actual Linux
 * error ordering belongs to the selected original syscall. */
int ap_prepare_original_epoll(struct ap_session *s,int pidfd,u64 call,u64 mm,
                              int syscall_nr,int argument,u64 *command) {
    if(!s || !command || !call || !ap_original_epoll_syscall((u64)syscall_nr))return invalid();
    if(enter_commands(s))return -1;
    struct ap_task_command c={.operation=AP_ORIGINAL_EPOLL_CALL,.expected_object=call,
        .generation_before=(u64)syscall_nr,.generation_after=mm,.expected_level=argument};
    int rc=submit(s,pidfd,&c);
    if(!rc)*command=c.command;
    leave_commands(s);return rc;
}
int ap_prepare_original_epoll_ctl(struct ap_session *s,int pidfd,u64 call,u64 mm,
                                  int epfd,int op,int fd,u64 pointer,u64 *command) {
    if(!s || !command || !call)return invalid();
    if(enter_commands(s))return -1;
    struct ap_task_command c={.operation=AP_ORIGINAL_EPOLL_CTL,.expected_object=call,
        .generation_before=pointer,.generation_after=mm,.expected_level=epfd,
        .expected_option=op,.original_count=(u64)(u32)fd};
    int rc=fd_accept_observer_ready_runtime(s);
    if(!rc)rc=submit(s,pidfd,&c);
    if(!rc)*command=c.command;
    leave_commands(s);return rc;
}
/* Read-only predicates used only by the same original operation. A missing
 * task row requires positive death of its retained PIDFD_THREAD; numeric task
 * absence and a read errno cannot authorize either collection or disarm. */
static int fd_thread_exited(int pidfd,bool *exited) {
    struct pollfd task_dead={.fd=pidfd,.events=POLLIN};
    int polled=poll(&task_dead,1,0);
    if(polled<0)return -1;
    if(task_dead.revents&(POLLERR|POLLNVAL) || polled>1 ||
       (polled==1 && !(task_dead.revents&POLLIN))) {errno=EAGAIN;return -1;}
    *exited=polled==1 && (task_dead.revents&POLLIN);return 0;
}
static int fd_require_dead_thread(int pidfd) {
    bool exited=false;if(fd_thread_exited(pidfd,&exited))return -1;
    if(!exited) {errno=EAGAIN;return -1;}return 0;
}
static int fd_dead_task_binding(struct ap_session *s,struct ap_pending_command *p,int pidfd,bool *stored) {
    struct ap_task_command task;
    if(fd_require_dead_thread(pidfd))return -1;
    if(!bpf_map_lookup_elem(s->tasks,&pidfd,&task)) {
        if(memcmp(&task,&p->submitted,sizeof(task))) {errno=ESTALE;return -1;}
        *stored=true;return 0;
    }
    if(errno!=ENOENT && errno!=ESRCH)return -1;
    *stored=false;return 0;
}
static int fd_disarm_dead_task_observed(struct ap_session *s,struct ap_pending_command *p,int pidfd,bool stored) {
    struct ap_task_command task;
    p->state=AP_SLOT_DISARMING;
    p->disarm=(struct ap_task_disarm_outcome){0};
    if(stored) {
        int rc=bpf_map_delete_elem(s->tasks,&pidfd),error=errno;
        p->disarm.phase=AP_DISARM_DEAD_DELETE_RETURNED;
        p->disarm.mutation_rc=rc;p->disarm.mutation_errno=rc?error:0;
        if(rc) {errno=error;return -1;}
    }
    int rc=bpf_map_lookup_elem(s->tasks,&pidfd,&task),error=errno;
    p->disarm.phase=AP_DISARM_DEAD_READBACK_RETURNED;
    p->disarm.readback_rc=rc;p->disarm.readback_errno=rc?error:0;
    if(!rc) {errno=EPROTO;return -1;}
    if(error!=ENOENT && error!=ESRCH) {errno=error;return -1;}
    return 0;
}
static int fd_disarm_dead_task(struct ap_session *s,struct ap_pending_command *p,int pidfd,bool stored) {
    return fd_disarm_dead_task_observed(s,p,pidfd,stored)?quarantine(p):0;
}
/* Exact target's libbpf returns -errno. Its task-storage mutation ENOENT
 * precedes effect. This predicate applies only to the retained syscall phase;
 * it is not authority to infer a native result or to retry a mutation. */
static bool fd_exact_task_missing(int rc,int error) { return rc==-ENOENT && error==ENOENT; }
static int fd_confirm_detached_task(struct ap_session *s,struct ap_pending_command *p,int pidfd) {
    struct pollfd task_dead={.fd=pidfd,.events=POLLIN};
    int polled=poll(&task_dead,1,0);
    if(polled<0)return -1;
    /* On the bound kernel HUP requires pid_task(original pid)==NULL. POLLIN
     * alone also describes an unreaped zombie and does not prove detachment. */
    if(polled!=1 || (task_dead.revents&(POLLIN|POLLHUP))!=(POLLIN|POLLHUP) ||
       (task_dead.revents&(POLLERR|POLLNVAL))) {errno=EAGAIN;return -1;}
    struct ap_task_command task;
    int rc=bpf_map_lookup_elem(s->tasks,&pidfd,&task),error=errno;
    if(!rc) {errno=EPROTO;return -1;}
    if(!fd_exact_task_missing(rc,error)) {errno=error;return -1;}
    p->disarm.detached_verified=true;return 0;
}
static int fd_disarm_birth_task(struct ap_session *s,struct ap_pending_command *p,int pidfd,bool exited,bool stored) {
    int rc=exited?fd_disarm_dead_task_observed(s,p,pidfd,stored):disarm_task_observed(s,pidfd,p);
    if(!rc)return 0;
    const struct ap_task_disarm_outcome *d=&p->disarm;
    bool no_effect=(d->phase==AP_DISARM_IDLE_UPDATE_RETURNED ||
                    d->phase==AP_DISARM_DEAD_DELETE_RETURNED) &&
                   fd_exact_task_missing(d->mutation_rc,d->mutation_errno);
    bool updated_then_detached=d->phase==AP_DISARM_IDLE_READBACK_RETURNED &&
                              d->mutation_rc==0 && fd_exact_task_missing(d->readback_rc,d->readback_errno);
    if((!no_effect && !updated_then_detached) || fd_confirm_detached_task(s,p,pidfd))return quarantine(p);
    return 0;
}
/* This private adapter operation is authorized only by the consumed Tool
 * marker proving guest.inject was never invoked. READY/map absence alone is
 * NOT that proof. Disarm precedes reuse; no native return is fabricated. */
static int cancel_uninvoked_command(struct ap_session *s,int pidfd,u64 command,u64 operation) {
    if(!s || pidfd<0 || !command)return invalid();
    if(enter_commands(s))return -1;
    int rc=-1;u32 slot=ap_command_slot(command);
    struct ap_pending_command *p=&s->pending[slot];
    if(!operation) {
        if(!ap_original_operation(p->submitted.operation)) {invalid();goto done;}
        operation=p->submitted.operation;
    }
    struct ap_task_command task;
    struct ap_command_result expected={.command=command,.operation=operation,
        .phase=AP_COMMAND_READY,.original_count=p->submitted.original_count},actual,empty={0};
    if(p->state!=AP_SLOT_ACTIVE || p->submitted.command!=command ||
       p->submitted.operation!=operation ||
       (!ap_original_operation(operation) && operation!=AP_NATIVE_BIRTH && operation!=AP_EPOLL_CTL_COPY) ||
       p->epoll_collected || p->original_selected || p->original_collected || p->birth_observed || p->birth_collected ||
       p->birth_child_admitted || p->birth_child_terminal) { invalid();goto done; }
    bool exited=false,stored=false;
    if(operation==AP_NATIVE_BIRTH && fd_thread_exited(pidfd,&exited))goto done;
    if(exited) {
        if(fd_dead_task_binding(s,p,pidfd,&stored))goto done;
    } else {
        if(bpf_map_lookup_elem(s->tasks,&pidfd,&task)) {
            // This was a read only. Re-observe the same Cancel on positive
            // death, before any update/delete has been submitted.
            if(operation!=AP_NATIVE_BIRTH || fd_dead_task_binding(s,p,pidfd,&stored))goto done;
            exited=true;
        } else if(memcmp(&task,&p->submitted,sizeof(task))) {errno=ESTALE;goto done;}
    }
    if(bpf_map_lookup_elem(s->commands,&slot,&actual))goto done;
    if(memcmp(&actual,&expected,sizeof(actual))) {errno=ESTALE;goto done;}
    if(!exited && operation==AP_NATIVE_BIRTH) {
        if(fd_thread_exited(pidfd,&exited))goto done;
        if(exited && fd_dead_task_binding(s,p,pidfd,&stored))goto done;
    }
    if(exited ? fd_disarm_dead_task(s,p,pidfd,stored) : disarm_task(s,pidfd,p))goto done;
    if(bpf_map_lookup_elem(s->commands,&slot,&actual))goto uncertain;
    if(memcmp(&actual,&expected,sizeof(actual))) { errno=ESTALE;goto uncertain; }
    if(bpf_map_update_elem(s->commands,&slot,&empty,BPF_EXIST) ||
       bpf_map_lookup_elem(s->commands,&slot,&actual))goto uncertain;
    if(memcmp(&actual,&empty,sizeof(actual))) { errno=EPROTO;goto uncertain; }
    memset(p,0,sizeof(*p));rc=0;goto done;
uncertain:
    p->state=AP_SLOT_QUARANTINED;
done:
    leave_commands(s);return rc;
}
int ap_cancel_uninvoked_original(struct ap_session *s,int pidfd,u64 command) {
    return cancel_uninvoked_command(s,pidfd,command,0);
}
int ap_cancel_uninvoked_birth(struct ap_session *s,int pidfd,u64 command) {
    return cancel_uninvoked_command(s,pidfd,command,AP_NATIVE_BIRTH);
}
/* Shared exact-handle terminal gate and physical row retirement. Both callers
 * retain their operation-specific validation; no task disappearance is a
 * syscall result or a no-child result. Caller holds the command lock. */
static int fd_retire_dead_rows(struct ap_session *s,struct ap_pending_command *p,
    int pidfd,bool stored_task,int map,const struct ap_invocation_key *key,
    bool row_present,u64 *task_absent) {
    struct ap_task_command task;struct ap_fd_call again;
    struct ap_command_result check,empty={0};
    u32 slot=ap_command_slot(p->submitted.command);
    p->state=AP_SLOT_DISARMING;
    if(stored_task && bpf_map_delete_elem(s->tasks,&pidfd))goto uncertain;
    if(!bpf_map_lookup_elem(s->tasks,&pidfd,&task)) { errno=EPROTO;goto uncertain; }
    if(errno!=ENOENT && errno!=ESRCH)goto uncertain;
    *task_absent=1;
    if(row_present) {
        if(bpf_map_delete_elem(map,key))goto uncertain;
        if(!bpf_map_lookup_elem(map,key,&again)) { errno=EPROTO;goto uncertain; }
        if(errno!=ENOENT)goto uncertain;
    }
    if(bpf_map_update_elem(s->commands,&slot,&empty,BPF_EXIST) ||
       bpf_map_lookup_elem(s->commands,&slot,&check))goto uncertain;
    if(memcmp(&check,&empty,sizeof(check))) { errno=EPROTO;goto uncertain; }
    free(p->stream_copy.records);
    memset(p,0,sizeof(*p));return 0;
uncertain:
    p->state=AP_SLOT_QUARANTINED;return -1;
}
/* The service binds this borrowed PIDFD_THREAD to the original preparation.
 * The caller additionally has the backend's exact final-wait observation. A
 * positive poll of that held object is required here before any map mutation;
 * READY, a missing hook, a PID number, or owner loss cannot authorize cleanup. */
int ap_retire_dead_original(struct ap_session *s,int pidfd,u64 command,
                            struct ap_original_terminal *out) {
    if(!s || pidfd<0 || !command || !out)return invalid();
    if(enter_commands(s))return -1;
    int rc=-1;u32 slot=ap_command_slot(command);
    struct ap_pending_command *p=&s->pending[slot];
    if(p->state!=AP_SLOT_ACTIVE || p->submitted.command!=command ||
       !ap_original_operation(p->submitted.operation)) { invalid();goto done; }
    if(fd_require_dead_thread(pidfd))goto done;
    struct ap_original_terminal retained={.call=p->submitted.expected_object};
    struct ap_command_result check;
    if(bpf_map_lookup_elem(s->commands,&slot,&retained.command) ||
       bpf_map_lookup_elem(s->commands,&slot,&check))goto done;
    *out=retained; /* Raw bytes remain diagnostics even if retirement fails. */
    if(memcmp(&retained.command,&check,sizeof(check)) || check.command!=command ||
       check.operation!=p->submitted.operation) { errno=ESTALE;goto done; }
    struct ap_task_command task;
    bool stored_task=false;
    if(!bpf_map_lookup_elem(s->tasks,&pidfd,&task)) {
        if(memcmp(&task,&p->submitted,sizeof(task))) { errno=ESTALE;goto done; }
        stored_task=true;
    } else if(errno!=ENOENT && errno!=ESRCH)goto done;
    int map=fd_map(s,"fd_calls");if(map<0)goto done;
    struct ap_invocation_key key={.task=check.task,.start=check.start_boottime};
    struct ap_fd_call call,again;
    if(check.phase==AP_COMMAND_READY) {
        struct ap_command_result reserved={.command=command,
            .operation=p->submitted.operation,.phase=AP_COMMAND_READY,
            .original_count=p->submitted.original_count};
        if(memcmp(&check,&reserved,sizeof(check))) { errno=EPROTO;goto done; }
        /* claim_result sets RUNNING/task/start before fd_connect_enter can
         * insert any row. With the actual task finally dead there is no
         * in-flight producer and this reservation owns no fd_calls row. */
    } else if(check.phase==AP_COMMAND_RUNNING || check.phase==AP_COMMAND_DONE) {
        if(!key.task || !key.start || check.identity.provider!=s->incarnation) {
            errno=EPROTO;goto done;
        }
        if(!bpf_map_lookup_elem(map,&key,&call)) {
            if(call.command!=command || call.operation!=p->submitted.operation ||
               call.original.selection.command!=command ||
               call.original.selection.call!=retained.call ||
               call.original.selection.owner_mm!=p->submitted.generation_after ||
               call.original.selection.provider!=s->incarnation ||
               call.original.selection.task!=key.task ||
               call.original.selection.task_start!=key.start) { errno=ESTALE;goto done; }
            if(bpf_map_lookup_elem(map,&key,&again))goto done;
            if(memcmp(&call,&again,sizeof(call))) { errno=ESTALE;goto done; }
            retained.original=call.original;retained.fd_call_present=1;
            /* Read's address union is private iterator/copy custody until its
             * actual exit. The terminal path has no such completion and must
             * neither export borrowed pointers nor invent a copy commit. */
            if(ap_original_receive(p->submitted.operation))
                memset(retained.original.address,0,sizeof(retained.original.address));
        } else if(errno!=ENOENT)goto done;
    } else { errno=EPROTO;goto done; }
    /* Authenticate the command and retained rows before mutating copy custody.
     * The exact terminal receipt rules out a later producer for this Call.
     * Preserve its command until the shared ring consumer crossed the retained
     * terminal position, including copies that never reached a commit. */
    int drained=stream_copy_terminal_ready(s,p);
    if(drained!=1) {if(!drained)errno=EAGAIN;goto done;}
    if(ap_original_receive(p->submitted.operation) &&
       p->stream_copy.delivered!=p->stream_copy.count) {errno=EPROTO;goto done;}
    *out=retained;
    rc=fd_retire_dead_rows(s,p,pidfd,stored_task,map,&key,
                           retained.fd_call_present,&retained.task_absent);
    *out=retained;
done:
    leave_commands(s);return rc;
}
/* Early observation does not collect/disarm/ACK the original command. Its
 * independent consumer retains the positive selection before releasing any
 * corresponding exclusion. Ctl has no pre-copy table exclusion: its immutable
 * pair is published only after original uaccess and actual native fdgets. */
static int read_original_selection_locked(struct ap_session *s,int pidfd,u64 command,
    struct ap_original_selection *selection,struct ap_original_result *epoll) {
    if(!selection || !command || pidfd<0)return invalid();
    int rc=-1;u32 slot=ap_command_slot(command);
    struct ap_pending_command *p=&s->pending[slot];
    if(p->state!=AP_SLOT_ACTIVE || p->submitted.command!=command ||
       !ap_original_operation(p->submitted.operation) ||
       (!!epoll)!=(p->submitted.operation==AP_ORIGINAL_EPOLL_CTL)) { invalid();goto done; }
    struct ap_task_command task;
    if(bpf_map_lookup_elem(s->tasks,&pidfd,&task))goto done;
    if(memcmp(&task,&p->submitted,sizeof(task))) { invalid();goto done; }
    struct ap_command_result result;
    if(bpf_map_lookup_elem(s->commands,&slot,&result))goto done;
    if(result.command!=command || result.operation!=p->submitted.operation ||
       result.original_count!=p->submitted.original_count ||
       ((p->submitted.operation==AP_ORIGINAL_CLOSE || ap_original_file_operation(p->submitted.operation) ||
         ap_original_receive(p->submitted.operation) || p->submitted.operation==AP_ORIGINAL_EPOLL_CTL || ap_original_allocator(p->submitted.operation)) &&
        result.identity.provider!=p->submitted.provider) ||
       (result.phase!=AP_COMMAND_RUNNING && result.phase!=AP_COMMAND_DONE) ||
       !result.task || !result.start_boottime) { unavailable();goto done; }
    int map=fd_map(s,"fd_calls");if(map<0)goto done;
    struct ap_invocation_key key={.task=result.task,.start=result.start_boottime};
    struct ap_fd_call first,second;
    if(bpf_map_lookup_elem(map,&key,&first))goto done;
    *selection=first.original.selection;
    if(first.operation!=p->submitted.operation || first.command!=command || first.original.problem ||
       !ap_original_selection_matches(&p->submitted,selection) ||
       selection->task!=key.task || selection->task_start!=key.start ||
       (epoll && !ap_original_epoll_ctl_selected(&p->submitted,&first.original))) { unavailable();goto done; }
    atomic_thread_fence(memory_order_acquire);
    if(bpf_map_lookup_elem(map,&key,&second))goto done;
    *selection=second.original.selection;
    if(second.operation!=first.operation || second.command!=command || second.original.problem ||
       memcmp(&first.original.selection,selection,sizeof(*selection)) ||
       (epoll && (!ap_original_epoll_ctl_selected(&p->submitted,&second.original) ||
          memcmp(first.original.address,second.original.address,offsetof(struct ap_original_epoll_ctl,ctl_returned))))) {
        unavailable();goto done;
    }
    struct ap_status status;struct ap_fd_status fd_status;
    if(ap_read_status(s,&status) || ap_read_fd_status(s,&fd_status))goto done;
    if(status.fatal || fd_status.problem) { unavailable();goto done; }
    if(fd_accept_observer_ready_runtime(s))goto done;
    if(p->original_selected && memcmp(&p->original_selection,selection,sizeof(*selection))) {
        errno=ESTALE;goto done;
    }
    if(epoll) {
        if(p->original_selected && memcmp(p->original_receipt.original.address,second.original.address,
              offsetof(struct ap_original_epoll_ctl,ctl_returned))) {errno=ESTALE;goto done;}
        memcpy(p->original_receipt.original.address,second.original.address,
            offsetof(struct ap_original_epoll_ctl,ctl_returned));
        *epoll=second.original;
    }
    p->original_selection=*selection;p->original_selected=true;rc=0;
done:
    return rc;
}
int ap_read_original_selection(struct ap_session *s,int pidfd,u64 command,
    struct ap_original_selection *selection) {
    if(enter_commands(s))return -1;
    int rc=read_original_selection_locked(s,pidfd,command,selection,NULL);
    leave_commands(s);return rc;
}
/* Both selections and the original copied bytes travel together. Generic
 * one-FD selection cannot silently acknowledge half of an epoll_ctl call. */
int ap_read_original_epoll_ctl_selection(struct ap_session *s,int pidfd,u64 command,
    struct ap_original_result *original) {
    if(!original)return invalid();
    if(enter_commands(s))return -1;
    struct ap_original_selection selected;
    int rc=read_original_selection_locked(s,pidfd,command,&selected,original);
    leave_commands(s);return rc;
}
int ap_collect_original_connect(struct ap_session *s,int pidfd,u64 command,
                                struct ap_command_result *result,struct ap_original_result *original) {
    if(!result || !original || !command)return invalid();
    if(enter_commands(s))return -1;
    if(stream_copy_drain(s)) {leave_commands(s);return -1;}
    int rc=-1;struct ap_command_result observed={0};
    u64 operation=s->pending[ap_command_slot(command)].submitted.operation;
    int completed=ap_original_operation(operation)?
        read_completion(s,pidfd,command,operation,&observed):invalid();
    int completion_errno=errno;*result=observed;
    if(completed)goto done;
    /* DONE is published after the final ring submission. This second drain
     * closes the finish-between-drain-and-query race before COLLECTED. */
    if(stream_copy_drain(s))goto done;
    struct ap_pending_command *p=&s->pending[ap_command_slot(command)];
    if(ap_original_receive(p->submitted.operation) && !p->stream_copy.committed) {
        errno=EAGAIN;goto done;
    }
    if(ap_original_receive(p->submitted.operation) && stream_copy_observer_ready(s))goto done;
    if(!p->original_selected) { unavailable();goto done; }
    int map=fd_map(s,"fd_calls");if(map<0)goto done;
    struct ap_invocation_key key={.task=observed.task,.start=observed.start_boottime};
    struct ap_fd_call first,second;
    if(bpf_map_lookup_elem(map,&key,&first))goto done;
    *original=first.original;
    if(first.command!=command || first.operation!=p->submitted.operation ||
       !ap_original_result_matches(&p->submitted,&observed,original)) { unavailable();goto done; }
    atomic_thread_fence(memory_order_acquire);
    if(bpf_map_lookup_elem(map,&key,&second))goto done;
    *original=second.original;
    if(memcmp(&first,&second,sizeof(first)) ||
       memcmp(&p->original_selection,&original->selection,sizeof(original->selection)) ||
       (operation==AP_ORIGINAL_EPOLL_CTL && memcmp(p->original_receipt.original.address,original->address,
           offsetof(struct ap_original_epoll_ctl,ctl_returned)))) {
        unavailable();goto done;
    }
    struct ap_fd_status fd_status;
    if(ap_read_fd_status(s,&fd_status))goto done;
    if(fd_status.problem) { unavailable();goto done; }
    if(fd_accept_observer_ready_runtime(s))goto done;
    p->original_receipt=second;p->original_collected=true;
    if(collect(s,pidfd,&observed))goto done;
    rc=0;
done:
    if(completed) { errno=completion_errno;rc=completed; }
    leave_commands(s);return rc;
}
/* Exact retained fd_calls retirement shared with the copy-only command.
 * Operation-specific admission and completion checks remain in the callers. */
static int fd_ack_call_row(struct ap_session *s,struct ap_pending_command *p) {
    int map=fd_map(s,"fd_calls");if(map<0)return -1;
    struct ap_invocation_key key={.task=p->receipt.task,.start=p->receipt.start_boottime};
    struct ap_fd_call actual;
    if(bpf_map_lookup_elem(map,&key,&actual))return -1;
    if(memcmp(&actual,&p->original_receipt,sizeof(actual))) { errno=ESTALE;return -1; }
    if(bpf_map_delete_elem(map,&key))return -1;
    if(!bpf_map_lookup_elem(map,&key,&actual)) { errno=EPROTO;return -1; }
    return errno==ENOENT?0:-1;
}
static int fd_ack_original(struct ap_session *s,struct ap_pending_command *p) {
    if(!ap_original_operation(p->submitted.operation))return 0;
    if(ap_original_receive(p->submitted.operation) &&
       (!p->stream_copy.manifest_read || p->stream_copy.delivered!=p->stream_copy.count)) {
        errno=EPROTO;return -1;
    }
    if(!p->original_selected || !p->original_collected) { errno=EPROTO;return -1; }
    int rc=fd_ack_call_row(s,p);
    if(!rc) {free(p->stream_copy.records);p->stream_copy=(struct ap_stream_copy_owned){0};}
    return rc;
}
int ap_read_fd_status(struct ap_session *s,struct ap_fd_status *out) {
    if(!s || !s->ready || !out)return invalid();
    int map=fd_map(s,"fd_status");if(map<0)return -1;
    u32 zero=0;if(bpf_map_lookup_elem(map,&zero,out))return -1;
    /* fdget session counters are global to unrelated host traffic. Journal
     * state has no fdget premise; operations that do require selection check
     * exact runtime link shape beside their task-scoped positive receipt. */
    return 0;
}
int ap_collect_accept(struct ap_session *s,int pidfd,u64 command,
                      struct ap_command_result *result,struct ap_fd_accept *receipt) {
    if(!result || !receipt)return invalid();
    if(enter_commands(s))return -1;
    if(stream_copy_drain(s)) {leave_commands(s);return -1;}
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
    if(fd_accept_observer_ready_runtime(s))goto done;
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

/* Birth observations remain in the SAME original command slot and fd_calls
 * row. The service authenticates reads through its retained preparation and
 * creator pidfd identity even after that creator has died. No new task lookup
 * or new command is permitted to reconstruct a historical birth. */
int ap_prepare_native_birth(struct ap_session *s,int pidfd,u64 call,u64 mm,u64 table,int syscall,u64 *command) {
    if(!s || !command || !call || !table || !ap_native_birth_syscall(syscall))return invalid();
    if(enter_commands(s))return -1;
    struct ap_task_command c={.operation=AP_NATIVE_BIRTH,.expected_object=call,
        .generation_before=table,.generation_after=mm,.expected_level=syscall};
    int rc=submit(s,pidfd,&c);if(!rc)*command=c.command;
    leave_commands(s);return rc;
}
static int read_birth(struct ap_session *s,u64 command,struct ap_native_birth *out) {
    if(!command || !out)return invalid();
    u32 slot=ap_command_slot(command);struct ap_pending_command *p=&s->pending[slot];
    if(p->state!=AP_SLOT_ACTIVE || p->submitted.operation!=AP_NATIVE_BIRTH ||
       p->submitted.command!=command)return invalid();
    struct ap_command_result result;
    if(bpf_map_lookup_elem(s->commands,&slot,&result))return -1;
    if(result.command!=command || result.operation!=AP_NATIVE_BIRTH ||
       (result.phase!=AP_COMMAND_RUNNING && result.phase!=AP_COMMAND_DONE) ||
       !result.task || !result.start_boottime)return unavailable();
    struct ap_invocation_key key={.task=result.task,.start=result.start_boottime};
    int map=fd_map(s,"fd_calls");if(map<0)return -1;
    struct ap_fd_call first,second;
    if(bpf_map_lookup_elem(map,&key,&first))return -1;
    *out=first.birth;
    if(first.operation!=AP_NATIVE_BIRTH || first.command!=command ||
       !ap_native_birth_matches(&p->submitted,out) ||
       out->creator_task!=key.task || out->creator_start!=key.start)return unavailable();
    atomic_thread_fence(memory_order_acquire);
    if(bpf_map_lookup_elem(map,&key,&second))return -1;
    *out=second.birth;
    if(second.operation!=first.operation || second.command!=command ||
       memcmp(&first.birth,out,sizeof(*out)))return unavailable();
    struct ap_status status;struct ap_fd_status fd_status;
    if(ap_read_status(s,&status) || ap_read_fd_status(s,&fd_status))return -1;
    if(status.fatal || fd_status.problem)return unavailable();
    if(p->birth_observed && memcmp(&p->birth_receipt,out,sizeof(*out))) { errno=ESTALE;return -1; }
    p->birth_receipt=*out;p->birth_observed=true;return 0;
}
int ap_read_native_birth(struct ap_session *s,u64 command,struct ap_native_birth *out) {
    if(enter_commands(s))return -1;
    int rc=read_birth(s,command,out);leave_commands(s);return rc;
}
/* This live binding is consumed once while the backend retains the exact
 * newborn stop. The service owns its transferred PIDFD_THREAD before entry.
 * Deleting the exact birth marker allows the existing normal registration to
 * create the idle row; failure/quarantine must never start that child. */
int ap_admit_native_birth_child(struct ap_session *s,int child,u64 command,struct ap_native_birth *out) {
    if(child<0)return invalid();
    if(enter_commands(s))return -1;
    int rc=-1;if(read_birth(s,command,out))goto done;
    struct ap_pending_command *p=&s->pending[ap_command_slot(command)];
    struct ap_task_command marker;
    if(p->birth_child_admitted || p->birth_child_terminal) { errno=ESTALE;goto done; }
    if(bpf_map_lookup_elem(s->tasks,&child,&marker))goto done;
    if(memcmp(&marker,&p->submitted,sizeof(marker))) { errno=ESTALE;goto done; }
    /* The native producer permits one child per command. The held pidfd
     * selects this marker; no local/raw PID equality is assumed. */
    if(bpf_map_delete_elem(s->tasks,&child))goto uncertain;
    if(!bpf_map_lookup_elem(s->tasks,&child,&marker)) { errno=EPROTO;goto uncertain; }
    if(errno!=ENOENT)goto uncertain;
    p->birth_child_admitted=true;rc=0;goto done;
uncertain:
    quarantine(p);
done:
    leave_commands(s);return rc;
}
int ap_admit_native_birth_terminal(struct ap_session *s,u64 command,struct ap_native_birth *out) {
    if(enter_commands(s))return -1;
    int rc=-1;if(read_birth(s,command,out))goto done;
    struct ap_pending_command *p=&s->pending[ap_command_slot(command)];
    if(p->birth_child_admitted || p->birth_child_terminal) { errno=ESTALE;goto done; }
    p->birth_child_terminal=true;rc=0;
done:
    leave_commands(s);return rc;
}
int ap_collect_native_birth(struct ap_session *s,int pidfd,u64 command,
                            struct ap_command_result *out,struct ap_native_birth *birth) {
    if(!out || !birth)return invalid();
    if(enter_commands(s))return -1;
    int rc=-1;struct ap_command_result completed={0};
    struct ap_pending_command *p=&s->pending[ap_command_slot(command)];
    if(read_completion(s,pidfd,command,AP_NATIVE_BIRTH,&completed)) {
        *out=completed;
        // No physical effect has been attempted. Creator exit may have
        // removed task storage before/during the live read. The SAME Collect
        // can still read its immutable DONE result with positive held death.
        bool stored=false;
        if(fd_dead_task_binding(s,p,pidfd,&stored) ||
           read_command_completion(s,command,AP_NATIVE_BIRTH,&completed)) { *out=completed;goto done; }
    }
    *out=completed;
    struct ap_invocation_key key={.task=completed.task,.start=completed.start_boottime};
    int map=fd_map(s,"fd_calls");if(map<0)goto done;
    struct ap_fd_call first,second;
    if(bpf_map_lookup_elem(map,&key,&first))goto done;
    *birth=first.birth;
    if(first.command!=command || first.operation!=AP_NATIVE_BIRTH || first.birth.problem ||
       first.birth.provider!=p->submitted.provider || first.birth.call!=p->submitted.expected_object ||
       first.birth.owner_mm!=p->submitted.generation_after ||
       first.birth.creator_task!=completed.task || first.birth.creator_start!=completed.start_boottime ||
       completed.identity.provider!=p->submitted.provider || completed.returned==0 || completed.returned< -4095) {
        unavailable();goto done;
    }
    if(completed.returned>0) {
        if(!ap_native_birth_matches(&p->submitted,birth)) { unavailable();goto done; }
        if(p->birth_observed && memcmp(&p->birth_receipt,birth,sizeof(*birth))) {errno=ESTALE;goto done;}
    } else if(birth->ready || p->birth_observed || p->birth_child_admitted || p->birth_child_terminal) {
        unavailable();goto done;
    }
    atomic_thread_fence(memory_order_acquire);
    if(bpf_map_lookup_elem(map,&key,&second))goto done;
    if(memcmp(&first,&second,sizeof(first))) { unavailable();goto done; }
    struct ap_fd_status status;
    if(ap_read_fd_status(s,&status))goto done;
    if(status.problem) { unavailable();goto done; }
    // All command/row authority is checked before any physical mutation.
    // Death can race the first live read or disarm. Preserve the exact phase
    // and raw result; the same Collect may prove detachment without a retry.
    bool exited=false,stored=false;
    if(fd_thread_exited(pidfd,&exited))goto done;
    if(exited && fd_dead_task_binding(s,p,pidfd,&stored))goto done;
    p->birth_completed=second;p->birth_collected=true;
    p->receipt=completed;
    if(fd_disarm_birth_task(s,p,pidfd,exited,stored))goto done;
    p->state=AP_SLOT_COLLECTED;
    rc=0;
done:
    leave_commands(s);return rc;
}
static int fd_ack_birth(struct ap_session *s,struct ap_pending_command *p) {
    if(p->submitted.operation!=AP_NATIVE_BIRTH)return 0;
    if(!p->birth_collected || (p->receipt.returned>0 &&
       (!p->birth_observed || (p->birth_child_admitted==p->birth_child_terminal)))) { errno=EPROTO;return -1; }
    int map=fd_map(s,"fd_calls");if(map<0)return -1;
    struct ap_invocation_key key={.task=p->receipt.task,.start=p->receipt.start_boottime};
    struct ap_fd_call actual;
    if(bpf_map_lookup_elem(map,&key,&actual))return -1;
    if(memcmp(&actual,&p->birth_completed,sizeof(actual))) { errno=ESTALE;return -1; }
    if(bpf_map_delete_elem(map,&key))return -1;
    if(!bpf_map_lookup_elem(map,&key,&actual)) { errno=EPROTO;return -1; }
    return errno==ENOENT?0:-1;
}

/* Positive-child terminal retirement only. Readiness of the held creator
 * pidfd proves no future producer for this command. The child marker must
 * already have been consumed by live admission or actual backend final wait. */
int ap_retire_dead_birth(struct ap_session *s,int pidfd,u64 command,
                         struct ap_native_birth_terminal *out) {
    if(!s || pidfd<0 || !command || !out)return invalid();
    if(enter_commands(s))return -1;
    int rc=-1;u32 slot=ap_command_slot(command);
    struct ap_pending_command *p=&s->pending[slot];
    if(p->state!=AP_SLOT_ACTIVE || p->submitted.command!=command ||
       p->submitted.operation!=AP_NATIVE_BIRTH) { invalid();goto done; }
    if(fd_require_dead_thread(pidfd))goto done;
    struct ap_native_birth_terminal retained={.call=p->submitted.expected_object};
    struct ap_command_result check;
    if(bpf_map_lookup_elem(s->commands,&slot,&retained.command) ||
       bpf_map_lookup_elem(s->commands,&slot,&check))goto done;
    *out=retained;
    if(memcmp(&retained.command,&check,sizeof(check)) || check.command!=command ||
       check.operation!=AP_NATIVE_BIRTH) { errno=ESTALE;goto done; }
    if((check.phase!=AP_COMMAND_RUNNING && check.phase!=AP_COMMAND_DONE) ||
       !check.task || !check.start_boottime ||
       (check.identity.provider && check.identity.provider!=s->incarnation) ||
       (check.phase==AP_COMMAND_DONE && (check.returned<=0 || check.identity.provider!=s->incarnation))) {
        errno=EPROTO;goto done;
    }
    if(!p->birth_observed || p->birth_child_admitted==p->birth_child_terminal) {
        errno=EAGAIN;goto done;
    }
    if(read_birth(s,command,&retained.birth))goto done;
    retained.fd_call_present=1;*out=retained;
    struct ap_task_command task;bool stored_task=false;
    if(!bpf_map_lookup_elem(s->tasks,&pidfd,&task)) {
        if(memcmp(&task,&p->submitted,sizeof(task))) { errno=ESTALE;goto done; }
        stored_task=true;
    } else if(errno!=ENOENT && errno!=ESRCH)goto done;
    int map=fd_map(s,"fd_calls");if(map<0)goto done;
    struct ap_invocation_key key={.task=check.task,.start=check.start_boottime};
    rc=fd_retire_dead_rows(s,p,pidfd,stored_task,map,&key,true,&retained.task_absent);
    *out=retained;
done:
    leave_commands(s);return rc;
}
