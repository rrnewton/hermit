/* SPDX-License-Identifier: GPL-2.0 */
/* Exact installed-image terminal inline ITER_UBUF REP MOVSB witness.
 * This is physical failed-store evidence, not a destination/Drain authority. */
#include "stream-copy-fault.h"
#ifdef __BPF__
struct {__uint(type,BPF_MAP_TYPE_ARRAY);__uint(max_entries,AP_COMMANDS);
    __type(key,u32);__type(value,struct ap_stream_fault_state);
} stream_copy_faults SEC(".maps");
#endif
static __attribute__((noinline)) struct ap_stream_fault_state *stream_copy_fault_state(
        struct ap_fd_call *call) {
    const u32 slot=ap_command_slot(call->command);
    return lookup(&stream_copy_faults,&slot);
}
static __attribute__((noinline)) int stream_copy_fault_same(
        const struct ap_fd_call *call,const struct ap_stream_fault_state *state) {
    const struct ap_original_selection *id=&call->original.selection;
    const struct ap_stream_copy_state *copy=&call->original.stream_copy;
    return state && state->phase && !call->original.problem &&
        state->provider==id->provider && state->command==id->command &&
        state->command==call->command && state->call==id->call &&
        state->task==id->task && state->start==id->task_start &&
        state->file==call->selected_file && state->pointer==call->selection.word &&
        state->frame==call->security_socket && state->attempt==copy->summary.attempts &&
        state->iterator==copy->iterator && state->skb==copy->skb &&
        state->source==copy->source && state->requested==copy->requested &&
        state->before==copy->before_count && (copy->copy_active&AP_STREAM_COPY_PHASE_MASK)==1;
}
static __attribute__((noinline)) int stream_copy_fault_arm(
        struct ap_fd_call *call,struct pt_regs *ctx) {
    struct ap_stream_fault_state *state=stream_copy_fault_state(call);
    struct ap_stream_copy_state *copy=&call->original.stream_copy;
    u32 zero=0;struct ap_config *config=lookup(&ap_config_map,&zero);
    u64 iter[4]={0};
    if(!state || state->phase || !config || config->anchor_phase!=AP_GROUPED_ANCHOR_ACTIVE ||
       fd_function_ip(ctx)!=ap_grouped_image_address(config->anchor_ip,AP_STREAM_FAULT_COPY_IMAGE) ||
       fd_read_kernel(iter,sizeof(iter),(const void *)copy->iterator) ||
       iter[3]!=copy->before_count)return 0;
    const struct ap_original_selection *id=&call->original.selection;
    state->provider=id->provider;state->command=call->command;state->call=id->call;
    state->task=id->task;state->start=id->task_start;state->file=call->selected_file;
    state->pointer=call->selection.word;state->frame=call->security_socket;
    state->attempt=copy->summary.attempts;state->iterator=copy->iterator;state->skb=copy->skb;
    state->source=copy->source;state->requested=copy->requested;state->before=copy->before_count;
    state->entry_offset=iter[1];state->ubuf=iter[2];state->iter_type=iter[0]&0xffffff;
    state->observed=0;state->fault_frame=0;state->phase=1;
    return 1;
}
/* Before fixup, iterator count still excludes only earlier successful inner
 * calls. BX-CX is this terminal REP's independently observed stored prefix.
 * The source coordinate and exact inner window join both contributions. */
static __attribute__((noinline)) int stream_copy_fault_enter(
        struct ap_fd_call *call,const u64 *args) {
    struct ap_stream_fault_state *state=stream_copy_fault_state(call);
    struct ap_stream_copy_state *copy=&call->original.stream_copy;
    u32 zero=0;struct ap_config *config=lookup(&ap_config_map,&zero);
    if(!stream_copy_fault_same(call,state) || state->phase!=1 || state->iter_type ||
       !config || config->anchor_phase!=AP_GROUPED_ANCHOR_ACTIVE ||
       !stream_copy_frontier_locked_frame(call,copy->copy_active>>8) ||
       !stream_copy_frontier_semantics(call,copy->copy_active>>8))goto failed;
    /* Mark entered BEFORE reads; nested callbacks must not replace this frame. */
    state->phase=2;state->fault_frame=args[0];state->trap=args[1];
    state->error=args[2];state->address=args[3];
    if(!state->fault_frame || state->trap!=14 || !(state->error&2) || (state->error&~3ULL) ||
       fd_read_kernel(&state->regs,sizeof(state->regs),(const void *)state->fault_frame))goto failed;
    const struct ap_stream_fault_regs *r=&state->regs;
    const u64 insn=ap_grouped_image_address(config->anchor_ip,AP_STREAM_FAULT_INSN_IMAGE);
    const u64 extable=ap_grouped_image_address(config->anchor_ip,AP_STREAM_FAULT_EXTABLE_IMAGE);
    u32 ex[3]={0};u8 code[8]={0};u64 iter[4]={0};
    if(!insn || !extable || r->ip!=insn || r->cs!=0x10 || (r->flags&0x400) ||
       r->dx!=state->iterator || !r->bx || !r->cx || r->cx>r->bx ||
       fd_read_kernel(ex,sizeof(ex),(const void *)extable) ||
       ex[0]!=0xff25f708U || ex[1]!=0xff25f709U || ex[2]!=3 ||
       fd_read_kernel(code,sizeof(code),(const void *)insn) ||
       code[0]!=0xf3 || code[1]!=0xa4 || code[2]!=0x90 || code[3]!=0x90 ||
       code[4]!=0x90 || code[5]!=0x90 || code[6]!=0x90 || code[7]!=0x90 ||
       fd_read_kernel(iter,sizeof(iter),(const void *)state->iterator) ||
       (iter[0]&0xffffff) || iter[2]!=state->ubuf || iter[3]>state->before)goto failed;
    const u64 prior=state->before-iter[3],within=r->bx-r->cx;
    if(prior>=state->requested || within>=state->requested-prior ||
       state->entry_offset>~0ULL-prior || iter[1]!=state->entry_offset+prior ||
       state->ubuf>=(1ULL<<47) || iter[1]>=(1ULL<<47)-state->ubuf ||
       r->bx>(1ULL<<47)-state->ubuf-iter[1] ||
       r->di!=state->ubuf+iter[1]+within || state->address!=r->di ||
       r->r14>~0ULL-within || r->si!=r->r14+within ||
       !stream_copy_unit_custody((void *)state->skb,state->source,state->requested,copy->copy_active>>8))
        goto failed;
    struct stream_copy_source_window window={0};
    if(!stream_copy_grouped_window(call,(void *)state->skb,prior,&window))goto failed;
#ifdef AP_STREAM_FAULT_MUTATE_WINDOW
    window.length--;
#endif
    if(
       !ap_stream_copy_source_enter(window.nonlinear) ||
       r->r14!=window.source || r->bx!=window.length || r->bx>iter[3])goto failed;
    state->nonlinear=window.nonlinear;
    state->observed=prior+within;
#ifdef AP_STREAM_FAULT_MUTATE_CX
    state->observed++;
#endif
    return 1;
failed:
    stream_copy_problem(call,AP_FD_OUTCOME);return 0;
}
static __attribute__((noinline)) int stream_copy_fault_exit(
        struct ap_fd_call *call,const u64 *args) {
    struct ap_stream_fault_state *state=stream_copy_fault_state(call);
    u32 zero=0;struct ap_config *config=lookup(&ap_config_map,&zero);
    struct ap_stream_fault_regs after={0};u64 iter[4]={0};
    if(!stream_copy_fault_same(call,state) || state->phase!=2 || !config ||
       config->anchor_phase!=AP_GROUPED_ANCHOR_ACTIVE ||
       args[0]!=state->fault_frame || args[1]!=state->trap ||
       args[2]!=state->error || args[3]!=state->address || (s32)args[4]!=1 ||
       fd_read_kernel(&after,sizeof(after),(const void *)state->fault_frame) ||
       after.ip!=ap_grouped_image_address(config->anchor_ip,AP_STREAM_FAULT_NEXT_IMAGE) ||
       !ap_stream_copy_source_exit(state->nonlinear))goto failed;
    /* This exact type3 fixup changes ONLY saved IP. No caller-supplied count. */
    after.ip=state->regs.ip;
    const u64 *a=(const u64 *)&after,*b=(const u64 *)&state->regs;
    for(u32 i=0;i<sizeof(after)/sizeof(u64);i++)if(a[i]!=b[i])goto failed;
    if(fd_read_kernel(iter,sizeof(iter),(const void *)state->iterator) ||
       (iter[0]&0xffffff)!=state->iter_type || iter[2]!=state->ubuf ||
       iter[3]>state->before || iter[1]!=state->entry_offset+(state->before-iter[3]) ||
       state->observed!=state->before-iter[3]+state->regs.bx-state->regs.cx)goto failed;
    state->phase=3;return 1;
failed:
    stream_copy_problem(call,AP_FD_OUTCOME);return 0;
}
static __attribute__((noinline)) int stream_copy_fault_complete(
        struct ap_fd_call *call,s64 returned,u64 *observed) {
    struct ap_stream_fault_state *state=stream_copy_fault_state(call);
    if(!stream_copy_fault_same(call,state) ||
       (!returned ? state->phase!=1 : (returned!=-14 || state->phase!=3)))return 0;
    *observed=returned?state->observed:state->requested;
#ifdef AP_STREAM_FAULT_MUTATE_DROP_PRIOR
    if(returned)*observed=state->regs.bx-state->regs.cx;
#endif
    if(*observed>state->requested || (returned && *observed==state->requested))return 0;
    state->phase=0;return 1;
}
