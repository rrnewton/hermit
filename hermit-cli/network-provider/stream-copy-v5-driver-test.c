/* SPDX-License-Identifier: MIT */
#define AP_NATIVE_COPY_VERSION 5
#define AP_GROUPED_PROVIDER 1
/* Additive V5 controls of the SAME retained-copy callback. The original
 * stream-copy-driver-test.c runs separately under exact default ABI7/copy4.
 * These test stubs are unchanged boundary scaffolding, not native evidence. */
#include <assert.h>
#include <errno.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "fd-effects.h"

/* Exact host execution of stream_copy_emit_grouped_source.  Kernel pointer
 * reads are translated onto three retained page images, while the included
 * producer body, its fragment bounds and its role gates are production code. */
#define AP_STREAM_COPY_TRANSPORT_SHIFT 8
#define TEST_FRAGMENTS 4
#define TEST_VMEMMAP 0xffd4000000000000ULL
#define TEST_DIRECT 0xff11000000000000ULL
struct stream_copy_skb_view {
    u64 head,data;
    u32 tail,end,size,nonlinear;
    s32 users;
};
struct stream_copy_shared_view { u64 frag_list; s32 dataref; u8 count,flags; };
struct test_frag { u64 netmem; u32 len,offset; };
struct skb_shared_info {
    struct test_frag frags[TEST_FRAGMENTS];
    u64 frag_list;
    s32 dataref;
    u8 nr_frags,flags;
};
struct sk_buff {
    struct stream_copy_skb_view view;
    struct skb_shared_info shared;
};
static struct ap_config source_config={
    .anchor_phase=AP_GROUPED_ANCHOR_ACTIVE,
    .vmemmap_base=TEST_VMEMMAP,
    .page_offset_base=TEST_DIRECT,
};
static int ap_config_map;
static unsigned char direct_pages[3][AP_STREAM_COPY_PAGE_BYTES];
static unsigned char emitted_bytes[64];
struct emitted_segment { u64 source,count,offset; };
static struct emitted_segment emitted_segments[8];
static u32 emitted_segment_count;
static void *lookup(void *map,const u32 *key) {
    assert(map==&ap_config_map && key && *key==0);return &source_config;
}
static int fd_read_kernel(void *out,u32 count,const void *source) {
    const u64 address=(u64)source;
    if(address>=TEST_DIRECT && address<TEST_DIRECT+sizeof(direct_pages)) {
        const u64 offset=address-TEST_DIRECT;
        if(count>sizeof(direct_pages)-offset)return -1;
        memcpy(out,(const unsigned char *)direct_pages+offset,count);return 0;
    }
    memcpy(out,source,count);return 0;
}
#define CORE(address) (address)
static int stream_copy_read_skb(struct sk_buff *skb,struct stream_copy_skb_view *out) {
    if(!skb)return 0;*out=skb->view;return 1;
}
static int stream_copy_read_shared(struct skb_shared_info *shared,
        struct stream_copy_shared_view *out) {
    if(!shared)return 0;
    *out=(struct stream_copy_shared_view){.frag_list=shared->frag_list,
        .dataref=shared->dataref,.count=shared->nr_frags,.flags=shared->flags};
    return 1;
}
static int stream_copy_emit_range(struct ap_fd_call *call,u64 source,u64 count,u64 offset) {
    assert(call && count && emitted_segment_count<8 && offset+count<=sizeof(emitted_bytes));
    emitted_segments[emitted_segment_count++]=(struct emitted_segment){source,count,offset};
    return !fd_read_kernel(emitted_bytes+offset,count,(const void *)source);
}
#include "stream-copy-grouped-source.inc"
#undef CORE
enum ap_slot_state { AP_SLOT_FREE, AP_SLOT_RESERVED, AP_SLOT_ACTIVE, AP_SLOT_DISARMING, AP_SLOT_COLLECTED, AP_SLOT_QUARANTINED };
struct ap_pending_command {
    enum ap_slot_state state;
    struct ap_task_command submitted;
    struct ap_command_result receipt;
    bool original_collected;
    struct ap_stream_copy_owned stream_copy;
    union { struct ap_stream_tx_owned stream_tx;
        struct ap_stream_tx_blocking_owned stream_tx_blocking; };
    struct ap_fd_call original_receipt;
};
struct ring { unsigned long consumer,producer; };
struct ring_buffer { unsigned consume_calls;struct ring ring; };
struct ap_session {
    u64 incarnation;bool ready;
    struct ap_pending_command pending[AP_COMMANDS];
    struct ring_buffer *stream_copy_ring;
    int stream_copy_error;
};
static int invalid(void) {errno=EINVAL;return -1;}
static int unavailable(void) {errno=ENODATA;return -1;}
static int enter_commands(struct ap_session *s) {return s && s->ready?0:invalid();}
static void leave_commands(struct ap_session *s) {(void)s;}
static bool terminal;
static int fd_thread_exited(int pidfd,bool *out) {assert(pidfd==91);*out=terminal;return 0;}
static int fd_require_dead_thread(int pidfd) {assert(pidfd==91);if(!terminal){errno=EAGAIN;return -1;}return 0;}
static struct ring *ring_buffer__ring(struct ring_buffer *r,unsigned int index) {assert(r && !index);return &r->ring;}
static unsigned long ring__consumer_pos(const struct ring *r) {return r->consumer;}
static unsigned long ring__producer_pos(const struct ring *r) {return r->producer;}
static int ring_buffer__consume(struct ring_buffer *ring) {ring->consume_calls++;return 0;}
static int ring_buffer__epoll_fd(const struct ring_buffer *ring) {assert(ring);return 71;}
int ap_read_fd_status(struct ap_session *s,struct ap_fd_status *out) {
    assert(s && s->ready);*out=(struct ap_fd_status){0};return 0;
}
static int stream_copy_observer_ready(struct ap_session *s) {assert(s && s->ready);return 0;}
#include "stream-copy-driver.h"
static void reset(struct ap_session *s,struct ring_buffer *ring) {
    for(unsigned i=0;i<AP_COMMANDS;i++)free(s->pending[i].stream_copy.records);
    memset(s,0,sizeof(*s));memset(ring,0,sizeof(*ring));terminal=false;
    s->incarnation=3;s->ready=true;s->stream_copy_ring=ring;
    struct ap_pending_command *p=&s->pending[ap_command_slot(7)];
    p->state=AP_SLOT_ACTIVE;p->submitted=(struct ap_task_command){.command=7,
        .operation=AP_ORIGINAL_READ,.expected_object=11,.original_count=64};
}
static struct ap_stream_copy_record raw(struct ap_session *s,u32 kind,u32 length) {
    struct ap_stream_copy_owned *copy=&s->pending[ap_command_slot(7)].stream_copy;
    return (struct ap_stream_copy_record){.provider=3,.command=7,.call=11,.task=13,.task_start=17,
        .sequence=copy->count+1,.attempt=copy->completed_units+1,.offset=copy->unit_cursor,
        .length=length,.kind=kind};
}
static struct ap_stream_copy_record begin(struct ap_session *s,u64 bytes,u64 order,u64 requested,u64 available,u64 disposition) {
    struct ap_stream_copy_record record=raw(s,AP_STREAM_COPY_BEGIN,sizeof(struct ap_stream_copy_begin));
    const struct ap_stream_copy_begin value={.file=23,.before=bytes,
        .start=bytes+(disposition==AP_STREAM_COPY_OBSERVE?record.offset:0),.order=order,.offset=record.offset,
        .requested=requested,.available=available,.skb_length=available,.position=42,
        .transport=AP_STREAM_COPY_TCP,.disposition=disposition};
    memcpy(record.bytes,&value,sizeof(value));return record;
}
static struct ap_stream_copy_record finish(struct ap_session *s,s64 returned) {
    struct ap_stream_copy_owned *copy=&s->pending[ap_command_slot(7)].stream_copy;
    const struct ap_stream_copy_begin *b=&copy->current_begin;
    const int consume=!returned && b->disposition==AP_STREAM_COPY_CONSUME;
    const struct ap_stream_copy_end end={.unit={.file=b->file,.order=b->order+(consume?1:0),
        .offset=b->offset,.requested=b->requested,.copied=copy->unit_bytes,.returned=returned,
        .position=b->position,.transport=b->transport,.disposition=b->disposition},
        .before=b->before,.after=b->before+(consume?copy->unit_bytes:0)};
    struct ap_stream_copy_record record=raw(s,AP_STREAM_COPY_END,sizeof(end));
    memcpy(record.bytes,&end,sizeof(end));return record;
}
static void accept(struct ap_session *s,const struct ap_stream_copy_record *r) {
    assert(!stream_copy_record(s,(void *)r,sizeof(*r)));assert(!s->stream_copy_error);
}
static void payload(struct ap_session *s,u32 length) {
    struct ap_stream_copy_record r=raw(s,AP_STREAM_COPY_DATA,length);
    memset(r.bytes,'Z',length);accept(s,&r);
}
static void commit5(struct ap_session *s,s64 returned) {
    struct ap_pending_command *p=&s->pending[ap_command_slot(7)];
    struct ap_stream_copy_owned *copy=&p->stream_copy;
    const struct ap_stream_copy_summary summary={.version=5,.initial_count=64,.attempts=copy->completed_units,
        .records=copy->count,.copied=copy->copied,.final_count=64-(returned>0?returned:0),
        .protocol_returned=(u64)returned,.protocol_complete=1};
    struct ap_stream_copy_record r=raw(s,AP_STREAM_COPY_COMMIT,sizeof(summary));
    r.attempt=copy->completed_units;r.offset=(u64)returned;
    memcpy(r.bytes,&summary,sizeof(summary));accept(s,&r);
    p->state=AP_SLOT_COLLECTED;p->original_collected=true;
    p->receipt=(struct ap_command_result){.command=7,.task=13,.start_boottime=17,.returned=returned};
    struct ap_stream_copy_manifest manifest={0};
    assert(!ap_original_read_copy_manifest(s,7,&manifest));
    assert(manifest.present==1 && manifest.summary.version==5 && manifest.returned==returned);
    for(u64 i=0;i<copy->count;i++) {
        struct ap_stream_copy_record readback={0};assert(!ap_original_read_copy_record(s,7,i,&readback));
        assert(!memcmp(&readback,&copy->records[i],sizeof(readback)));
    }
    assert(copy->delivered==copy->count);
}
static void active_prefix_and_interleave(struct ap_session *s,struct ring_buffer *ring) {
    reset(s,ring);struct ap_stream_copy_record r=begin(s,0,0,2,8,AP_STREAM_COPY_CONSUME);accept(s,&r);
    struct ap_stream_copy_owned *copy=&s->pending[ap_command_slot(7)].stream_copy;
    assert(copy->count==1 && copy->unit_visible==0 && copy->begin_active);
    payload(s,2);assert(copy->count==2 && copy->unit_visible==0);
    r=finish(s,0);accept(s,&r);assert(copy->unit_visible==3 && copy->byte_frontier==2 && copy->unit_order==1);
    /* Another file consumer commits two bytes before this Call resumes. */
    r=begin(s,4,2,2,4,AP_STREAM_COPY_CONSUME);accept(s,&r);payload(s,2);r=finish(s,0);accept(s,&r);
    assert(copy->unit_cursor==4 && copy->byte_frontier==6 && copy->unit_order==3 && copy->count==6);
    commit5(s,4);
}
static void peek_and_fault(struct ap_session *s,struct ring_buffer *ring) {
    reset(s,ring);struct ap_pending_command *p=&s->pending[ap_command_slot(7)];
    p->submitted.operation=AP_ORIGINAL_RECVMSG_CALL;p->submitted.expected_option=0x42;
    struct ap_stream_copy_record r=begin(s,0,0,32,64,AP_STREAM_COPY_OBSERVE);accept(s,&r);payload(s,32);
    r=finish(s,0);accept(s,&r);assert(!p->stream_copy.byte_frontier && !p->stream_copy.unit_order);
    r=begin(s,0,0,32,32,AP_STREAM_COPY_OBSERVE);accept(s,&r);
    assert(p->stream_copy.current_begin.start==32);payload(s,32);r=finish(s,0);accept(s,&r);commit5(s,64);
    reset(s,ring);r=begin(s,0,0,64,64,AP_STREAM_COPY_CONSUME);accept(s,&r);payload(s,32);
    r=finish(s,-EFAULT);accept(s,&r);
    assert(p->stream_copy.unit_cursor==0 && p->stream_copy.copied==32 && !p->stream_copy.byte_frontier);
    commit5(s,-EFAULT);
}
static void strict_shapes_and_retained_partial(struct ap_session *s,struct ring_buffer *ring) {
    for(unsigned mutation=0;mutation<14;mutation++) {
        reset(s,ring);struct ap_stream_copy_record r=begin(s,0,0,2,4,AP_STREAM_COPY_CONSUME);
        struct ap_stream_copy_begin b;memcpy(&b,r.bytes,sizeof(b));
        switch(mutation) {
        case 0:b.file=0;break;case 1:b.start=1;break;case 2:b.offset=1;break;
        case 3:b.requested=0;break;case 4:b.requested=5;break;case 5:b.available=3;break;
        case 6:b.source_offset=5;break;case 7:b.skb_length=1ULL<<32;break;
        case 8:b.nonlinear=5;break;case 9:b.position=1ULL<<32;break;
        case 10:b.transport=3;break;case 11:b.disposition=AP_STREAM_COPY_OBSERVE;break;
        case 12:r.length--;break;case 13:r.kind=AP_STREAM_COPY_UNIT;break;
        }
        memcpy(r.bytes,&b,sizeof(b));assert(stream_copy_record(s,&r,sizeof(r))==-EPROTO);
        assert(s->pending[ap_command_slot(7)].stream_copy.count==0);
    }
    for(unsigned mutation=0;mutation<11;mutation++) {
        reset(s,ring);struct ap_stream_copy_record r=begin(s,0,0,2,4,AP_STREAM_COPY_CONSUME);accept(s,&r);payload(s,2);
        r=finish(s,0);u64 value;memcpy(&value,r.bytes+8*mutation,8);value++;
        memcpy(r.bytes+8*mutation,&value,8);assert(stream_copy_record(s,&r,sizeof(r))==-EPROTO);
        const struct ap_stream_copy_owned *copy=&s->pending[ap_command_slot(7)].stream_copy;
        assert(copy->count==2 && copy->completed_units==0 && copy->begin_active && copy->unit_visible==0);
    }
    /* A genuine legacy Unit72 is never interpreted as the V5 End88. */
    reset(s,ring);struct ap_stream_copy_record old=begin(s,0,0,2,4,AP_STREAM_COPY_CONSUME);
    accept(s,&old);payload(s,2);old=finish(s,0);
    old.kind=AP_STREAM_COPY_UNIT;old.length=sizeof(struct ap_stream_copy_unit);
    memset(old.bytes+old.length,0,sizeof(old.bytes)-old.length);
    assert(stream_copy_record(s,&old,sizeof(old))==-EPROTO);
    assert(s->pending[ap_command_slot(7)].stream_copy.count==2);
    assert(!s->pending[ap_command_slot(7)].stream_copy.completed_units);
    /* A late legacy manifest cannot renegotiate a parsed V5 prefix. */
    reset(s,ring);old=begin(s,0,0,2,4,AP_STREAM_COPY_CONSUME);accept(s,&old);
    payload(s,2);old=finish(s,0);accept(s,&old);
    const struct ap_stream_copy_summary legacy={.version=4,.initial_count=64,.attempts=1,
        .records=3,.copied=2,.final_count=62,.protocol_returned=2,.protocol_complete=1};
    old=raw(s,AP_STREAM_COPY_COMMIT,sizeof(legacy));old.attempt=1;old.offset=2;
    memcpy(old.bytes,&legacy,sizeof(legacy));
    assert(stream_copy_record(s,&old,sizeof(old))==-EPROTO);
    assert(s->pending[ap_command_slot(7)].stream_copy.count==3);
    assert(s->pending[ap_command_slot(7)].stream_copy.completed_units==1);
    reset(s,ring);struct ap_stream_copy_record r=raw(s,AP_STREAM_COPY_DATA,1);r.bytes[0]='Z';
    assert(stream_copy_record(s,&r,sizeof(r))==-EPROTO);
    reset(s,ring);r=begin(s,0,0,2,4,AP_STREAM_COPY_CONSUME);accept(s,&r);payload(s,1);
    terminal=true;struct ap_stream_copy_progress progress={0};
    assert(!ap_original_read_copy_progress(s,91,7,&progress));
    assert(progress.terminal==1 && progress.records==2 && !progress.exited && !progress.protocol);
    struct ap_stream_copy_record retained;assert(!ap_original_read_copy_record(s,7,0,&retained));
    assert(retained.kind==AP_STREAM_COPY_BEGIN);assert(!ap_original_read_copy_record(s,7,1,&retained));
    assert(retained.kind==AP_STREAM_COPY_DATA && retained.length==1);
}
/* The helper recvmsg/recvfrom exit accepts the packaged V5 summary and
 * refuses the fixed V4 literal that marked every V5 receive AP_FD_OUTCOME. */
static void recv_exit_copy_version(void) {
    struct ap_fd_call call={0};
    call.original.stream_copy.summary.version=5;assert(ap_original_recv_copy_version(&call));
    call.original.stream_copy.summary.version=4;assert(!ap_original_recv_copy_version(&call));
    call.original.stream_copy.summary.version=0;assert(!ap_original_recv_copy_version(&call));
    assert(!ap_original_recv_copy_version(0));
}
static void exact_image_fragment_address(void) {
    const u64 vmemmap=0xffffea0000000000ULL,page_offset=0xffff888000000000ULL;
    const u64 page=vmemmap+10*64;
    const u64 five_level_vmemmap=0xffd4000000000000ULL;
    const u64 five_level_page_offset=0xff11000000000000ULL;
    assert(ap_stream_fragment_image_layout(4096,64));
    assert(!ap_stream_fragment_image_layout(8192,64));
    assert(!ap_stream_fragment_image_layout(4096,56));
    assert(!ap_stream_fragment_image_layout(0,64));
    assert(!ap_stream_fragment_image_layout(4096,0));
    assert(ap_stream_fragment_source(page,0,1,vmemmap,page_offset)==page_offset+10*4096);
    assert(ap_stream_fragment_source(page,4096,1,vmemmap,page_offset)==page_offset+11*4096);
    assert(ap_stream_fragment_source(page,4097,17,vmemmap,page_offset)==page_offset+11*4096+1);
    assert(ap_stream_fragment_source(page,4095,1,vmemmap,page_offset)==page_offset+10*4096+4095);
    assert(ap_stream_fragment_source(page,4095,2,vmemmap,page_offset)==page_offset+10*4096+4095);
    assert(ap_stream_fragment_source(page,8191,4098,vmemmap,page_offset)==page_offset+11*4096+4095);
    assert(!ap_stream_fragment_source(page|1,0,1,vmemmap,page_offset));
    assert(!ap_stream_fragment_source(page,0,0,vmemmap,page_offset));
    assert(!ap_stream_fragment_source(vmemmap-64,0,1,vmemmap,page_offset));
    assert(!ap_stream_fragment_source(page+1,0,1,vmemmap,page_offset));
    assert(!ap_stream_fragment_source(~0ULL-63,4096,1,vmemmap,page_offset));
    assert(!ap_stream_fragment_source(page,0xffffffffULL,2,vmemmap,page_offset));
    assert(!ap_stream_fragment_source(page,0,1,0x1000,page_offset));
    assert(!ap_stream_fragment_source(page,0,1,vmemmap,0x1000));
    assert(!ap_stream_fragment_source(vmemmap+64,0,1,vmemmap,0xfffffffffffff000ULL));
    assert(ap_stream_fragment_source(five_level_vmemmap,0,1,five_level_vmemmap,
        five_level_page_offset)==five_level_page_offset);
    assert(!ap_stream_fragment_source(0xfeffffffffffffc0ULL,0,1,
        five_level_vmemmap,five_level_page_offset));
    assert(!ap_stream_fragment_source(five_level_vmemmap,0,1,
        0xfeffffffffffffc0ULL,five_level_page_offset));
    assert(!ap_stream_fragment_source(five_level_vmemmap,0,1,
        five_level_vmemmap,0xfefffffffffff000ULL));
}
static void reset_emission(void) {
    memset(emitted_bytes,0,sizeof(emitted_bytes));
    memset(emitted_segments,0,sizeof(emitted_segments));
    emitted_segment_count=0;
}
static void grouped_source_producer_bytes(void) {
    struct ap_fd_call call={0};
    struct sk_buff skb={0};
    const unsigned char linear[]="ABCDEFGHIJ";
    call.original.stream_copy.summary.initial_count=16;
    call.original.stream_copy.before_count=16;
    call.original.stream_copy.source=2;
    call.original.stream_copy.requested=5;
    skb.view=(struct stream_copy_skb_view){.data=(u64)linear,.size=10};
    reset_emission();
    assert(stream_copy_emit_grouped_source(&call,&skb));
    assert(emitted_segment_count==1);
    assert(emitted_segments[0].source==(u64)linear+2);
    assert(emitted_segments[0].count==5 && emitted_segments[0].offset==0);
    assert(!memcmp(emitted_bytes,"CDEFG",5));

    memcpy(direct_pages[0]+100,"ijk",3);
    memcpy(direct_pages[1]+4094,"lmno",4);
    memset(&call,0,sizeof(call));memset(&skb,0,sizeof(skb));
    call.original.stream_copy.summary.initial_count=32;
    call.original.stream_copy.before_count=32;
    call.original.stream_copy.source=6;
    call.original.stream_copy.requested=9;
    call.original.stream_copy.copy_active=AP_STREAM_COPY_TCP<<AP_STREAM_COPY_TRANSPORT_SHIFT;
    skb.shared.dataref=1;skb.shared.nr_frags=2;
    skb.shared.frags[0]=(struct test_frag){.netmem=TEST_VMEMMAP,.len=3,.offset=100};
    skb.shared.frags[1]=(struct test_frag){.netmem=TEST_VMEMMAP+64,.len=4,.offset=4094};
    skb.view=(struct stream_copy_skb_view){.head=(u64)&skb.shared,.data=(u64)linear,
        .end=0,.size=15,.nonlinear=7};
    reset_emission();
    assert(stream_copy_emit_grouped_source(&call,&skb));
    assert(emitted_segment_count==3);
    assert(emitted_segments[0].count==2 && emitted_segments[0].offset==0);
    assert(emitted_segments[1].count==3 && emitted_segments[1].offset==2);
    assert(emitted_segments[2].count==4 && emitted_segments[2].offset==5);
    assert(!memcmp(emitted_bytes,"GHijklmno",9));

    /* Fragment-only start inside the first fragment catches the historical
     * W3c overread: the first emission is the two-byte suffix, not frag len. */
    call.original.stream_copy.source=9;
    call.original.stream_copy.requested=6;
    reset_emission();
    assert(stream_copy_emit_grouped_source(&call,&skb));
    assert(emitted_segment_count==2);
    assert(emitted_segments[0].count==2 && emitted_segments[0].offset==0);
    assert(emitted_segments[1].count==4 && emitted_segments[1].offset==2);
    assert(!memcmp(emitted_bytes,"jklmno",6));

    call.original.stream_copy.source=14;
    call.original.stream_copy.requested=2;
    reset_emission();
    assert(!stream_copy_emit_grouped_source(&call,&skb));
    assert(!emitted_segment_count);
}
int main(void) {
    recv_exit_copy_version();exact_image_fragment_address();grouped_source_producer_bytes();
    assert(ap_stream_copy_source_enter(0));
    assert(ap_stream_copy_source_exit(0));
    assert(ap_stream_copy_source_enter(1));
    assert(ap_stream_copy_source_exit(1));
    assert(ap_stream_copy_source_roles(0));
    assert(ap_stream_copy_source_roles(1));
    assert(ap_stream_copy_linear_take(2,3,10)==3);
    assert(ap_stream_copy_linear_take(8,5,10)==2);
    assert(ap_stream_copy_linear_take(10,5,10)==0);
    assert(ap_stream_copy_linear_take(12,5,10)==0);
    assert(ap_stream_copy_linear_take(2,0,10)==0);
    u64 within=99,take=99;
    assert(ap_stream_copy_fragment_window(12,20,10,5,&within,&take)==1);
    assert(within==2 && take==3); /* never read 20 bytes from a 3-byte suffix */
    assert(ap_stream_copy_fragment_window(15,4,10,5,&within,&take)==-1);
    assert(ap_stream_copy_fragment_window(9,4,10,5,&within,&take)==0);
    assert(ap_stream_copy_fragment_window(10,0,10,5,&within,&take)==0);
    assert(ap_stream_copy_fragment_window(10,1,~0ULL-2,5,&within,&take)==0);
    /* Mixed/multi-fragment plan: two linear bytes, then exact 3+4 fragment
     * suffixes. The second request is deliberately larger than each fragment. */
    u64 start=8,remaining=9,linear=10,cursor=linear,total=0;
    u64 linear_take=ap_stream_copy_linear_take(start,remaining,linear);
    assert(linear_take==2);start+=linear_take;remaining-=linear_take;total+=linear_take;
    assert(ap_stream_copy_fragment_window(start,remaining,cursor,3,&within,&take)==1);
    assert(within==0 && take==3);start+=take;remaining-=take;cursor+=3;total+=take;
    assert(ap_stream_copy_fragment_window(start,remaining,cursor,4,&within,&take)==1);
    assert(within==0 && take==4);remaining-=take;total+=take;
    assert(total==9 && remaining==0);
    assert(ap_stream_copy_mixed_head_owned(2,3,15,5,1,0));
    assert(ap_stream_copy_mixed_head_owned(8,5,15,5,1,0));
    assert(!ap_stream_copy_mixed_head_owned(10,3,15,5,1,0));
    assert(!ap_stream_copy_mixed_head_owned(2,3,15,5,2,0));
    assert(!ap_stream_copy_mixed_head_owned(2,3,15,5,1,1));
    assert(!ap_stream_copy_mixed_head_owned(2,0,15,5,1,0));
    assert(!ap_stream_copy_mixed_head_owned(2,14,15,5,1,0));
    assert(!ap_stream_copy_mixed_head_owned(2,3,15,16,1,0));
    struct ap_session *s=calloc(1,sizeof(*s));assert(s);struct ring_buffer ring={0};
    active_prefix_and_interleave(s,&ring);peek_and_fault(s,&ring);strict_shapes_and_retained_partial(s,&ring);
    reset(s,&ring);free(s);
    puts("copy5 actual C collector: Begin/DATA/End, interleave, Peek, fault, 25 malformed pairs, terminal partial custody, recv exit accepts copy5 and refuses copy4, exact grouped linear/mixed/multifrag byte producer, page-layout/address refusals; no native proof");
    return 0;
}
