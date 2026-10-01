/* SPDX-License-Identifier: MIT */
/* Host boundary execution of the actual production helper entry/exit, custody,
 * source emitter, terminal-fixup issuer and C collector. No native claim. */
#define AP_NATIVE_COPY_VERSION 5
#define AP_GROUPED_PROVIDER 1
#define AP_FTRACE_PROVIDER 1
#define AP_FTRACE_RUNTIME_MUTANT 1
#include <assert.h>
#include <errno.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "fd-effects.h"
#include "stream-frontier.h"
#include "grouped-probes.h"
#define CORE(x) (x)
#define AP_STREAM_COPY_PHASE_MASK 255ULL
#define AP_STREAM_COPY_TRANSPORT_SHIFT 8
#define TEST_VMEMMAP 0xffd4000000000000ULL
#define TEST_DIRECT 0xff11000000000000ULL
struct pt_regs { u64 r15,r14,r13,r12,bp,bx,r11,r10,r9,r8,ax,cx,dx,si,di,orig_ax,ip,cs,flags,sp,ss; };
struct iov_iter { u64 type,iov_offset,ubuf,count; };
struct sock { s32 sk_peek_off; };
struct tcp_sock { struct sock sk; u16 urg_data; };
struct socket { struct sock *sk; };
struct file { struct socket *private_data; };
struct tcp_skb_cb { u32 seq; };
struct skb_shared_info {
    struct {u64 netmem;u32 offset,len;} frags[17];
    u64 frag_list;struct {s32 counter;} dataref;u8 nr_frags,flags;
};
struct sk_buff {
    struct {struct {s32 counter;} refs;} users;
    u64 head,data;u32 tail,end,len,data_len;
    u64 bits; unsigned char cb[48];
};
/* The zero host bitfield storage is the boundary equivalent of CO-RE reads;
 * all custody decisions still execute the unmodified production body. */
#define __builtin_preserve_field_info(field,which) ((which)==1?8:(which)==0?__builtin_offsetof(struct sk_buff,bits):0)
static struct ap_config config={.anchor_phase=AP_GROUPED_ANCHOR_ACTIVE,
    .anchor_ip=AP_GROUPED_CONNECT_IMAGE,.vmemmap_base=TEST_VMEMMAP,.page_offset_base=TEST_DIRECT};
static struct ap_fd_file known;
static struct file selected;
static struct ap_fd_call active;
static int fd_files,ap_config_map,stream_copy_records;
static int stream_copy_faults;
static void *test_fault_lookup(void);
static unsigned char pages[3][4096],head[4096];
static struct ap_stream_copy_record emitted[64],reserved;
static u32 emitted_count;
static u64 problems;
static bool missing_witness,ring_full,bad_extable,bad_code,read_failure;
static void *lookup(void *map,const void *key) {
    if(map==&fd_files)return *(const u64 *)key==(u64)&selected?&known:0;
    if(map==&ap_config_map)return *(const u32 *)key==0?&config:0;
    if(map==&stream_copy_faults)return *(const u32 *)key==ap_command_slot(7)?test_fault_lookup():0;
    return 0;
}
static int fd_read_kernel(void *out,u32 n,const void *source) {
    const u64 addr=(u64)source;
    if(read_failure)return -1;
    if(addr==0xffffffff82d595c0ULL) {
        const u32 ex[]={0xff25f708U,0xff25f709U,3};assert(n==sizeof(ex));memcpy(out,ex,n);
        if(bad_extable)((u8 *)out)[8]^=1;return 0;
    }
    if(addr==0xffffffff81fb8cc8ULL) {
        const u8 code[]={0xf3,0xa4,0x90,0x90,0x90,0x90,0x90,0x90};
        assert(n==sizeof(code));memcpy(out,code,n);if(bad_code)((u8 *)out)[0]^=1;return 0;
    }
    if(addr>=TEST_DIRECT && addr<TEST_DIRECT+sizeof(pages)) {
        const u64 off=addr-TEST_DIRECT;if(n>sizeof(pages)-off)return -1;
        memcpy(out,(const u8 *)pages+off,n);return 0;
    }
    if(!source)return -1;memcpy(out,source,n);return 0;
}
static void *bpf_get_kmem_cache(u64 address) {return address==(u64)head?head:0;}
static u64 fd_function_ip(struct pt_regs *ctx) {return ctx->ip;}
static void fd_problem(u64 p) {problems|=p;}
static void *stream_copy_reserve(void *map,u64 size,u64 flags) {
    assert(map==&stream_copy_records && size==sizeof(reserved) && !flags);
    return !ring_full && emitted_count<64?&reserved:0;
}
static void stream_copy_submit(void *record,u64 flags) {
    assert(record==&reserved && !flags);emitted[emitted_count++]=reserved;
}
static void stream_copy_discard(void *record,u64 flags) {assert(record==&reserved && !flags);}
static long stream_copy_loop(u32 n,long (*cb)(u32,void *),void *data,u64 flags) {
    assert(!flags);for(u32 i=0;i<n;i++)if(cb(i,data))return i+1;return n;
}
static void stream_copy_record_init(struct ap_stream_copy_record *,const struct ap_fd_call *);
#define AP_STREAM_COPY_FAULT_ENABLED 1
static int stream_copy_fault_arm(struct ap_fd_call *,struct pt_regs *);
#include "stream-copy-custody.inc"
#include "stream-copy-problem.inc"
#include "stream-copy-unit-enter.inc"
#include "stream-copy-emit.inc"
#include "stream-copy-grouped-source.inc"
#include "stream-copy-fault.inc"
static struct ap_stream_fault_state witness;
static void *test_fault_lookup(void) {return missing_witness?0:&witness;}
#include "stream-copy-unit-exit.inc"
enum ap_slot_state { AP_SLOT_FREE, AP_SLOT_RESERVED, AP_SLOT_ACTIVE, AP_SLOT_DISARMING, AP_SLOT_COLLECTED, AP_SLOT_QUARANTINED };
struct ap_pending_command {
    enum ap_slot_state state;
    struct ap_task_command submitted;
    struct ap_command_result receipt;
    bool original_collected;
    struct ap_stream_copy_owned stream_copy;
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

static struct iov_iter iter;
static struct sk_buff skb;
static struct tcp_sock tcp;
static struct socket socket_value;
static void setup(u64 requested,int fragments) {
    memset(&active,0,sizeof(active));memset(&known,0,sizeof(known));
    memset(&witness,0,sizeof(witness));
    missing_witness=ring_full=bad_extable=bad_code=read_failure=false;
    memset(&skb,0,sizeof(skb));memset(head,0,sizeof(head));
    memset(&tcp,0,sizeof(tcp));memset(&iter,0,sizeof(iter));
    emitted_count=0;problems=0;tcp.sk.sk_peek_off=-1;socket_value.sk=&tcp.sk;
    selected.private_data=&socket_value;
    known.identity=23;assert(ap_stream_frontier_enroll_install(&known,23,1,1));
    active.command=7;active.operation=AP_ORIGINAL_READ;active.selected_file=23;
    active.selection.word=(u64)&selected;
    active.security_socket=ap_stream_frame_advance(
        ap_stream_frame_advance(ap_stream_frame_begin(1,0xffff888000100000ULL),1,2),1,3);
    active.original.selection=(struct ap_original_selection){.provider=3,.command=7,
        .call=11,.task=13,.task_start=17,.original_count=requested,.file=23,.ready=1};
    active.original.stream_copy.summary.version=5;
    active.original.stream_copy.summary.initial_count=requested;
    active.original.stream_copy.iterator=(u64)&iter;
    active.original.stream_copy.copy_active=AP_STREAM_COPY_TCP<<8;
    iter.count=requested;iter.ubuf=0x100000ULL;
    skb.head=(u64)head;skb.data=(u64)head;skb.end=1024;
    skb.users.refs.counter=1;skb.len=requested;
    ((struct tcp_skb_cb *)skb.cb)->seq=42;
    struct skb_shared_info *shared=(void *)(head+skb.end);
    shared->dataref.counter=1;
    if(fragments) {
        skb.data_len=requested;shared->nr_frags=fragments;
        shared->frags[0].netmem=TEST_VMEMMAP;shared->frags[0].len=fragments==2?128:requested;
        shared->frags[0].offset=100;
        if(fragments==2) {
            shared->frags[1].netmem=TEST_VMEMMAP+64;shared->frags[1].len=requested-128;
            shared->frags[1].offset=200;
        }
    } else {skb.tail=requested;}
    for(u64 i=0;i<sizeof(pages);i++)((u8 *)pages)[i]=(u8)(i*17+31);
    for(u64 i=0;i<requested;i++)head[i]=(u8)(i*17+31);
}
static void helper_enter(u64 requested) {
    const u32 before=emitted_count;
    u64 caller=0xffffffff81fb7523ULL;
    struct pt_regs ctx={.di=(u64)&skb,.dx=(u64)&iter,.cx=requested,
        .sp=(u64)&caller,.ip=0xffffffff81fb85d0ULL};
    stream_copy_unit_enter(&active,&ctx);
    assert(!problems && emitted_count==before+1 && emitted[before].kind==AP_STREAM_COPY_BEGIN);
}
static void check_records(u64 observed,s64 returned,u64 committed) {
    struct ap_session s={.incarnation=3,.ready=true};
    struct ap_pending_command *pending=&s.pending[ap_command_slot(7)];
    pending->state=AP_SLOT_ACTIVE;
    pending->submitted=(struct ap_task_command){.command=7,.operation=AP_ORIGINAL_READ,
        .expected_object=11,.original_count=active.original.stream_copy.summary.initial_count};
    for(u32 i=0;i<emitted_count;i++)assert(!stream_copy_record(&s,&emitted[i],sizeof(emitted[i])));
    struct ap_stream_copy_owned *owned=&pending->stream_copy;
    assert(!s.stream_copy_error);
    assert(owned->copied==observed);
    assert(owned->unit_cursor==committed && owned->byte_frontier==committed);
    assert(known.stream_bytes==committed && known.stream_units==(committed?1:0));
    const struct ap_stream_copy_record *last=&emitted[emitted_count-1];
    assert(last->kind==AP_STREAM_COPY_END);
    struct ap_stream_copy_end end;memcpy(&end,last->bytes,sizeof(end));
    assert(end.unit.requested==512 && end.unit.copied==observed && end.unit.returned==returned);
    u64 byte=0;
    for(u32 i=0;i<emitted_count;i++)if(emitted[i].kind==AP_STREAM_COPY_DATA) {
        for(u32 j=0;j<emitted[i].length;j++,byte++)
            assert(emitted[i].bytes[j]==pages[0][100+byte]);
    }
    assert(byte==observed);free(owned->records);
}
static struct pt_regs terminal_frame(u64 source,u64 length,u64 residual) {
    const u64 within=length-residual;
    return (struct pt_regs){.bx=length,.cx=residual,.dx=(u64)&iter,.r14=source,
        .si=source+within,.di=iter.ubuf+iter.iov_offset+within,
        .ip=0xffffffff81fb8cc8ULL,.cs=0x10,.flags=0x202};
}
static void terminal_pair(struct pt_regs *frame) {
    u64 args[]={ (u64)frame,14,2,frame->di,1 };
    assert(stream_copy_fault_enter(&active,args));
    frame->ip=0xffffffff81fb8ccdULL;
    assert(stream_copy_fault_exit(&active,args));
}
static void helper_exit(s64 returned) {
    struct pt_regs ctx={.ax=(u64)returned};stream_copy_unit_exit(&active,&ctx);
}
static void simple(bool full,u64 prefix,int fragments) {
    setup(512,fragments);helper_enter(512);
    if(!full) {
        struct pt_regs frame=terminal_frame(fragments?TEST_DIRECT+100:(u64)head,512,512-prefix);
        terminal_pair(&frame);
    } else {iter.count=0;iter.iov_offset=512;}
    helper_exit(full?0:-14);assert(!problems);
    if(fragments)check_records(full?512:prefix,full?0:-14,full?512:0);
    else {
        /* Independent linear fixture uses the same exact collector; bytes
         * are intentionally different from the page-backed source. */
        for(u32 i=0;i<emitted_count;i++)if(emitted[i].kind==AP_STREAM_COPY_DATA)
            assert(!memcmp(emitted[i].bytes,head+emitted[i].offset,emitted[i].length));
        /* Normalize only the fixture expectation, never emitted records. */
        memcpy(pages[0]+100,head,512);
        check_records(full?512:prefix,full?0:-14,full?512:0);
    }
}
static void extended(bool prior_success) {
    setup(512,prior_success?1:2);
    if(prior_success) {
        active.original.stream_copy.summary.initial_count=1024;
        active.original.selection.original_count=1024;iter.count=1024;
        helper_enter(512);iter.count=512;iter.iov_offset=512;helper_exit(0);assert(!problems);
        ((struct tcp_skb_cb *)skb.cb)->seq=554;
    }
    helper_enter(512);
    if(!prior_success) {iter.count=384;iter.iov_offset=128;}
    struct pt_regs frame=terminal_frame(prior_success?TEST_DIRECT+100:TEST_DIRECT+4096+200,
        prior_success?512:384,prior_success?449:321);
    terminal_pair(&frame);
    /* Actual datagram failure reverts both this REP and all earlier inner
     * copies. It does not undo the physical stores or a prior successful unit. */
    iter.count=512;iter.iov_offset=prior_success?512:0;
    helper_exit(-14);assert(!problems);
    const u64 observed=prior_success?575:191,committed=prior_success?512:0;
    const struct ap_stream_copy_record *last=&emitted[emitted_count-1];
    struct ap_stream_copy_end end;memcpy(&end,last->bytes,sizeof(end));
    assert(last->kind==AP_STREAM_COPY_END && end.unit.requested==512);
    assert(end.unit.copied==(prior_success?63:191) && end.unit.returned==-14);
    assert(end.before==committed && end.after==committed && end.unit.order==(prior_success?1:0));
    unsigned char expected[575];
    if(prior_success) {memcpy(expected,pages[0]+100,512);memcpy(expected+512,pages[0]+100,63);}
    else {memcpy(expected,pages[0]+100,128);memcpy(expected+128,pages[1]+200,63);}
    u64 at=0;
    for(u32 i=0;i<emitted_count;i++)if(emitted[i].kind==AP_STREAM_COPY_DATA) {
        assert(at+emitted[i].length<=observed);
        assert(!memcmp(emitted[i].bytes,expected+at,emitted[i].length));at+=emitted[i].length;
    }
    assert(at==observed && known.stream_bytes==committed && known.stream_units==(prior_success?1:0));
    /* Real same-Call COMMIT body, after the protocol/sys_exit boundary inputs.
     * Failure wins only if there was no earlier successful unit. */
    const s64 returned=prior_success?512:-14;
    active.original.stream_copy.summary.final_count=512;
    active.original.stream_copy.summary.protocol_returned=(u64)returned;
    active.original.stream_copy.summary.protocol_complete=1;
    active.original.stream_copy.iterator=0;active.original.stream_copy.copy_active=0;
    stream_copy_commit(&active,returned);assert(!problems);
    struct ap_session s={.incarnation=3,.ready=true};
    struct ap_pending_command *pending=&s.pending[ap_command_slot(7)];
    pending->state=AP_SLOT_ACTIVE;
    pending->submitted=(struct ap_task_command){.command=7,.operation=AP_ORIGINAL_READ,
        .expected_object=11,.original_count=prior_success?1024:512};
    for(u32 i=0;i<emitted_count;i++)assert(!stream_copy_record(&s,&emitted[i],sizeof(emitted[i])));
    assert(!s.stream_copy_error && pending->stream_copy.committed);
    assert(pending->stream_copy.copied==observed && pending->stream_copy.unit_cursor==committed);
    assert(pending->stream_copy.byte_frontier==committed);
    assert(pending->stream_copy.completed_units==(prior_success?2:1));
    free(pending->stream_copy.records);
}
static void refusals(void) {
    for(unsigned test=0;test<25;test++) {
        setup(512,1);helper_enter(512);
        struct pt_regs frame=terminal_frame(TEST_DIRECT+100,512,449);
        u64 args[]={ (u64)&frame,14,2,frame.di,1 };
        switch(test) {
        case 0:missing_witness=true;break;
        case 1:witness.phase=0;break;
        case 2:witness.command++;break;
        case 3:witness.call++;break;
        case 4:witness.task++;break;
        case 5:witness.start++;break;
        case 6:witness.attempt++;break;
        case 7:witness.iterator++;break;
        case 8:witness.file++;break;
        case 9:witness.frame++;break;
        case 10:frame.cx++;break; /* DI/SI no longer match this residual. */
        case 11:frame.r14++;frame.si++;break; /* Internally coherent wrong source. */
        case 12:frame.bx++;frame.cx++;break; /* Coherent prefix, wrong native window. */
        case 13:frame.ip++;break;
        case 14:args[1]=13;break;
        case 15:args[2]=0;break;
        case 16:bad_extable=true;break;
        case 17:bad_code=true;break;
        case 18:iter.iov_offset++;break;
        case 19:frame.flags|=0x400;break;
        case 20:read_failure=true;break;
        case 21:frame.cx=0;break; /* Not a terminal failed REP. */
        case 22:iter.type=1;break;
        case 23:args[3]++;break;
        case 24:witness.pointer+=4;break;
        }
        assert(!stream_copy_fault_enter(&active,args));
        assert(problems && active.original.problem && (known.stream_state&AP_STREAM_FRONTIER_POISON));
        assert(emitted_count==1 && !known.stream_bytes);
    }
    for(unsigned test=0;test<6;test++) {
        setup(512,1);helper_enter(512);
        struct pt_regs frame=terminal_frame(TEST_DIRECT+100,512,449);
        u64 args[]={ (u64)&frame,14,2,frame.di,1 };
        assert(stream_copy_fault_enter(&active,args));
        if(test==0)assert(!stream_copy_fault_enter(&active,args)); /* Nested pair. */
        else if(test==1) {helper_exit(-14);assert(problems);} /* Missing fexit. */
        else {
            frame.ip=0xffffffff81fb8ccdULL;
            if(test==2)args[4]=0;
            if(test==3)frame.cx++;
            if(test==4)args[0]+=8;
            if(test==5)frame.ip++;
            assert(!stream_copy_fault_exit(&active,args));
        }
        assert(problems && emitted_count==1 && !known.stream_bytes);
    }
    setup(512,1);helper_enter(512);helper_exit(-14);assert(problems && emitted_count==1);
    setup(512,1);helper_enter(512);
    struct pt_regs frame=terminal_frame(TEST_DIRECT+100,512,449);terminal_pair(&frame);
    ring_full=true;helper_exit(-14);
    assert(problems&AP_FD_CAPACITY);assert(!known.stream_bytes && emitted_count==1);
    /* Same command cannot re-arm while its first helper remains active. */
    setup(512,1);helper_enter(512);
    u64 caller=0xffffffff81fb7523ULL;
    struct pt_regs ctx={.di=(u64)&skb,.dx=(u64)&iter,.cx=512,.sp=(u64)&caller,.ip=0xffffffff81fb85d0ULL};
    stream_copy_unit_enter(&active,&ctx);assert(problems && emitted_count==1);
}
int main(int argc,char **argv) {
    assert(argc<=2);
    if(argc==2 && !strcmp(argv[1],"full")) {simple(true,0,1);puts("actual producer+collector full512 passed");return 0;}
    if(argc==2 && !strcmp(argv[1],"fault")) {simple(false,63,1);puts("actual producer+collector failed-prefix63 passed");return 0;}
    if(argc==2 && !strcmp(argv[1],"two-fragment")) {extended(false);puts("actual producer+collector prior-inner128 plus failed63 passed");return 0;}
    if(argc==2 && !strcmp(argv[1],"linear")) {simple(false,63,0);puts("actual linear producer+collector failed-prefix63 passed");return 0;}
    assert(argc==1 || !strcmp(argv[1],"all"));
    simple(true,0,1);
    const u64 prefixes[]={0,1,7,63,255,511};
    for(unsigned i=0;i<sizeof(prefixes)/sizeof(prefixes[0]);i++)simple(false,prefixes[i],1);
    simple(true,0,0);simple(false,63,0);extended(false);extended(true);refusals();
    puts("actual producer+collector: full, zero, prefixes, two-fragment B-Q, prior-success, stale/nested/missing refusals passed; native UNRUN");
    return 0;
}
