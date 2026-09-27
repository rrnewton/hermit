/* SPDX-License-Identifier: MIT */
#ifndef HERMIT_PROVIDER_STREAM_COPY_H
#define HERMIT_PROVIDER_STREAM_COPY_H
#include "provider.h"
#ifdef AP_FTRACE_PROVIDER
#include "ftrace-coverage.h"
#endif

/* One original Call owns these observations through its existing ACK. Copy
 * offsets are iterator positions, not append offsets: a kernel revert followed
 * by a retry can overwrite an earlier observation at the same position. */
#define AP_STREAM_COPY_VERSION 4ULL
#define AP_STREAM_COPY_VERSION_FRONTIER 5ULL
/* Packaging supplies this exact contract to BOTH object and adapter. Historical
 * standalone controls retain ABI7/copy4; a record never chooses its grammar. */
#ifndef AP_NATIVE_COPY_VERSION
#define AP_NATIVE_COPY_VERSION AP_STREAM_COPY_VERSION
#endif
#if AP_NATIVE_COPY_VERSION != 4 && AP_NATIVE_COPY_VERSION != 5
#error "unsupported native receive copy contract"
#endif
#define AP_STREAM_COPY_BYTES 512U
#define AP_STREAM_COPY_RING_BYTES (1U << 20)
#define AP_STREAM_COPY_DATA 1U
#define AP_STREAM_COPY_COMMIT 2U
#define AP_STREAM_COPY_UNIT 3U
#define AP_STREAM_COPY_BEGIN 4U
#define AP_STREAM_COPY_END 5U
#define AP_STREAM_COPY_TCP 1ULL
#define AP_STREAM_COPY_UNIX 2ULL
#define AP_STREAM_COPY_CONSUME 1ULL
#define AP_STREAM_COPY_OBSERVE 2ULL
#define AP_STREAM_COPY_ENTRY_COOKIE 5ULL
#define AP_STREAM_COPY_EXIT_COOKIE 6ULL
#define AP_STREAM_COPY_FRAG_ENTRY_COOKIE 7ULL
#define AP_STREAM_COPY_FRAG_EXIT_COOKIE 8ULL
/* Keep the compatibility roles on the production source-selection branch.
 * A role mutant therefore removes a real linear or fragment observation,
 * rather than changing only a test-side coverage bitmap. */
static inline int ap_stream_copy_source_roles(u32 nonlinear) {
#ifdef AP_FTRACE_PROVIDER
    return nonlinear?(ap_ftrace_role_enabled(14) && ap_ftrace_role_enabled(15)):
        (ap_ftrace_role_enabled(12) && ap_ftrace_role_enabled(13));
#else
    (void)nonlinear;return 1;
#endif
}
/* A nonlinear skb can still satisfy some or all of one copy from its linear
 * head. Return only that first bounded segment; the caller continues from the
 * resulting absolute skb offset through authenticated fragments. */
static inline u64 ap_stream_copy_linear_take(u64 source,u64 requested,u64 linear) {
    if(!requested || source>=linear)return 0;
    const u64 available=linear-source;
    return requested<available?requested:available;
}
/* A mixed skb's linear head is stable only with one full head/data owner.
 * Payload-only references do not qualify, and a page-backed head needs a
 * separate page ownership certificate. Fragment custody is checked apart. */
static inline int ap_stream_copy_mixed_head_owned(u64 source,u64 requested,
        u64 size,u64 nonlinear,s32 dataref,u64 head_frag) {
    return requested && nonlinear && nonlinear<=size && source<size-nonlinear &&
        requested<=size-source && dataref==1 && !head_frag;
}
/* The accepted image is x86-64 with 4 KiB pages and 64-byte vmemmap entries.
 * This reconstructs the same page-backed skb_frag address arithmetic used by
 * its audited __skb_datagram_iter body. A compound-page fragment may cross
 * base-page boundaries; the x86 direct map remains contiguous, while the
 * caller separately proves the requested range remains inside frag->len.
 * This establishes only an address: the separate users/dataref/flags/
 * no-frag-list predicate still owns custody. */
#define AP_STREAM_COPY_PAGE_SHIFT 12U
#define AP_STREAM_COPY_PAGE_BYTES (1ULL<<AP_STREAM_COPY_PAGE_SHIFT)
#define AP_STREAM_COPY_PAGE_STRUCT_SHIFT 6U
/* This exact admitted x86 image runs with five-level paging.  Its vmemmap and
 * direct map are in the sign-extended 57-bit kernel half (0xff...), while
 * text remains in the narrower 48-bit region (0xffff...).  Reject user and
 * non-canonical hole addresses without incorrectly excluding the two live
 * bases authenticated during the grouped-anchor handshake. */
#define AP_STREAM_COPY_KERNEL_MIN 0xff00000000000000ULL
/* The address translation is admitted only for the exact page geometry used
 * to derive it. The userspace loader supplies live sysconf/BTF observations;
 * a build-ID match alone never substitutes for either explicit dimension. */
#ifndef __BPF__
static inline int ap_stream_fragment_image_layout(u64 page_size,u64 page_struct_size) {
    return page_size==AP_STREAM_COPY_PAGE_BYTES &&
        page_struct_size==(1ULL<<AP_STREAM_COPY_PAGE_STRUCT_SHIFT);
}
#endif
static inline u64 ap_stream_fragment_source(u64 netmem,u64 fragment_offset,u64 fragment_length,
        u64 vmemmap_base,u64 page_offset_base) {
    if(!netmem || (netmem&1) || !fragment_length ||
       fragment_offset>0xffffffffULL || fragment_length>0xffffffffULL-fragment_offset ||
       vmemmap_base<AP_STREAM_COPY_KERNEL_MIN || (vmemmap_base&((1ULL<<AP_STREAM_COPY_PAGE_STRUCT_SHIFT)-1)) ||
       page_offset_base<AP_STREAM_COPY_KERNEL_MIN || (page_offset_base&(AP_STREAM_COPY_PAGE_BYTES-1)))return 0;
    const u64 page_step=(fragment_offset>>AP_STREAM_COPY_PAGE_SHIFT)<<AP_STREAM_COPY_PAGE_STRUCT_SHIFT;
    if(netmem>~0ULL-page_step)return 0;
    const u64 page=netmem+page_step;
    if(page<vmemmap_base)return 0;
    const u64 page_delta=page-vmemmap_base;
    if((page_delta&((1ULL<<AP_STREAM_COPY_PAGE_STRUCT_SHIFT)-1)) ||
       page_delta>(~0ULL>>AP_STREAM_COPY_PAGE_STRUCT_SHIFT))return 0;
    const u64 direct_delta=page_delta<<AP_STREAM_COPY_PAGE_STRUCT_SHIFT;
    if(page_offset_base>~0ULL-direct_delta)return 0;
    const u64 direct_page=page_offset_base+direct_delta;
    const u64 within=fragment_offset&(AP_STREAM_COPY_PAGE_BYTES-1);
    if(direct_page>~0ULL-within)return 0;
    const u64 source=direct_page+within;
    if(source<AP_STREAM_COPY_KERNEL_MIN || source>~0ULL-(fragment_length-1))return 0;
    return source;
}
/* Audited installed image: both devirtualized calls are five-byte direct
 * calls to _copy_to_iter. Successors observe RAX before any revert. */
#define AP_STREAM_COPY_SYMBOL "__skb_datagram_iter"
#define AP_STREAM_COPY_ENTRY_OFFSET 0x64ULL
#define AP_STREAM_COPY_EXIT_OFFSET 0x69ULL
#define AP_STREAM_COPY_FRAG_ENTRY_OFFSET 0x26bULL
#define AP_STREAM_COPY_FRAG_EXIT_OFFSET 0x270ULL
/* Private receive-frame state shares the otherwise unused security_socket
 * word for operations11/21/22 only. Zero word is idle. A nonzero aligned word
 * with tag0 is the witnessed Unix path, never an uninitialized Call. */
#define AP_STREAM_DISPATCH4_COOKIE 32ULL
#define AP_STREAM_DISPATCH6_COOKIE 33ULL
#define AP_STREAM_DISPATCH_OFFSET 0x1bULL
static inline int ap_stream_frame_stack_valid(u64 stack) {
    return stack>=56 && !(stack&7) && stack<=~0ULL-216;
}
static inline u64 ap_stream_frame_begin(u64 protocol,u64 stack) {
    return protocol>=1 && protocol<=3 && ap_stream_frame_stack_valid(stack)?stack|protocol:0;
}
#ifdef __BPF__
static __attribute__((noinline)) u64 ap_stream_frame_protocol(
#else
static inline u64 ap_stream_frame_protocol(
#endif
u64 word) {
    if(!ap_stream_frame_stack_valid(word&~7ULL))return 0;
    const u64 tag=word&7;
    if(tag==0 || tag==3)return 3;
    return tag==1 || tag==4 || tag==6?1:2;
}
#ifdef __BPF__
static __attribute__((noinline)) u64 ap_stream_frame_stage(
#else
static inline u64 ap_stream_frame_stage(
#endif
u64 word) {
    if(!ap_stream_frame_protocol(word))return 0;
    const u64 tag=word&7;
    return tag>=1 && tag<=3?1:tag==4 || tag==5?2:3;
}
#ifdef __BPF__
static __attribute__((noinline)) u64 ap_stream_frame_advance(
#else
static inline u64 ap_stream_frame_advance(
#endif
u64 word,u64 protocol,u64 stage) {
    if(ap_stream_frame_protocol(word)!=protocol)return 0;
    const u64 before=ap_stream_frame_stage(word),stack=word&~7ULL;
    if(protocol<=2 && before==1 && stage==2)return stack|(protocol+3);
    if(protocol<=2 && before==2 && stage==3)return stack|(protocol+5);
    if(protocol==3 && before==1 && stage==3)return stack;
    return 0;
}
static inline u64 ap_stream_tcp_target(u64 protocol,u64 function) {
    const u64 delta=protocol==1?0x39ea70ULL:protocol==2?0x9b150ULL:0;
    return delta && function>delta?function-delta:0;
}
static inline int ap_stream_dispatch_site(u64 word,u64 function,u64 cookie,u64 ip,u64 stack,u64 target) {
    const u64 protocol=ap_stream_frame_protocol(word);
    return (protocol==1 || protocol==2) && ap_stream_frame_stage(word)==1 &&
        cookie==(protocol==1?AP_STREAM_DISPATCH4_COOKIE:AP_STREAM_DISPATCH6_COOKIE) &&
        function && function<=~0ULL-AP_STREAM_DISPATCH_OFFSET-1 &&
        ip==function+AP_STREAM_DISPATCH_OFFSET+1 && stack==(word&~7ULL) &&
        target && target==ap_stream_tcp_target(protocol,function);
}
static inline int ap_stream_copy_classic_site(u64 word,u64 function,u64 cookie,u64 ip) {
    const u64 protocol=ap_stream_frame_protocol(word);
    const u64 delta=protocol==1?0x39d450ULL:protocol==2?0x99b30ULL:protocol==3?0x1a17a0ULL:0;
    if(ap_stream_frame_stage(word)!=3 || !delta || function<=delta)return 0;
    const u64 base=function-delta;
    u64 offset=0;
    if(cookie==AP_STREAM_COPY_ENTRY_COOKIE)offset=AP_STREAM_COPY_ENTRY_OFFSET;
    else if(cookie==AP_STREAM_COPY_EXIT_COOKIE)offset=AP_STREAM_COPY_EXIT_OFFSET;
    else if(cookie==AP_STREAM_COPY_FRAG_ENTRY_COOKIE)offset=AP_STREAM_COPY_FRAG_ENTRY_OFFSET;
    else if(cookie==AP_STREAM_COPY_FRAG_EXIT_COOKIE)offset=AP_STREAM_COPY_FRAG_EXIT_OFFSET;
    else return 0;
    return base<=~0ULL-offset-1 && ip==base+offset+1;
}
/* A successor reached through an indirect/alternative branch has no matching
 * direct-call entry. It is a protocol failure, never an empty observation. */
static inline int ap_stream_copy_pair_matches(u64 phase,u64 cookie,u64 entry,u64 returned) {
    return (cookie==AP_STREAM_COPY_EXIT_COOKIE || cookie==AP_STREAM_COPY_FRAG_EXIT_COOKIE) &&
        phase==(cookie==AP_STREAM_COPY_EXIT_COOKIE?2ULL:3ULL) &&
        entry && entry<=~0ULL-5 && returned==entry+5;
}
/* All three supported socket protocol BTF returns are four-byte signed int.
 * Upper RAX bits are outside that ABI; actual copy length is separately u64. */
static inline s64 ap_stream_copy_protocol_return(u64 register_value) {
    return (s32)register_value;
}
/* recvmsg always substitutes its own sockaddr scratch, even when the actual
 * imported user header requests no address. Authenticate that exact wrapper
 * invocation and its saved imported operands; a nonnull name alone is not
 * permission. The image check binds the full wrapper and import bodies. */
#define AP_STREAM_RECVMSG_NAME_SLOT 16ULL
#define AP_STREAM_RECVMSG_CONTROL_SLOT 24ULL
#define AP_STREAM_RECVMSG_SCRATCH_SLOT 32ULL
#define AP_STREAM_RECVMSG_HEADER_SLOT 192ULL
#define AP_STREAM_RECVMSG_OUTER_SLOT 208ULL
#define AP_STREAM_RECVMSG_FRAME_BYTES 216ULL
struct ap_stream_recvmsg_frame {
    u64 stack,returned,header,name,user_name,user_control,outer_return;
};
struct ap_stream_message_view {
    u64 name,control,length;
    u32 flags;
    s32 name_length;
};
static inline int ap_stream_message_plain(const struct ap_stream_message_view *message,
        u64 expected_name,int unix_name_completed) {
    /* Only the paired Unix return may have filled the authenticated internal
     * sockaddr. Its imported user-name pointer was NULL, so the bound wrapper
     * does not copy this address to userspace. Entry and all other paths keep
     * the original zero-length requirement. */
    return message && message->name==expected_name &&
        (unix_name_completed ? expected_name && message->name_length>=0 && message->name_length<=128
                             : !message->name_length) &&
        !message->control && !message->length && !message->flags;
}
static inline int ap_stream_recvmsg_frame_matches(u64 protocol,u64 function,
        u64 header,const struct ap_stream_recvmsg_frame *frame) {
    if(!frame || !function || !header || frame->header!=header ||
       !frame->stack || (frame->stack&7) ||
       frame->stack>~0ULL-AP_STREAM_RECVMSG_FRAME_BYTES ||
       frame->returned<=0x1d2aULL || frame->outer_return!=frame->returned-0x1d2aULL ||
       frame->name!=frame->stack+AP_STREAM_RECVMSG_SCRATCH_SLOT ||
       frame->user_name || frame->user_control)return 0;
    if(protocol==1)return function>0x2e9fe8ULL && frame->returned==function-0x2e9fe8ULL;
    if(protocol==2)return function<=~0ULL-0x19938ULL && frame->returned==function+0x19938ULL;
    return protocol==3 && function>0xee338ULL && frame->returned==function-0xee338ULL;
}
/* kprobe_multi can receive partial ftrace registers: R14 is unavailable.
 * The exact ____sys_recvmsg prologue saves its caller's R15 at SP+192 before
 * using R15 for the kernel message. The bound ___sys_recvmsg body preserves
 * the imported user-header pointer there, and its exact call return at SP+208
 * authenticates that provenance. No userspace reread supplies these operands. */
static __attribute__((always_inline)) inline u64 ap_stream_recvmsg_scratch(
        u64 protocol,u64 function,u64 header,u64 stack,const void *name_slot,
        long (*read_kernel)(void *,u32,const void *)) {
    struct ap_stream_recvmsg_frame frame={.stack=stack};
    if(!read_kernel || !name_slot || !stack || (stack&7) ||
       stack>~0ULL-AP_STREAM_RECVMSG_FRAME_BYTES ||
       read_kernel(&frame.returned,sizeof(frame.returned),(const void *)stack) ||
       read_kernel(&frame.user_name,sizeof(frame.user_name),
            (const void *)(stack+AP_STREAM_RECVMSG_NAME_SLOT)) ||
       read_kernel(&frame.user_control,sizeof(frame.user_control),
            (const void *)(stack+AP_STREAM_RECVMSG_CONTROL_SLOT)) ||
       read_kernel(&frame.header,sizeof(frame.header),
            (const void *)(stack+AP_STREAM_RECVMSG_HEADER_SLOT)) ||
       read_kernel(&frame.outer_return,sizeof(frame.outer_return),
            (const void *)(stack+AP_STREAM_RECVMSG_OUTER_SLOT)) ||
       read_kernel(&frame.name,sizeof(frame.name),name_slot) ||
       !ap_stream_recvmsg_frame_matches(protocol,function,header,&frame))return 0;
    return frame.name;
}
/* Necessary part of the protocol-specific TCP certificate, not a complete
 * custody predicate. Every current zerocopy/shared flag and unknown bit fails. */
static inline int ap_stream_copy_tcp_storage_flags(u32 flags) {return flags==0;}
/* Relative text coordinates on the package's already-bound installed image.
 * They validate the actual scalar receive caller, not a symbol-name guess.
 * Both receive locks remain held through the successful native commit. */
#define AP_STREAM_COPY_TCP_CALLER_DELTA 0xb5b49ULL
#define AP_STREAM_COPY_UNIX_CALLER_DELTA 0x1a26caULL
#define AP_STREAM_COPY_TCP_INLINE_CALLER_BACK 0x10adULL
/* The exact installed tcp_recvmsg also inlines tcp_recvmsg_locked. Both
 * audited copy successors precede copied_seq advancement under the same
 * socket lock. The Unix actor remains under its caller's iolock. */
static inline int ap_stream_copy_unit_caller(u64 transport,u64 function,u64 returned) {
    if(!function || !returned)return 0;
    if(transport==AP_STREAM_COPY_TCP)
        return (function>AP_STREAM_COPY_TCP_INLINE_CALLER_BACK &&
                returned==function-AP_STREAM_COPY_TCP_INLINE_CALLER_BACK) ||
            (function<=~0ULL-AP_STREAM_COPY_TCP_CALLER_DELTA &&
             returned==function+AP_STREAM_COPY_TCP_CALLER_DELTA);
    return transport==AP_STREAM_COPY_UNIX && function<=~0ULL-AP_STREAM_COPY_UNIX_CALLER_DELTA &&
        returned==function+AP_STREAM_COPY_UNIX_CALLER_DELTA;
}

/* Version4: successful Consume advances the native consumed frontier.
 * Observe (PEEK) and failures retain that frontier without consuming. The
 * Call-local iterator advances on either successful disposition; these
 * physical observations do not establish a causal release cut. */
struct ap_stream_copy_unit {
    u64 file,order,offset,requested,copied;
    s64 returned;
    u64 position,transport,disposition;
};
/* Version5 uses DISTINCT Begin and End kinds. Unit72/kind3 and the
 * historical DATA1/COMMIT2 meanings remain exact version4. Full available
 * extent is independent of requested and observed bytes; no permanent skb
 * identity, immutable unseen payload or replay rollback unit is asserted. */
struct ap_stream_copy_begin {
    u64 file,before,start,order,offset,requested,available;
    u64 source_offset,skb_length,nonlinear,position,transport,disposition;
};
struct ap_stream_copy_end {
    struct ap_stream_copy_unit unit;
    u64 before,after;
};
_Static_assert(sizeof(struct ap_stream_copy_unit)==72,"historical copy4 Unit ABI");
_Static_assert(sizeof(struct ap_stream_copy_begin)==104,"copy5 Begin ABI");
_Static_assert(sizeof(struct ap_stream_copy_end)==88,"copy5 End ABI");
/* Shared scalar wire checks. The caller has ALREADY selected grammar5
 * from the immutable adapter contract and joined the command identity. */
static inline int ap_stream_copy_begin_valid(const struct ap_stream_copy_begin *begin,
        u64 maximum,u64 cursor,u64 disposition) {
    if(!begin || !begin->file || !begin->requested || begin->offset!=cursor ||
       cursor>maximum || begin->requested>maximum-cursor ||
       !begin->available || begin->requested>begin->available ||
       begin->source_offset>begin->skb_length ||
       begin->available!=begin->skb_length-begin->source_offset ||
       begin->skb_length>0xffffffffULL || begin->nonlinear>begin->skb_length ||
       begin->position>0xffffffffULL ||
       (begin->transport!=AP_STREAM_COPY_TCP && begin->transport!=AP_STREAM_COPY_UNIX) ||
       (begin->transport==AP_STREAM_COPY_UNIX &&
        (begin->nonlinear || begin->position!=begin->source_offset)) ||
       (begin->nonlinear && begin->source_offset<begin->skb_length-begin->nonlinear) ||
       begin->disposition!=disposition ||
       (disposition!=AP_STREAM_COPY_CONSUME && disposition!=AP_STREAM_COPY_OBSERVE))return 0;
    const u64 traversal=disposition==AP_STREAM_COPY_OBSERVE?cursor:0;
    return traversal<=~0ULL-begin->before && begin->start==begin->before+traversal &&
        begin->available<=~0ULL-begin->start;
}
static inline int ap_stream_copy_end_valid(const struct ap_stream_copy_begin *begin,
        const struct ap_stream_copy_end *end,u64 copied) {
    if(!begin || !end)return 0;
    const struct ap_stream_copy_unit *unit=&end->unit;
    if(unit->file!=begin->file || unit->offset!=begin->offset ||
       unit->requested!=begin->requested || unit->position!=begin->position ||
       unit->transport!=begin->transport || unit->disposition!=begin->disposition ||
       unit->copied!=copied || copied>begin->requested || end->before!=begin->before ||
       (unit->returned!=0 && unit->returned!=-14) ||
       (!unit->returned && copied!=begin->requested) ||
       (unit->returned && copied==begin->requested))return 0;
    const int consumes=!unit->returned && unit->disposition==AP_STREAM_COPY_CONSUME;
    if(consumes && (copied>~0ULL-begin->before || begin->order==~0ULL))return 0;
    return unit->order==begin->order+(consumes?1:0) &&
        end->after==begin->before+(consumes?copied:0);
}
struct ap_stream_copy_record {
    u64 provider,command,call,task,task_start;
    u64 sequence,attempt,offset;
    u32 length,kind;
    u8 bytes[AP_STREAM_COPY_BYTES];
};
struct ap_stream_copy_summary {
    u64 version,initial_count,attempts,records,copied;
    u64 final_count,protocol_returned,protocol_complete;
};
struct ap_stream_copy_manifest {
    u64 provider,command,call,task,task_start,present;
    s64 returned;
    struct ap_stream_copy_summary summary;
};
struct ap_stream_copy_progress { u64 records,exited,terminal,protocol; };
/* Private borrowed pointers are live only between the actual paired kernel
 * callbacks. They never appear in a userspace payload receipt. */
struct ap_stream_copy_state {
    union {
        struct ap_stream_copy_summary summary;
        /* The protocol result does not exist until its return. While one
         * _copy_to_iter is active, these three words retain its operands and entry PC.
         * They are cleared at its paired return before the summary is sealed. */
        struct { u64 prefix[5],source,count,complete; } callback;
    };
    u64 iterator,protocol_ip;
    u64 source,requested,before_count,copy_active;
    u64 skb,copied;
};
_Static_assert(__builtin_offsetof(struct ap_stream_copy_state,callback.source)==
    __builtin_offsetof(struct ap_stream_copy_state,summary.final_count) &&
    __builtin_offsetof(struct ap_stream_copy_state,callback.count)==
    __builtin_offsetof(struct ap_stream_copy_state,summary.protocol_returned) &&
    __builtin_offsetof(struct ap_stream_copy_state,callback.complete)==
    __builtin_offsetof(struct ap_stream_copy_state,summary.protocol_complete),
    "Copy operand storage must alias only the not-yet-published protocol result");
_Static_assert(sizeof(struct ap_stream_copy_state)<=128,
    "Read copy context must fit the existing per-operation private union");
#ifndef __BPF__
#include <stdbool.h>
#include <stddef.h>
#include <linux/bpf.h>
#include <string.h>
static inline int ap_stream_copy_link_matches(unsigned which,
        const struct bpf_prog_info *p,u32 ps,const struct bpf_link_info *l,u32 ls,
        const char *symbol) {
    const u64 offsets[]={AP_STREAM_COPY_ENTRY_OFFSET,AP_STREAM_COPY_EXIT_OFFSET,
        AP_STREAM_COPY_FRAG_ENTRY_OFFSET,AP_STREAM_COPY_FRAG_EXIT_OFFSET};
    const u64 cookies[]={AP_STREAM_COPY_ENTRY_COOKIE,AP_STREAM_COPY_EXIT_COOKIE,
        AP_STREAM_COPY_FRAG_ENTRY_COOKIE,AP_STREAM_COPY_FRAG_EXIT_COOKIE};
    return which<4 && p && l && symbol &&
        ps>=offsetof(struct bpf_prog_info,recursion_misses)+sizeof(p->recursion_misses) &&
        ls>=offsetof(struct bpf_link_info,perf_event.kprobe.missed)+sizeof(l->perf_event.kprobe.missed) &&
        p->type==BPF_PROG_TYPE_KPROBE && p->id && !p->recursion_misses &&
        l->type==BPF_LINK_TYPE_PERF_EVENT && l->id && l->prog_id==p->id &&
        l->perf_event.type==BPF_PERF_EVENT_KPROBE &&
        l->perf_event.kprobe.name_len==sizeof(AP_STREAM_COPY_SYMBOL) && !strcmp(symbol,AP_STREAM_COPY_SYMBOL) &&
        l->perf_event.kprobe.offset==offsets[which] && l->perf_event.kprobe.cookie==cookies[which] &&
        !l->perf_event.kprobe.missed;
}
struct ap_stream_copy_owned {
    struct ap_stream_copy_record *records;
    struct ap_stream_copy_summary summary;
    u64 task,task_start,copied,last_attempt,delivered;
    u64 unit_bytes,unit_cursor,unit_file,unit_order,completed_units,unit_visible;
    s64 returned;
    u64 terminal_cut;
    struct ap_stream_copy_begin current_begin;
    u64 byte_frontier;
    bool begin_active,frontier_seen;
    size_t count,capacity;
    bool committed,manifest_read,terminal_cut_set,terminal_drained;
};
struct ap_session;
int ap_original_copy_poll_fd(struct ap_session *);
int ap_drain_original_copy(struct ap_session *);
int ap_original_read_copy_ready(struct ap_session *,int,u64,u32);
int ap_original_read_copy_manifest(struct ap_session *,u64,struct ap_stream_copy_manifest *);
int ap_original_read_copy_record(struct ap_session *,u64,u64,struct ap_stream_copy_record *);
int ap_original_read_copy_progress(struct ap_session *,int,u64,struct ap_stream_copy_progress *);
#endif
#endif
