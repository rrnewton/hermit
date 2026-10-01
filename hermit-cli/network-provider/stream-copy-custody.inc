/* A kprobe register or a pointer saved in a map is a scalar to the verifier.
 * CO-RE relocates these field addresses; probe_read_kernel performs every
 * actual dereference and a failed read refuses the custody certificate. */
struct stream_copy_skb_view {
    u64 head,data;
    u32 tail,end,size,nonlinear;
    s32 users;
};
struct stream_copy_shared_view { u64 frag_list; s32 dataref; u8 count,flags; };
static __attribute__((noinline)) int stream_copy_read_skb(
        struct sk_buff *skb,struct stream_copy_skb_view *out) {
    return skb &&
        !fd_read_kernel(&out->users,sizeof(out->users),CORE(&skb->users.refs.counter)) &&
        !fd_read_kernel(&out->head,sizeof(out->head),CORE(&skb->head)) &&
        !fd_read_kernel(&out->data,sizeof(out->data),CORE(&skb->data)) &&
        !fd_read_kernel(&out->tail,sizeof(out->tail),CORE(&skb->tail)) &&
        !fd_read_kernel(&out->end,sizeof(out->end),CORE(&skb->end)) &&
        !fd_read_kernel(&out->size,sizeof(out->size),CORE(&skb->len)) &&
        !fd_read_kernel(&out->nonlinear,sizeof(out->nonlinear),CORE(&skb->data_len));
}
static __attribute__((noinline)) int stream_copy_read_shared(
        struct skb_shared_info *shared,struct stream_copy_shared_view *out) {
    return !fd_read_kernel(&out->count,sizeof(out->count),CORE(&shared->nr_frags)) &&
        !fd_read_kernel(&out->flags,sizeof(out->flags),CORE(&shared->flags)) &&
        !fd_read_kernel(&out->frag_list,sizeof(out->frag_list),CORE(&shared->frag_list)) &&
        !fd_read_kernel(&out->dataref,sizeof(out->dataref),CORE(&shared->dataref.counter));
}

/* The positive slab result is joined to the valid skb ownership contract:
 * one skb owner, one FULL data reference, no page-backed head or nonlinear
 * payload. Header-cloned/low-half tests do not exclude payload-only aliases.
 * No mapping==NULL inference or borrowed page snapshot is payload authority. */
static __attribute__((noinline)) int stream_copy_linear_head(struct sk_buff *skb,
        u64 source,u64 length,const struct stream_copy_skb_view *view) {
    if(view->nonlinear || view->users!=1)return 0;
    const u64 head=view->head,data=view->data,tail=view->tail,end=view->end,size=view->size;
    u64 head_frag=0;
    const u32 bit_bytes=__builtin_preserve_field_info(skb->head_frag,1);
    const u32 bit_offset=__builtin_preserve_field_info(skb->head_frag,0);
    if(!bit_bytes || bit_bytes>8 || fd_read_kernel(&head_frag,bit_bytes,
          (const u8 *)skb+bit_offset))return 0;
    head_frag<<=__builtin_preserve_field_info(skb->head_frag,4);
    head_frag>>=__builtin_preserve_field_info(skb->head_frag,5);
    if(head_frag)return 0;
    if(!head || !data || end>~0ULL-head || tail>end || data<head ||
       data>head+tail || size!=head+tail-data || source<data ||
       source>head+tail || length>head+tail-source || !bpf_get_kmem_cache(head))return 0;
    struct skb_shared_info *shared=(struct skb_shared_info *)(head+end);
    struct stream_copy_shared_view info={0};
    if(!stream_copy_read_shared(shared,&info))return 0;
    return !info.count && !info.frag_list && info.dataref==1;
}

/* Qualified tcp_recvmsg_locked consumes the native TCP receive queue. TCP
 * marks externally overwritable splice fragments SHARED_FRAG; its ordinary
 * copy-to-page producer owns the copied range even when the page is shared
 * with a cache or another immutable skb. skb_try_coalesce propagates that mark.
 * ZEROCOPY/managed fragments and frag lists require separate certificates.
 * Unix's splice helper does not establish this contract and is excluded.
 * See the retained producer/sanitization source for this protocol-specific
 * invariant: a refcount or mapping==NULL is not used as its substitute. */
static __attribute__((noinline)) int stream_copy_tcp_fragments(struct sk_buff *skb,u64 offset,u64 length,
        const struct stream_copy_skb_view *view) {
    if(view->users!=1)return 0;
    const u64 head=view->head,data=view->data,tail=view->tail,end=view->end,
        size=view->size,nonlinear=view->nonlinear;
    u64 unreadable=0;
    const u32 bit_bytes=__builtin_preserve_field_info(skb->unreadable,1);
    const u32 bit_offset=__builtin_preserve_field_info(skb->unreadable,0);
    if(!bit_bytes || bit_bytes>8 || fd_read_kernel(&unreadable,bit_bytes,(const u8 *)skb+bit_offset))return 0;
    unreadable<<=__builtin_preserve_field_info(skb->unreadable,4);
    unreadable>>=__builtin_preserve_field_info(skb->unreadable,5);
    if(unreadable)return 0;
    if(!head || !data || end>~0ULL-head || tail>end || data<head || data>head+tail ||
       !nonlinear || nonlinear>size || size-nonlinear!=head+tail-data ||
       offset>size || length>size-offset)return 0;
    struct skb_shared_info *shared=(struct skb_shared_info *)(head+end);
    struct stream_copy_shared_view info={0};
    if(!stream_copy_read_shared(shared,&info))return 0;
    const u32 count=info.count;
    if(!count || count>sizeof(shared->frags)/sizeof(shared->frags[0]) ||
       info.frag_list || !ap_stream_copy_tcp_storage_flags(info.flags) || info.dataref<=0)return 0;
    if(ap_stream_copy_linear_take(offset,length,size-nonlinear)) {
        u64 head_frag=0;
        const u32 head_frag_bytes=__builtin_preserve_field_info(skb->head_frag,1);
        const u32 head_frag_offset=__builtin_preserve_field_info(skb->head_frag,0);
        if(!head_frag_bytes || head_frag_bytes>8 || fd_read_kernel(&head_frag,head_frag_bytes,
              (const u8 *)skb+head_frag_offset))return 0;
        head_frag<<=__builtin_preserve_field_info(skb->head_frag,4);
        head_frag>>=__builtin_preserve_field_info(skb->head_frag,5);
        if(!ap_stream_copy_mixed_head_owned(offset,length,size,nonlinear,
              info.dataref,head_frag))return 0;
    }
    u64 bytes=0;
    for(u32 i=0;i<sizeof(shared->frags)/sizeof(shared->frags[0]);i++) {
        if(i>=count)break;
        u64 netmem=0;u32 length=0,offset=0;
        if(fd_read_kernel(&netmem,sizeof(netmem),CORE(&shared->frags[i].netmem)) ||
           fd_read_kernel(&length,sizeof(length),CORE(&shared->frags[i].len)) ||
           fd_read_kernel(&offset,sizeof(offset),CORE(&shared->frags[i].offset)))return 0;
        /* net_iov device memory is not host-readable page storage. No page
         * ownership conclusion is drawn from this representation check. */
        if(!netmem || (netmem&1) || !length || offset>0xffffffffULL-length ||
           bytes>nonlinear || length>nonlinear-bytes)return 0;
        bytes+=length;
    }
    return bytes==nonlinear;
}
static __attribute__((noinline)) int stream_copy_unit_custody(
        struct sk_buff *skb,u64 offset,u64 length,u64 transport) {
    struct stream_copy_skb_view view={0};
    if(!stream_copy_read_skb(skb,&view))return 0;
    if(view.nonlinear)return transport==AP_STREAM_COPY_TCP &&
        stream_copy_tcp_fragments(skb,offset,length,&view);
    return view.data<=~0ULL-offset && stream_copy_linear_head(skb,view.data+offset,length,&view);
}
