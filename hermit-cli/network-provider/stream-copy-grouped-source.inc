/* SPDX-License-Identifier: GPL-2.0 */
/*
 * Shared definition of the grouped skb byte producer.  The BPF object and the
 * host producer control include this exact body after supplying the same
 * narrow kernel-read, configuration-lookup, and ring-emission interfaces.
 * Keep this as a definition rather than a copied model: a bounds mutant here
 * must fail the host control that consumes it.
 */
static __attribute__((noinline)) int stream_copy_emit_grouped_source(
        struct ap_fd_call *call,struct sk_buff *skb) {
    struct ap_stream_copy_state *copy=&call->original.stream_copy;
    struct stream_copy_skb_view view={0};
    if(!stream_copy_read_skb(skb,&view) || !copy->requested ||
       copy->source>view.size || copy->requested>view.size-copy->source)return 0;
    const u64 output=copy->summary.initial_count-copy->before_count;
    if(!view.nonlinear)return ap_stream_copy_source_enter(0) &&
        view.data<=~0ULL-copy->source &&
        stream_copy_emit_range(call,view.data+copy->source,copy->requested,output) &&
        ap_stream_copy_source_exit(0);
    const u64 linear=view.size-view.nonlinear;
    u64 cursor=linear,start=copy->source,remaining=copy->requested;
    const u64 linear_take=ap_stream_copy_linear_take(start,remaining,linear);
    if(linear_take) {
        if(!ap_stream_copy_source_enter(0) || view.data>~0ULL-start ||
           !stream_copy_emit_range(call,view.data+start,linear_take,output))return 0;
        if(!ap_stream_copy_source_exit(0))return 0;
        remaining-=linear_take;start+=linear_take;
        if(!remaining)return 1;
    }
    if(!ap_stream_copy_source_enter(1) ||
       copy->copy_active>>AP_STREAM_COPY_TRANSPORT_SHIFT!=AP_STREAM_COPY_TCP)return 0;
    struct skb_shared_info *shared=(struct skb_shared_info *)(view.head+view.end);
    struct stream_copy_shared_view info={0};
    if(!stream_copy_read_shared(shared,&info) || !info.count ||
       info.count>sizeof(shared->frags)/sizeof(shared->frags[0]) ||
       info.frag_list || !ap_stream_copy_tcp_storage_flags(info.flags) || info.dataref<=0)return 0;
    u32 zero=0;struct ap_config *config=lookup(&ap_config_map,&zero);
    if(!config || config->anchor_phase!=AP_GROUPED_ANCHOR_ACTIVE ||
       !config->vmemmap_base || !config->page_offset_base)return 0;
    for(u32 i=0;i<sizeof(shared->frags)/sizeof(shared->frags[0]);i++) {
        if(i>=info.count)break;
        u64 netmem=0;u32 length=0,offset=0;
        if(fd_read_kernel(&netmem,sizeof(netmem),CORE(&shared->frags[i].netmem)) ||
           fd_read_kernel(&length,sizeof(length),CORE(&shared->frags[i].len)) ||
           fd_read_kernel(&offset,sizeof(offset),CORE(&shared->frags[i].offset)) ||
           !length || cursor>view.size || length>view.size-cursor)return 0;
        u64 within=0,take=0;
        const int window=ap_stream_copy_fragment_window(
            start,remaining,cursor,length,&within,&take);
        if(window<0) {cursor+=length;continue;}
        if(!window)return 0;
        if((u64)offset>0xffffffffULL-within)return 0;
        u64 emit_count=take;
#ifdef AP_STREAM_COPY_MUTATE_FRAGMENT_LENGTH
        emit_count=length;
#endif
        const u64 source=ap_stream_fragment_source(netmem,(u64)offset+within,emit_count,
            config->vmemmap_base,config->page_offset_base);
        if(!source || !stream_copy_emit_range(call,source,emit_count,
              output+(copy->requested-remaining)))return 0;
        remaining-=take;start+=take;cursor+=length;
        if(!remaining)return ap_stream_copy_source_exit(1);
    }
    return 0;
}
