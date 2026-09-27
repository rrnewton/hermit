/* SPDX-License-Identifier: MIT */
#ifndef HERMIT_PROVIDER_STREAM_FRONTIER_H
#define HERMIT_PROVIDER_STREAM_FRONTIER_H
#include "stream-copy.h"

/* AUTONOMOUS-BOT-IMPLEMENTED: same existing fd_files incarnation owns this
 * finite transition. TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/3174).
 *
 * Only the authenticated fresh Socket/Accept fd_install entry enrolls zero.
 * Generic lookup/census never initializes this state. The actual receive lock
 * serializes supported begin/end writers; unsupported observers may ONLY OR
 * poison concurrently. No whole-row replacement or poison-clearing store is
 * permitted after fd_files creation. Physical final release removes the row
 * before its address can be reused, through the existing fd_file lifecycle.
 *
 * These counters are native observations, not replay layout or release proof.
 * Complete membership of every consumer remains a separate prerequisite. */
#define AP_STREAM_FRONTIER_ENROLLED 1ULL
#define AP_STREAM_FRONTIER_POISON 2ULL
#define AP_STREAM_FRONTIER_ACTIVE 4ULL
#define AP_STREAM_FRONTIER_OBSERVING 8ULL
/* Inputs must come directly from the same Call's issued frame word. This
 * scalar check neither creates membership nor accepts a transport as a frame. */
static inline int ap_stream_frontier_frame_valid(u64 protocol,u64 stage,u64 transport) {
    return stage==3 && ((transport==AP_STREAM_COPY_TCP && (protocol==1 || protocol==2)) ||
        (transport==AP_STREAM_COPY_UNIX && protocol==3));
}
struct ap_fd_file {
    u64 identity,stream_units;
    u64 stream_state,stream_bytes;
    u64 stream_birth_command,stream_birth_install;
    u64 stream_command,stream_attempt,stream_entry_bytes,stream_entry_order;
    u64 stream_requested,stream_iterator_offset,stream_available;
    struct ap_stream_copy_begin stream_layout;
};
struct ap_stream_frontier_request {
    u64 file,command,attempt,disposition;
    u64 iterator_offset,requested,available;
};
struct ap_stream_frontier_begin {
    u64 before,start,order;
};
struct ap_stream_frontier_end {
    u64 before,after,order_before,order_after;
};
static inline u64 ap_stream_frontier_state(struct ap_fd_file *file) {
    return __sync_val_compare_and_swap(&file->stream_state,0,0);
}
static inline void ap_stream_frontier_poison(struct ap_fd_file *file) {
    if(file)__sync_fetch_and_or(&file->stream_state,AP_STREAM_FRONTIER_POISON);
}
/* The caller supplies a positively authenticated fresh installation and its
 * immutable journal BEGIN. Neither nonzero integers nor this helper alone
 * establish that provenance; fd_install_enter is the sole production issuer. */
static inline int ap_stream_frontier_enroll_install(struct ap_fd_file *file,
        u64 identity,u64 command,u64 install) {
    if(!file)return 0;
    if(!identity || file->identity!=identity || !command || !install ||
       ap_stream_frontier_state(file) || file->stream_units || file->stream_bytes ||
       file->stream_birth_command || file->stream_birth_install ||
       file->stream_command || file->stream_attempt || file->stream_entry_bytes ||
       file->stream_entry_order || file->stream_requested || file->stream_iterator_offset ||
       file->stream_available)goto refused;
    /* Every layout byte belongs to one of these thirteen u64 fields. Compare
     * their values directly: BPF lowers memcmp against zero to byte loads. */
    _Static_assert(sizeof(struct ap_stream_copy_begin)==13*sizeof(u64),"complete layout bytes");
    _Static_assert(__builtin_offsetof(struct ap_stream_copy_begin,disposition)==12*sizeof(u64),
        "final layout word has no trailing or internal padding");
    const struct ap_stream_copy_begin *layout=&file->stream_layout;
    if(layout->file || layout->before || layout->start || layout->order ||
       layout->offset || layout->requested || layout->available || layout->source_offset ||
       layout->skb_length || layout->nonlinear || layout->position || layout->transport ||
       layout->disposition)goto refused;
    file->stream_birth_command=command;file->stream_birth_install=install;
    if(__sync_val_compare_and_swap(&file->stream_state,0,AP_STREAM_FRONTIER_ENROLLED))
        goto refused;
    return 1;
refused:
    ap_stream_frontier_poison(file);return 0;
}
/* Call even for a known file without an admissible command. Silence on that
 * path would allow a later enrolled reader to inherit an invented byte zero. */
#ifdef __BPF__
static __attribute__((noinline)) int ap_stream_frontier_member(
#else
static inline int ap_stream_frontier_member(
#endif
        struct ap_fd_file *file,u64 identity) {
    if(!file)return 0;
    /* Protocol entry can precede acquisition of the native receive lock.
     * Another supported reader's current attempt is not a membership failure.
     * Only Begin, reached after that lock, requires the exact idle state. */
    const u64 state=ap_stream_frontier_state(file);
    if(!identity || file->identity!=identity || !file->stream_birth_command ||
       !file->stream_birth_install ||
       !(state&AP_STREAM_FRONTIER_ENROLLED) ||
       (state&~(AP_STREAM_FRONTIER_ENROLLED|
            AP_STREAM_FRONTIER_ACTIVE|AP_STREAM_FRONTIER_OBSERVING)) ||
       ((state&AP_STREAM_FRONTIER_OBSERVING) &&
        !(state&AP_STREAM_FRONTIER_ACTIVE))) {
        ap_stream_frontier_poison(file);return 0;
    }
    return 1;
}
static inline int ap_stream_frontier_begin_attempt(struct ap_fd_file *file,
        const struct ap_stream_frontier_request *request,
        struct ap_stream_frontier_begin *out) {
    if(!file)return 0;
    if(!request || !out || !request->command || !request->attempt ||
       !request->requested || request->requested>request->available ||
       (request->disposition!=AP_STREAM_COPY_CONSUME &&
        request->disposition!=AP_STREAM_COPY_OBSERVE) ||
       !ap_stream_frontier_member(file,request->file))goto refused;
    /* Consume observes the current OFD byte frontier, never current+Call
     * cursor. Only an authenticated head-PEEK interval adds local traversal. */
    const u64 traversal=request->disposition==AP_STREAM_COPY_OBSERVE
        ? request->iterator_offset : 0;
    const u64 before=file->stream_bytes,order=file->stream_units;
    if(traversal>~0ULL-before || request->available>~0ULL-(before+traversal))
        goto refused;
    const u64 active=AP_STREAM_FRONTIER_ENROLLED|AP_STREAM_FRONTIER_ACTIVE|
        (request->disposition==AP_STREAM_COPY_OBSERVE?AP_STREAM_FRONTIER_OBSERVING:0);
    if(__sync_val_compare_and_swap(&file->stream_state,AP_STREAM_FRONTIER_ENROLLED,active)
        !=AP_STREAM_FRONTIER_ENROLLED)goto refused;
    file->stream_command=request->command;file->stream_attempt=request->attempt;
    file->stream_entry_bytes=before;file->stream_entry_order=order;
    file->stream_requested=request->requested;file->stream_iterator_offset=request->iterator_offset;
    file->stream_available=request->available;
    if(ap_stream_frontier_state(file)!=active)goto refused;
    *out=(struct ap_stream_frontier_begin){.before=before,.start=before+traversal,.order=order};
    return 1;
refused:
    ap_stream_frontier_poison(file);return 0;
}
/* The audited paired outer-copy return occurs under the same receive lock.
 * Successful Consume commits all requested bytes; a fault commits none of this
 * attempt even when earlier stores are visible. Prior successful attempts stay
 * counted. Check BOTH overflows before changing either accepted counter.
 *
 * A concurrent unsupported observer can poison during these stores. The final
 * CAS cannot erase that bit and no successful End is issued on CAS failure.
 * Active fields may be cleared here only because the real receive lock still
 * excludes a next supported Begin until this callback and kernel continuation
 * return; this helper is not a substitute for that lock provenance. */
static inline int ap_stream_frontier_end_attempt(struct ap_fd_file *file,
        const struct ap_stream_frontier_request *request,u64 copied,s64 returned,
        struct ap_stream_frontier_end *out) {
    if(!file)return 0;
    if(!request || !out || !request->file || file->identity!=request->file ||
       !request->command || file->stream_command!=request->command ||
       !request->attempt || file->stream_attempt!=request->attempt ||
       !request->requested || request->requested!=file->stream_requested ||
       request->iterator_offset!=file->stream_iterator_offset || request->available!=file->stream_available ||
       copied>request->requested ||
       (returned!=0 && returned!=-14) ||
       (!returned && copied!=request->requested) ||
       (returned && copied==request->requested) ||
       (request->disposition!=AP_STREAM_COPY_CONSUME &&
        request->disposition!=AP_STREAM_COPY_OBSERVE))goto refused;
    const u64 active=AP_STREAM_FRONTIER_ENROLLED|AP_STREAM_FRONTIER_ACTIVE|
        (request->disposition==AP_STREAM_COPY_OBSERVE?AP_STREAM_FRONTIER_OBSERVING:0);
    const u64 before=file->stream_entry_bytes,order=file->stream_entry_order;
    if(ap_stream_frontier_state(file)!=active || file->stream_bytes!=before ||
       file->stream_units!=order)goto refused;
    const int consume=!returned && request->disposition==AP_STREAM_COPY_CONSUME;
    if(consume && (copied>~0ULL-before || order==~0ULL))goto refused;
    const struct ap_stream_frontier_end completed={.before=before,
        .after=consume?before+copied:before,.order_before=order,.order_after=consume?order+1:order};
    file->stream_bytes=completed.after;file->stream_units=completed.order_after;
    file->stream_command=0;file->stream_attempt=0;
    file->stream_entry_bytes=0;file->stream_entry_order=0;
    file->stream_requested=0;file->stream_iterator_offset=0;file->stream_available=0;
    __builtin_memset(&file->stream_layout,0,sizeof(file->stream_layout));
    if(__sync_val_compare_and_swap(&file->stream_state,active,AP_STREAM_FRONTIER_ENROLLED)
        !=active)goto refused;
    *out=completed;return 1;
refused:
    ap_stream_frontier_poison(file);return 0;
}
#endif
