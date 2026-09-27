/* SPDX-License-Identifier: GPL-2.0 */
#include "grouped-probes.h"
static u64 (*fd_attach_cookie_raw)(void *)=(void *)BPF_FUNC_get_attach_cookie;
static __attribute__((noinline)) unsigned fd_grouped_role(struct pt_regs *ctx) {
    u32 zero=0;struct ap_config *c=lookup(&ap_config_map,&zero);
    /* No guest command can be admitted until the loader has read back the
     * actual one-use anchor and published ACTIVE. Unrelated host callbacks
     * during attachment cannot fabricate an anchor from their own PC. */
    if(!c || c->anchor_phase<AP_GROUPED_ANCHORED)return 0;
    if(c->anchor_phase>AP_GROUPED_ANCHOR_ACTIVE || !c->anchor_task ||
       !c->anchor_start || !c->anchor_ip)return 0;
    return ap_grouped_site_role(c->anchor_ip,CORE(ctx->ip));
}
static __attribute__((noinline)) u64 fd_attach_cookie(void *raw) {
    struct pt_regs *ctx=raw;u64 cookie=fd_attach_cookie_raw(raw);
    return cookie==AP_GROUPED_COOKIE?ap_grouped_semantic_cookie(fd_grouped_role(ctx)):cookie;
}
static __attribute__((noinline)) int fd_grouped_bootstrap(struct pt_regs *ctx,u64 kind) {
    u32 zero=0;struct ap_config *c=lookup(&ap_config_map,&zero);
    if(!c || c->anchor_phase!=AP_GROUPED_ANCHOR_ARMED || kind!=AP_CONNECT_ENTRY_COOKIE ||
       c->anchor_task!=pid_tgid())return 0;
    u64 ip=fd_function_ip(ctx),start=CORE(current_task()->start_boottime);
    u64 vmemmap_address=ap_grouped_image_address(ip,AP_GROUPED_VMEMMAP_BASE_IMAGE);
    u64 page_offset_address=ap_grouped_image_address(ip,AP_GROUPED_PAGE_OFFSET_BASE_IMAGE);
    u64 vmemmap_base=0,page_offset_base=0;
    if(bpf_session_is_return(ctx) || (s32)CORE(ctx->di)!=-1 || CORE(ctx->si) || CORE(ctx->dx) ||
       !start || c->anchor_start || c->anchor_ip || c->vmemmap_base || c->page_offset_base ||
       !ap_grouped_site_ip(1,ip) || !vmemmap_address || !page_offset_address ||
       fd_read_kernel(&vmemmap_base,sizeof(vmemmap_base),(const void *)vmemmap_address) ||
       fd_read_kernel(&page_offset_base,sizeof(page_offset_base),(const void *)page_offset_address)) {
        fd_problem(AP_FD_IDENTITY);return 1;
    }
    c->anchor_ip=ip;c->anchor_start=start;
    c->vmemmap_base=vmemmap_base;c->page_offset_base=page_offset_base;
    if(__sync_val_compare_and_swap(&c->anchor_phase,AP_GROUPED_ANCHOR_ARMED,AP_GROUPED_ANCHORED)!=AP_GROUPED_ANCHOR_ARMED)
        fd_problem(AP_FD_DUPLICATE);
    return 1;
}
#ifndef AP_FTRACE_PROVIDER
static __attribute__((noinline)) int fd_stream_copy_enter(struct pt_regs *);
static __attribute__((noinline)) int fd_stream_copy_exit(struct pt_regs *);
static __attribute__((noinline)) int stream_membership_dispatch(struct pt_regs *);
#endif
