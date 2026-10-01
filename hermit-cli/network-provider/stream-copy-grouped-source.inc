/* SPDX-License-Identifier: GPL-2.0 */
/* Actual source-coordinate walk shared by the BPF issuer and host controls.
 * A requested unit is immutable; the observed prefix is a separate bound. */
struct stream_copy_source_window {u64 source,length,nonlinear;};
static __attribute__((noinline)) int stream_copy_grouped_window(
        struct ap_fd_call *call,struct sk_buff *skb,u64 progress,
        struct stream_copy_source_window *out) {
    struct ap_stream_copy_state *copy=&call->original.stream_copy;
    struct stream_copy_skb_view view={0};
    if(!stream_copy_read_skb(skb,&view) || !copy->requested ||
       progress>=copy->requested || copy->source>view.size ||
       copy->requested>view.size-copy->source || view.nonlinear>view.size)return 0;
    const u64 start=copy->source+progress,remaining=copy->requested-progress;
    const u64 linear=view.size-view.nonlinear;
    const u64 linear_take=ap_stream_copy_linear_take(start,remaining,linear);
    if(linear_take) {
        if(view.data>~0ULL-start)return 0;
        *out=(struct stream_copy_source_window){view.data+start,linear_take,0};return 1;
    }
    if(!view.nonlinear ||
       copy->copy_active>>AP_STREAM_COPY_TRANSPORT_SHIFT!=AP_STREAM_COPY_TCP)return 0;
    struct skb_shared_info *shared=(struct skb_shared_info *)(view.head+view.end);
    struct stream_copy_shared_view info={0};
    if(!stream_copy_read_shared(shared,&info) || !info.count ||
       info.count>sizeof(shared->frags)/sizeof(shared->frags[0]) ||
       info.frag_list || !ap_stream_copy_tcp_storage_flags(info.flags) || info.dataref<=0)return 0;
    u32 zero=0;struct ap_config *config=lookup(&ap_config_map,&zero);
    if(!config || config->anchor_phase!=AP_GROUPED_ANCHOR_ACTIVE ||
       !config->vmemmap_base || !config->page_offset_base)return 0;
    u64 cursor=linear;
    for(u32 i=0;i<sizeof(shared->frags)/sizeof(shared->frags[0]);i++) {
        if(i>=info.count)break;
        u64 netmem=0;u32 length=0,offset=0;
        if(fd_read_kernel(&netmem,sizeof(netmem),CORE(&shared->frags[i].netmem)) ||
           fd_read_kernel(&length,sizeof(length),CORE(&shared->frags[i].len)) ||
           fd_read_kernel(&offset,sizeof(offset),CORE(&shared->frags[i].offset)) ||
           !length || cursor>view.size || length>view.size-cursor)return 0;
        u64 within=0,take=0;
        const int window=ap_stream_copy_fragment_window(start,remaining,cursor,length,&within,&take);
        if(window<0) {cursor+=length;continue;}
        if(!window || (u64)offset>0xffffffffULL-within)return 0;
#ifdef AP_STREAM_COPY_MUTATE_FRAGMENT_LENGTH
        take=length;
#endif
        const u64 source=ap_stream_fragment_source(netmem,(u64)offset+within,take,
            config->vmemmap_base,config->page_offset_base);
        if(!source)return 0;
        *out=(struct stream_copy_source_window){source,take,1};return 1;
    }
    return 0;
}
static __attribute__((noinline)) int stream_copy_emit_grouped_prefix(
        struct ap_fd_call *call,struct sk_buff *skb,u64 observed) {
    struct ap_stream_copy_state *copy=&call->original.stream_copy;
    if(observed>copy->requested || copy->before_count>copy->summary.initial_count)return 0;
    const u64 output=copy->summary.initial_count-copy->before_count;
    u64 progress=0;
    /* At most one linear window plus every actual skb fragment. */
    for(u32 i=0;i<=sizeof(((struct skb_shared_info *)0)->frags)/
            sizeof(((struct skb_shared_info *)0)->frags[0]);i++) {
        if(progress==observed)return 1;
        struct stream_copy_source_window window={0};
        if(!stream_copy_grouped_window(call,skb,progress,&window) ||
           !ap_stream_copy_source_enter(window.nonlinear))return 0;
        u64 count=window.length;
#ifdef AP_STREAM_FAULT_MUTATE_SOURCE
        window.source++;
#endif
        if(count>observed-progress)count=observed-progress;
        if(!stream_copy_emit_range(call,window.source,count,output+progress) ||
           !ap_stream_copy_source_exit(window.nonlinear))return 0;
        progress+=count;
    }
    return 0;
}
static __attribute__((noinline)) int stream_copy_emit_grouped_source(
        struct ap_fd_call *call,struct sk_buff *skb) {
    return stream_copy_emit_grouped_prefix(call,skb,call->original.stream_copy.requested);
}
