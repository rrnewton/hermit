/* SPDX-License-Identifier: MIT */
#ifndef HERMIT_PROVIDER_STREAM_TX_H
#define HERMIT_PROVIDER_STREAM_TX_H
#include "stream-copy.h"
#include "stream-tx-image.h"
#include "stream-tx-live-image.h"

/* AUTONOMOUS-BOT-IMPLEMENTED: bounded original Sendto capture, not a guest
 * buffer snapshot. TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/3464).
 * Receive copy version/units and their authority are deliberately unchanged. */
#define AP_ORIGINAL_SENDTO_CALL 24ULL
#define AP_STREAM_TX_BLOCKING_VERSION 2ULL
#define AP_SENDTO_SYSCALL 44ULL
#define AP_STREAM_TX_VERSION 1ULL
#define AP_STREAM_TX_COOKIE 24ULL
#define AP_STREAM_TX_LOCK_COOKIE 25ULL
#define AP_STREAM_TX_UNLOCK_COOKIE 26ULL
#define AP_STREAM_TX_IMAGE 0xffffffff81fbabf0ULL
#define AP_STREAM_TX_LOCK_IMAGE 0xffffffff81fb70c0ULL
#define AP_STREAM_TX_UNLOCK_IMAGE 0xffffffff81f54820ULL
#define AP_STREAM_TX_SYMBOL "tcp_sendmsg"
#define AP_STREAM_TX_LOCK_SYMBOL "lock_sock_nested"
#define AP_STREAM_TX_UNLOCK_SYMBOL "release_sock"
#define AP_STREAM_TX_LOCK_RETURN 0x28ULL
#define AP_STREAM_TX_UNLOCK_RETURN 0x85dULL
#define AP_STREAM_TX_INNER_STACK 0xe0ULL
#define AP_STREAM_TX_DATA 6U
#define AP_STREAM_TX_COMMIT 7U
#define AP_STREAM_TX_FLAGS 0x4040U
/* Exact little-endian x86 image bytes, represented as scalar immediates so
 * LLVM does not introduce an owned .rodata BPF map for an expected-byte table.
 * The caller reads only 35/28 bytes into zero-initialized stack words. */
static __attribute__((always_inline)) inline int ap_stream_tx_image_words(
        const u64 a[5],const u64 b[4]) {
    return a[0]==0x4155415641574155ULL && a[1]==0x0000a8ec81485354ULL &&
        a[2]==0x48f78949d4894900ULL && a[3]==0xa8e8f631ed31fb89ULL &&
        a[4]==0xffffc4ULL && b[0]==0xfff993d3e8df8948ULL &&
        b[1]==0x0000a8c48148e889ULL && b[2]==0x5e415d415c415b00ULL &&
        b[3]==0xc35d5f41ULL;
}
struct ap_stream_tx_summary {
    u64 version,file,requested,captured,sequence_before,sequence_after;
    u64 protocol_returned,protocol_complete;
};
/* Only summary is public. These borrowed references are cleared before the
 * original syscall result is published; terminal export clears the union. */
struct ap_stream_tx_state {
    struct ap_stream_tx_summary summary;
    /* The first72 bytes are the v2 public summary. This remains inside the
     * original128-byte union; v1 leaves this word and its public tail zero. */
    u64 saved_timeout_ticks;
    u64 socket,message,function,stack,records,active;
};
struct ap_stream_tx_blocking_summary {
    struct ap_stream_tx_summary prefix;
    u64 saved_timeout_ticks;
};
struct ap_stream_tx_blocking_capture {
    u64 provider,command,call,task,task_start;
    s64 returned;
    struct ap_stream_tx_blocking_summary summary;
    u8 bytes[AP_STREAM_COPY_BYTES];
};
_Static_assert(sizeof(struct ap_stream_tx_blocking_summary)==72,"blocking TX summary ABI");
_Static_assert(sizeof(struct ap_stream_tx_blocking_capture)==632,"blocking TX capture ABI");
struct ap_stream_tx_capture {
    u64 provider,command,call,task,task_start;
    s64 returned;
    struct ap_stream_tx_summary summary;
    u8 bytes[AP_STREAM_COPY_BYTES];
};
_Static_assert(sizeof(struct ap_stream_tx_summary)==64,"TX summary ABI");
_Static_assert(sizeof(struct ap_stream_tx_state)<=128,"existing original union");
_Static_assert(sizeof(struct ap_stream_tx_capture)==624,"TX capture ABI");
static inline int ap_stream_tx_flags(u64 flags) {return flags==0x4000 || flags==AP_STREAM_TX_FLAGS;}
static inline int ap_stream_tx_command(const struct ap_task_command *c) {
    return c && c->operation==AP_ORIGINAL_SENDTO_CALL && c->provider && c->command &&
        c->expected_object && c->expected_level>=0 && c->generation_before &&
        c->original_count && c->original_count<=AP_STREAM_COPY_BYTES &&
        ap_stream_tx_flags((u32)c->expected_option);
}
static __attribute__((always_inline)) inline int ap_stream_tx_operands(
        const struct ap_task_command *c,u64 nr,u64 fd,u64 buffer,u64 count,
        u64 flags,u64 destination,u64 address_length) {
    return ap_stream_tx_command(c) && nr==AP_SENDTO_SYSCALL &&
        fd==(u64)(u32)c->expected_level && buffer==c->generation_before &&
        count==c->original_count && flags==(u64)(u32)c->expected_option &&
        !destination && !address_length;
}
static inline int ap_stream_tx_summary_valid(const struct ap_stream_tx_summary *s,
        u64 file,u64 requested,s64 returned) {
    return s && file && requested && requested<=AP_STREAM_COPY_BYTES &&
        returned>=-4095 && returned<=(s64)requested &&
        s->version==AP_STREAM_TX_VERSION && s->file==file && s->requested==requested &&
        s->captured==(returned>0?(u64)returned:0) &&
        s->sequence_before<=0xffffffffULL && s->sequence_after<=0xffffffffULL &&
        (u32)((u32)s->sequence_after-(u32)s->sequence_before)==s->captured &&
        s->protocol_returned==(u64)returned && s->protocol_complete==1;
}
/* Separate positive-only blocking grammar. Never weaken v1's predicates or
 * select a larger public layout from frame-controlled length/version bytes. */
static inline int ap_stream_tx_operation(u64 operation) {
    return operation==AP_ORIGINAL_SENDTO_CALL || operation==AP_ORIGINAL_SENDTO_BLOCKING_CALL;
}
static inline int ap_stream_tx_timeout(u64 ticks) {
    return ticks && ticks<=AP_STREAM_TX_TIMEOUT_MAX;
}
static inline int ap_stream_tx_blocking_command(const struct ap_task_command *c) {
    return c && c->operation==AP_ORIGINAL_SENDTO_BLOCKING_CALL && c->provider && c->command &&
        c->expected_object && c->expected_level>=0 && c->generation_before &&
        c->original_count && c->original_count<=AP_STREAM_COPY_BYTES &&
        c->expected_option==0x4000 && ap_stream_tx_timeout(c->expected_timeout_ticks);
}
static inline int ap_stream_tx_any_command(const struct ap_task_command *c) {
    return ap_task_command_extension_valid(c) &&
        (ap_stream_tx_command(c) || ap_stream_tx_blocking_command(c));
}
static __attribute__((always_inline)) inline int ap_stream_tx_any_operands(
        const struct ap_task_command *c,u64 nr,u64 fd,u64 buffer,u64 count,
        u64 flags,u64 destination,u64 address_length) {
    return ap_task_command_extension_valid(c) &&
        (ap_stream_tx_operands(c,nr,fd,buffer,count,flags,destination,address_length) ||
        (ap_stream_tx_blocking_command(c) && nr==AP_SENDTO_SYSCALL &&
         fd==(u64)(u32)c->expected_level && buffer==c->generation_before &&
         count==c->original_count && flags==0x4000 && !destination && !address_length));
}
static __attribute__((always_inline)) inline int ap_stream_tx_blocking_parts_valid(
        const struct ap_stream_tx_summary *s,u64 saved,u64 file,u64 requested,s64 returned,u64 timeout) {
    return s && file && requested && requested<=AP_STREAM_COPY_BYTES &&
        returned>0 && returned<=(s64)requested &&
        s->version==AP_STREAM_TX_BLOCKING_VERSION && s->file==file && s->requested==requested &&
        s->captured==(u64)returned &&
        s->sequence_before<=0xffffffffULL && s->sequence_after<=0xffffffffULL &&
        (u32)((u32)s->sequence_after-(u32)s->sequence_before)==s->captured &&
        s->protocol_returned==(u64)returned && s->protocol_complete==1 &&
        ap_stream_tx_timeout(timeout) && saved==timeout;
}
static inline int ap_stream_tx_blocking_summary_valid(const struct ap_stream_tx_blocking_summary *b,
        u64 file,u64 requested,s64 returned,u64 timeout) {
    return b && ap_stream_tx_blocking_parts_valid(&b->prefix,b->saved_timeout_ticks,
        file,requested,returned,timeout);
}
/* The real producer currently requires exactly one retained rtx OR unsent SKB.
 * The <=512 interval makes modulo-u32 comparison unambiguous. Intersections
 * must cover the next byte exactly: no gap, overlap, reordering or duplicate.
 * Returns 2 for a wholly disjoint SKB, 1 for the exact next intersection, and
 * 0 on malformed geometry. Host controls call this same production decoder. */
struct ap_stream_tx_interval {u64 file;u32 first,length,covered;};
/* No traversal: one root without children, or one circular-list element, and
 * the other queue must be empty. A larger queue is unsupported, not truncated. */
struct ap_stream_tx_queue_view {
    u64 root,left,right,parent,head,next,previous,element_next,element_previous;
    u32 queued;
};
static __attribute__((always_inline)) inline int ap_stream_tx_single_queue(
        const struct ap_stream_tx_queue_view *q) {
    if(!q || !q->head)return 0;
    if(q->root)return !q->left && !q->right && !(q->parent&~3ULL) && !q->queued &&
        q->next==q->head && q->previous==q->head;
    return q->queued==1 && q->next && q->next!=q->head && q->previous==q->next &&
        q->element_next==q->head && q->element_previous==q->head;
}
/* Exact installed tcp_sendmsg prologue owns six saves + 0xa8 locals; either
 * native callee's return slot is 0xe0 below the original function entry. */
static inline int ap_stream_tx_caller(u64 function,u64 outer_stack,u64 stack,
        u64 returned,u64 relative) {
    return function && outer_stack>=AP_STREAM_TX_INNER_STACK &&
        stack==outer_stack-AP_STREAM_TX_INNER_STACK && function<=~0ULL-relative &&
        returned==function+relative;
}
static __attribute__((always_inline)) inline int ap_stream_tx_interval_piece(struct ap_stream_tx_interval *v,
        u64 file,u32 sequence,u32 end,u32 bytes,u32 flags,u64 *offset,u64 *length) {
    if(!v || !file || file!=v->file || !v->length || v->length>AP_STREAM_COPY_BYTES ||
       v->covered>v->length || !offset || !length || bytes>0x7fffffffU)return 0;
    const s64 lo=(s32)(sequence-v->first),hi=lo+(u32)(end-sequence);
    if(hi<=0 || lo>=(s64)v->length)return 2;
    if((flags&3U) || (u32)(end-sequence)!=bytes || !bytes || hi<=lo)return 0;
    const s64 begin=lo<0?0:lo,finish=hi>(s64)v->length?(s64)v->length:hi;
    if(begin!=(s64)v->covered || finish<=begin)return 0;
    *offset=(u64)(begin-lo);*length=(u64)(finish-begin);
    v->covered+=(u32)*length;return 1;
}
/* Exact full-storage geometry, before any payload is emitted. The copied TCP
 * range may be shared with an immutable transmit clone; a refcount of one is
 * not required for dataref. External/zerocopy/frag_list storage is refused. */
static __attribute__((always_inline)) inline int ap_stream_tx_storage(u64 head,u64 data,u32 tail,u32 end,u32 bytes,
        u32 nonlinear,s32 users,u32 count,u32 flags,u64 frag_list,s32 dataref) {
    return head && data && users==1 && end<=~0ULL-head && tail<=end &&
        data>=head && data<=head+tail && nonlinear<=bytes &&
        bytes-nonlinear==head+tail-data && count<=17 &&
        (!!count)==!!nonlinear && !flags && !frag_list && dataref>0;
}
/* Preserve full geometry/provenance checks, but bound this TX implementation
 * to a linear head plus at most one ordinary page fragment. Receive's complete
 * fragment decoder and the shared storage predicate remain unchanged. */
static __attribute__((always_inline)) inline int ap_stream_tx_single_storage(u64 head,u64 data,u32 tail,u32 end,u32 bytes,
        u32 nonlinear,s32 users,u32 count,u32 flags,u64 frag_list,s32 dataref) {
    return count<=1 && ap_stream_tx_storage(head,data,tail,end,bytes,nonlinear,
        users,count,flags,frag_list,dataref);
}
static inline int ap_stream_tx_fragment(u64 netmem,u32 offset,u32 length,
        u64 nonlinear,u64 *accumulated) {
    if(!accumulated || !netmem || (netmem&1) || !length || offset>0xffffffffU-length ||
       *accumulated>nonlinear || length>nonlinear-*accumulated)return 0;
    *accumulated+=length;return 1;
}
#ifndef __BPF__
struct ap_stream_tx_owned {
    struct ap_stream_tx_capture capture;
    u64 records,received;
    bool committed,read;
};
int ap_prepare_original_sendto(struct ap_session *,int,u64,u64,int,u64,u64,int,u64 *);
int ap_original_sendto_capture(struct ap_session *,u64,struct ap_stream_tx_capture *);
struct ap_stream_tx_blocking_owned {
    struct ap_stream_tx_blocking_capture capture;
    u64 records,received;
    bool committed,read;
};
int ap_prepare_original_sendto_blocking(struct ap_session *,int,u64,u64,int,u64,u64,int,u64,u64 *);
int ap_original_sendto_blocking_capture(struct ap_session *,u64,struct ap_stream_tx_blocking_capture *);
#endif
#endif
