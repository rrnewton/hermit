/* SPDX-License-Identifier: GPL-2.0 */
/* The loader admits exactly four outer and five inner address/cookie pairs
 * before ready; the original owned-link metadata check is mandatory. Read the
 * full physical cookie, not the legacy AP_GROUPED_COOKIE site translation.
 * An inadmissible cookie performs no state access below and returns 1 (skip
 * this session's return). That is NOT a sticky product refusal by itself:
 * correctness relies on rejecting the malformed link before commands start.
 * Do not pollute host-wide diagnostic state for an unadmitted callback. */
SEC("kprobe.session") __attribute__((nodebug)) int fd_so(struct pt_regs *ctx) {
#ifdef AP_GROUPED_PROVIDER
    const u64 kind=fd_attach_cookie_raw(ctx);
#else
    const u64 kind=fd_attach_cookie(ctx);
#endif
    if(kind!=AP_ACCEPT_ENTRY_COOKIE && kind!=AP_CONNECT_ENTRY_COOKIE &&
       kind!=AP_FDUPFD_COOKIE && kind!=AP_EPOLL_CTL_COOKIE)return 1;
#ifdef AP_GROUPED_PROVIDER
    if(fd_grouped_bootstrap(ctx,kind))return 1;
#endif
    if(kind==AP_EPOLL_CTL_COOKIE)return fd_epoll_ctl_session(ctx);
    if(kind==AP_FDUPFD_COOKIE)return fd_fdupfd_session(ctx,kind);
    if(bpf_session_is_return(ctx)) {fd_problem(AP_FD_IDENTITY);return 1;}
    struct ap_task_command *c=command();
    u64 operation=kind==AP_ACCEPT_ENTRY_COOKIE?AP_ACCEPT_EFFECT:AP_ORIGINAL_CONNECT;
    if(!c || c->operation!=operation)return 1;
    struct ap_command_result *r=claim_result(c);if(!r)return 1;
    int published=kind==AP_ACCEPT_ENTRY_COOKIE?
        fd_accept_enter(ctx,kind,c):fd_connect_enter(ctx,kind,c);
    if(published)r->identity.provider=incarnation();
    return 1;
}
SEC("kprobe.session") __attribute__((nodebug)) int fd_si(struct pt_regs *ctx) {
#ifdef AP_GROUPED_PROVIDER
    const u64 kind=fd_attach_cookie_raw(ctx);
#else
    const u64 kind=fd_attach_cookie(ctx);
#endif
    if(kind!=AP_CONNECT_SECURITY_COOKIE && kind!=AP_CONNECT_AUDIT_COOKIE &&
       kind!=AP_FDUPFD_ALLOC_COOKIE && kind!=AP_FILE_FDGET_COOKIE &&
       kind!=AP_READ_FDGET_COOKIE)return 1;
    /* None of these five literal cookies can satisfy the unchanged bootstrap
     * helper's kind==AP_CONNECT_ENTRY_COOKIE guard. No anchor transition is
     * moved to an inner function, and all delegated helper bodies are intact. */
    if(kind==AP_FILE_FDGET_COOKIE)return fd_file_fdget_session(ctx);
    if(kind==AP_READ_FDGET_COOKIE)return fd_read_fdget_session(ctx);
    if(kind==AP_FDUPFD_ALLOC_COOKIE)return fd_fdupfd_session(ctx,kind);
    return fd_connect_session(ctx);
}
